#!/bin/bash
# BoringSSL libcrypto for wasm32-wasip2 → $VLIB/libbssl_crypto.a.
# (build_bssl_ssl.sh appends the ssl/pki/decrepit objects to the same
# archive afterwards — run them in that order.)
#
# PATHS RE-ROOTED (2026-08): originally the source list lived in a session
# scratchpad and objects went to /tmp/vlib; both now live under
# native/runtime-build/. Compile flags are byte-identical to the original.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
BUN_PLUGIN="${BUN_PLUGIN:-$(cd "$HERE/.." && pwd)}"
N="${BUN_NATIVE:-$BUN_PLUGIN/native}"
WORK="${WORK:-$N/runtime-build}"
VLIB="${VLIB:-$WORK/vlib}"
WASI_SDK="${WASI_SDK:?set WASI_SDK (wasi-sdk-34-rc.2)}"
CC="$WASI_SDK/bin/clang++"; AR="$WASI_SDK/bin/llvm-ar"
mkdir -p "$VLIB/bssl" "$WORK/lists"

# The compile list comes from BoringSSL's own build manifest. NOTE the list
# is written WITHOUT a trailing newline on purpose: the while-read loop then
# skips the last entry (bcm.cc), which needs its own explicit compile anyway
# (the fipsmodule umbrella TU) — see the punchlist gotcha.
if [ ! -f "$WORK/lists/boringssl_srcs.txt" ]; then
    python3 - "$N/boringssl/gen/sources.json" "$WORK/lists/boringssl_srcs.txt" <<'EOF'
import json, sys
d = json.load(open(sys.argv[1]))
srcs = []
for k in ['crypto', 'bcm']:
    srcs += [s for s in d[k].get('srcs', []) if s.endswith(('.c', '.cc'))]
open(sys.argv[2], 'w').write('\n'.join(srcs))
print(len(srcs), 'boringssl crypto sources')
EOF
fi

FLAGS="-O2 -fno-exceptions -fno-rtti -DOPENSSL_NO_ASM -I$N/boringssl/include -I$N/boringssl -I$N/boringssl/crypto/fipsmodule"
i=0; objs=(); fails=0
while read f; do
  [ -z "$f" ] && continue
  o="$VLIB/bssl/$i.o"
  if $CC --target=wasm32-wasip2 $FLAGS -c "$N/boringssl/$f" -o "$o" 2>"$VLIB/bssl/$i.err"; then objs+=("$o"); else fails=$((fails+1)); echo "FAIL $f: $(grep -m1 -oE 'error: .{0,50}' $VLIB/bssl/$i.err|head -1)"; fi
  i=$((i+1))
done < "$WORK/lists/boringssl_srcs.txt"
$AR rcs "$VLIB/libbssl_crypto.a" "${objs[@]}"
# bcm.cc explicitly (the while-read above skipped it — no trailing newline).
$CC --target=wasm32-wasip2 $FLAGS -c "$N/boringssl/crypto/fipsmodule/bcm.cc" -o "$VLIB/bssl/bcm.o"
$AR rcs "$VLIB/libbssl_crypto.a" "$VLIB/bssl/bcm.o"
echo "=== boringssl: ${#objs[@]}+bcm objs, $fails fails, $(ls -la "$VLIB/libbssl_crypto.a"|awk '{print $5}') bytes"
