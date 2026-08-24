#!/usr/bin/env bash
# Boot wk's runtime inside the iOS simulator and report what worked.
#
# `simctl spawn` runs an iOS binary without an app bundle or a signing
# identity, which is enough to answer whether the engine, the fabric and a real
# guest work on the platform — the questions that would sink the port. It is
# not an app: no UIKit, no jetsam limits, and the simulator does not enforce
# the JIT ban, so a run here proves the code path, not the constraint. Only
# hardware proves that.
set -euo pipefail

GUEST="${GUEST:-plugins/hellofs/target/wasm32-wasip1/debug/hellofs.wasm}"
TARGET="aarch64-apple-ios-sim"

command -v xcrun >/dev/null || { echo "no xcrun — see: mise run ios-check"; exit 1; }

CLANG="$(xcrun -f clang)"
key="${TARGET//-/_}"
export "CC_${key}=$CLANG"
export "CXX_${key}=$(xcrun -f clang++)"
export "AR_${key}=$(xcrun -f ar)"

echo "building wk-ios for $TARGET"
cargo build --target "$TARGET" --features pulley -p wk-ios --bin wk-ios
BIN="target/$TARGET/debug/wk-ios"
[ -x "$BIN" ] || { echo "no binary at $BIN"; exit 1; }

# Reuse a booted simulator if there is one; otherwise boot the first available.
udid="$(xcrun simctl list devices booted -j 2>/dev/null |
        /usr/bin/python3 -c 'import json,sys
d=json.load(sys.stdin)["devices"]
print(next((x["udid"] for v in d.values() for x in v), ""))' || true)"
if [ -z "$udid" ]; then
  udid="$(xcrun simctl list devices available -j |
          /usr/bin/python3 -c 'import json,sys
d=json.load(sys.stdin)["devices"]
cands=[x for k,v in d.items() if "iOS" in k for x in v]
print(cands[0]["udid"] if cands else "")')"
  [ -n "$udid" ] || { echo "no available iOS simulator — see: mise run ios-check"; exit 1; }
  echo "booting simulator $udid"
  xcrun simctl boot "$udid"
  # `bootstatus -b` waits for the device to finish booting.
  xcrun simctl bootstatus "$udid" -b >/dev/null 2>&1 || true
fi

name="$(xcrun simctl list devices -j |
        /usr/bin/python3 -c "import json,sys
d=json.load(sys.stdin)['devices']
print(next((x['name'] for v in d.values() for x in v if x['udid']=='$udid'),'?'))")"
echo "running on $name ($udid)"
echo

# The simulator shares the host filesystem, so the guest can stay where it is.
guest_abs=""
[ -f "$GUEST" ] && guest_abs="$(cd "$(dirname "$GUEST")" && pwd)/$(basename "$GUEST")"
[ -n "$guest_abs" ] || echo "note: $GUEST not built — the guest check will be skipped"

xcrun simctl spawn "$udid" "$(pwd)/$BIN" ${guest_abs:+"$guest_abs"}
