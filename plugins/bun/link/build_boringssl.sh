#!/bin/bash
N="$BUN_NATIVE"; WASI_SDK="$WASI_SDK"; S="$WORK"
CC="$WASI_SDK/bin/clang++"; AR="$WASI_SDK/bin/llvm-ar"
mkdir -p /tmp/vlib/bssl
FLAGS="-O2 -fno-exceptions -fno-rtti -DOPENSSL_NO_ASM -I$N/boringssl/include -I$N/boringssl -I$N/boringssl/crypto/fipsmodule"
i=0; objs=(); fails=0
while read f; do
  [ -z "$f" ] && continue
  o="/tmp/vlib/bssl/$i.o"
  if $CC --target=wasm32-wasip2 $FLAGS -c "$N/boringssl/$f" -o "$o" 2>"/tmp/vlib/bssl/$i.err"; then objs+=("$o"); else fails=$((fails+1)); echo "FAIL $f: $(grep -m1 -oE 'error: .{0,50}' /tmp/vlib/bssl/$i.err|head -1)"; fi
  i=$((i+1))
done < "$S/boringssl_srcs.txt"
$AR rcs /tmp/vlib/libbssl_crypto.a "${objs[@]}"
echo "=== boringssl: ${#objs[@]} objs, $fails fails, $(ls -la /tmp/vlib/libbssl_crypto.a|awk '{print $5}') bytes"
