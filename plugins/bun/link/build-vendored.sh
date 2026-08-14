#!/bin/bash
N="$BUN_NATIVE"; WASI_SDK="$WASI_SDK"; CC="$WASI_SDK/bin/clang"; AR="$WASI_SDK/bin/llvm-ar"
mkdir -p /tmp/vlib
build_lib() {
  local name="$1"; shift
  local out="/tmp/vlib/lib$name.a"; rm -f "$out"
  local objs=(); local i=0
  for c in "$@"; do
    local o="/tmp/vlib/${name}_$i.o"
    if $CC --target=wasm32-wasip2 -O2 -fno-exceptions $CFLAGS -c "$c" -o "$o" 2>/tmp/vlib/${name}_$i.err; then objs+=("$o"); else echo "  FAIL $(basename $c): $(grep -m1 -oE 'error: .{0,40}|fatal error: .{0,35}' /tmp/vlib/${name}_$i.err|head -1)"; fi
    i=$((i+1))
  done
  $AR rcs "$out" "${objs[@]}" 2>/dev/null && echo "== $name.a: ${#objs[@]} objs, $(ls -la $out|awk '{print $5}') bytes"
}
# zstd (disable asm)
CFLAGS="-DZSTD_DISABLE_ASM -I$N/zstd/lib -I$N/zstd/lib/common" build_lib zstd $(find $N/zstd/lib/common $N/zstd/lib/compress $N/zstd/lib/decompress -name '*.c')
# brotli
CFLAGS="-I$N/brotli/c/include" build_lib brotli $(find $N/brotli/c/common $N/brotli/c/dec $N/brotli/c/enc -name '*.c')
echo "DONE vendored"
