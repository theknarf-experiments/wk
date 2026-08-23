#!/usr/bin/env bash
# Report what is still missing before wk can be built for, and run on, iOS —
# and print the exact command to fix each gap. Read-only: the fixes that need
# `sudo` or a multi-gigabyte download are left to a human to run deliberately.
set -uo pipefail

XCODE_APP="${XCODE_APP:-/Applications/Xcode.app}"
DEV_DIR="$XCODE_APP/Contents/Developer"
missing=0

ok()   { printf '  \033[32mok\033[0m    %s\n' "$1"; }
todo() { printf '  \033[33mtodo\033[0m  %s\n     -> %s\n' "$1" "$2"; missing=$((missing + 1)); }

echo "iOS readiness"
echo

# --- toolchain ---------------------------------------------------------------
if [ -d "$XCODE_APP" ]; then
  ok "Xcode present ($XCODE_APP)"
else
  todo "Xcode is not installed" "install Xcode from the App Store"
  echo
  echo "$missing item(s) to resolve."
  exit 1
fi

active="$(xcode-select -p 2>/dev/null)"
if [ "$active" = "$DEV_DIR" ]; then
  ok "xcode-select points at Xcode"
else
  todo "xcode-select points at ${active:-nothing}, not Xcode" \
       "sudo xcode-select -s $DEV_DIR"
fi

# simctl is the first thing an unaccepted license blocks, so probe with it.
if DEVELOPER_DIR="$DEV_DIR" xcrun simctl help >/dev/null 2>&1; then
  ok "Xcode license accepted"
  license_ok=1
else
  todo "Xcode license not accepted" "sudo xcodebuild -license accept"
  license_ok=0
fi

# --- simulator runtime -------------------------------------------------------
# The SDK (compile against) and the runtime (boot on) ship separately since
# Xcode 15, so having one says nothing about the other.
if ls "$DEV_DIR/Platforms/iPhoneSimulator.platform/Developer/SDKs/" >/dev/null 2>&1; then
  ok "iPhoneSimulator SDK present (enough to compile)"
else
  todo "no iPhoneSimulator SDK" "xcodebuild -downloadPlatform iOS"
fi

if [ "$license_ok" = 1 ]; then
  runtimes="$(DEVELOPER_DIR="$DEV_DIR" xcrun simctl list runtimes 2>/dev/null | grep -ci '^iOS' || true)"
  if [ "${runtimes:-0}" -gt 0 ]; then
    ok "iOS simulator runtime installed ($runtimes)"
  else
    todo "no iOS simulator runtime (nothing to boot)" "xcodebuild -downloadPlatform iOS   # ~7GB"
  fi
else
  todo "simulator runtimes unknown (blocked by the license)" "accept the license first, then re-run"
fi

# --- rust targets ------------------------------------------------------------
installed="$(rustup target list --installed 2>/dev/null)"
for t in aarch64-apple-ios-sim aarch64-apple-ios; do
  if grep -qx "$t" <<<"$installed"; then
    ok "rust target $t"
  else
    todo "rust target $t missing" "rustup target add $t"
  fi
done

echo
if [ "$missing" -eq 0 ]; then
  echo "Ready: mise run ios-build"
else
  echo "$missing item(s) to resolve."
fi

# --- what the simulator cannot tell you --------------------------------------
cat <<'NOTE'

Note: the simulator runs native code and does not enforce the JIT ban, so it
cannot validate the Pulley path — a Cranelift build runs there happily. It is
for the app shell, windowing and touch input. The no-JIT and memory-pressure
questions need a real device.
NOTE

exit $([ "$missing" -eq 0 ] && echo 0 || echo 1)
