#!/bin/bash
# BoringSSL ssl/pki/decrepit for wasm32-wasip2, APPENDED to
# $VLIB/libbssl_crypto.a (run build_boringssl.sh first).
#
# PATHS RE-ROOTED (2026-08): was absolute-path snapshots (/tmp/vlib + a
# session scratchpad + /Users/... checkout). Flags byte-identical.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
BUN_PLUGIN="${BUN_PLUGIN:-$(cd "$HERE/.." && pwd)}"
N="${BUN_NATIVE:-$BUN_PLUGIN/native}"
WORK="${WORK:-$N/runtime-build}"
VLIB="${VLIB:-$WORK/vlib}"
WASI_SDK="${WASI_SDK:?set WASI_SDK (wasi-sdk-34-rc.2)}"
CC="$WASI_SDK/bin/clang++"; AR="$WASI_SDK/bin/llvm-ar"
mkdir -p "$VLIB/bssl" "$WORK/lists"

# List from BoringSSL's manifest (this one WITH the trailing newline — every
# entry, unlike the crypto list, is consumed by the loop).
if [ ! -f "$WORK/lists/bssl_ssl_srcs.txt" ]; then
    python3 - "$N/boringssl/gen/sources.json" "$WORK/lists/bssl_ssl_srcs.txt" <<'EOF'
import json, sys
d = json.load(open(sys.argv[1]))
srcs = []
for k in ['ssl', 'pki', 'decrepit']:
    srcs += [s for s in d[k].get('srcs', []) if s.endswith(('.c', '.cc'))]
open(sys.argv[2], 'w').write('\n'.join(srcs) + '\n')
print(len(srcs), 'ssl/pki/decrepit sources')
EOF
fi

FLAGS="-O2 -fno-exceptions -fno-rtti -DOPENSSL_NO_ASM -I$N/boringssl/include -I$N/boringssl -I$N/boringssl/crypto/fipsmodule"
i=0; objs=(); fails=0
while read f; do
  [ -z "$f" ] && continue
  o="$VLIB/bssl/ssl_$i.o"
  if $CC --target=wasm32-wasip2 $FLAGS -c "$N/boringssl/$f" -o "$o" 2>"$VLIB/bssl/ssl_$i.err"; then objs+=("$o"); else fails=$((fails+1)); echo "FAIL $f: $(grep -m1 -oE 'error: .{0,45}' $VLIB/bssl/ssl_$i.err|head -1)"; fi
  i=$((i+1))
done < "$WORK/lists/bssl_ssl_srcs.txt"
$AR rcs "$VLIB/libbssl_crypto.a" "${objs[@]}"
echo "=== ssl/pki/decrepit: ${#objs[@]} objs, $fails fails"
