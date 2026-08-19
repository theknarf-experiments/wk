#!/bin/bash
# zstd + brotli for wasm32-wasip2 → $VLIB/lib{zstd,brotli}.a.
# (The rest of the vendored C libraries are in build_vendored_extra.sh.)
#
# PATHS RE-ROOTED (2026-08): objects went to /tmp/vlib; now $VLIB.
# Flags byte-identical.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
BUN_PLUGIN="${BUN_PLUGIN:-$(cd "$HERE/.." && pwd)}"
N="${BUN_NATIVE:-$BUN_PLUGIN/native}"
WORK="${WORK:-$N/runtime-build}"
VLIB="${VLIB:-$WORK/vlib}"
WASI_SDK="${WASI_SDK:?set WASI_SDK (wasi-sdk-34-rc.2)}"
CC="$WASI_SDK/bin/clang"; AR="$WASI_SDK/bin/llvm-ar"
mkdir -p "$VLIB"
build_lib() {
  local name="$1"; shift
  local out="$VLIB/lib$name.a"; rm -f "$out"
  mkdir -p "$VLIB/$name.d"
  local objs=(); local i=0
  for c in "$@"; do
    local o="$VLIB/$name.d/${name}_$i.o"
    if $CC --target=wasm32-wasip2 -O2 -fno-exceptions $CFLAGS -c "$c" -o "$o" 2>"$VLIB/$name.d/${name}_$i.err"; then objs+=("$o"); else echo "  FAIL $(basename $c): $(grep -m1 -oE 'error: .{0,40}|fatal error: .{0,35}' $VLIB/$name.d/${name}_$i.err|head -1)"; fi
    i=$((i+1))
  done
  $AR rcs "$out" "${objs[@]}" 2>/dev/null && echo "== $name.a: ${#objs[@]} objs, $(ls -la $out|awk '{print $5}') bytes"
}
# zstd (disable asm)
CFLAGS="-DZSTD_DISABLE_ASM -I$N/zstd/lib -I$N/zstd/lib/common" build_lib zstd $(find $N/zstd/lib/common $N/zstd/lib/compress $N/zstd/lib/decompress -name '*.c')
# brotli
CFLAGS="-I$N/brotli/c/include" build_lib brotli $(find $N/brotli/c/common $N/brotli/c/dec $N/brotli/c/enc -name '*.c')
echo "DONE vendored"
