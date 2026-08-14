#!/bin/bash
N="/Users/knarf/projects/theknarf-experiments/wk/plugins/bun/native"; WASI_SDK="/Users/knarf/.local/share/mise/installs/github-web-assembly-wasi-sdk/wasi-sdk-34-rc.2"; S="/private/tmp/claude-501/-Users-knarf-projects-theknarf-experiments-wk/30610b6d-8ee0-4b7b-901e-7d0641cd3850/scratchpad"
CC="$WASI_SDK/bin/clang++"; AR="$WASI_SDK/bin/llvm-ar"
FLAGS="-O2 -fno-exceptions -fno-rtti -DOPENSSL_NO_ASM -I$N/boringssl/include -I$N/boringssl -I$N/boringssl/crypto/fipsmodule"
i=0; objs=(); fails=0
while read f; do
  [ -z "$f" ] && continue
  o="/tmp/vlib/bssl/ssl_$i.o"
  if $CC --target=wasm32-wasip2 $FLAGS -c "$N/boringssl/$f" -o "$o" 2>"/tmp/vlib/bssl/ssl_$i.err"; then objs+=("$o"); else fails=$((fails+1)); echo "FAIL $f: $(grep -m1 -oE 'error: .{0,45}' /tmp/vlib/bssl/ssl_$i.err|head -1)"; fi
  i=$((i+1))
done < "$S/bssl_ssl_srcs.txt"
$AR rcs /tmp/vlib/libbssl_crypto.a "${objs[@]}"
echo "=== ssl/pki/decrepit: ${#objs[@]} objs, $fails fails"
