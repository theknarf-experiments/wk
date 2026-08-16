#!/bin/bash
# Build ls-hpack (HTTP/2 HPACK) for wasm32-wasip2 → /tmp/{lshpack,xxhash}.o,
# picked up by link_all.sh so node:http2 gets a REAL HPACK codec (the old
# link/wasi-stubs.c placeholders were non-functional and trapped).
#
# Source is the ls-hpack checkout under ../native/lshpack (gitignored, like
# native/mimalloc); cloned at the commit bun pins (scripts/build/deps/lshpack.ts).
# Notes matching bun's own build:
#   * wasi-libc ships no <sys/queue.h> (a glibc/BSD-ism) — use lshpack's vendored
#     compat/queue copy.
#   * xxhash (bundled at deps/xxhash) is namespaced XXH_NAMESPACE=lshpack_ so its
#     XXH32/XXH64 can't collide with any other xxhash in the final link.
#   * LS_HPACK_USE_LARGE_TABLES=1 keeps the fast decode tables; the bss-huff-
#     tables optimization (moving 768 KB from .rodata to .bss) is skipped — on
#     wasm the extra .rodata is negligible against the ~181 MB module.
set -euo pipefail
cd "$(dirname "$0")"

WASI_SDK="${WASI_SDK:-$HOME/wasi-sdk}"
CC="$WASI_SDK/bin/clang"
LS="$(cd ../native && pwd)/lshpack"
LSHPACK_COMMIT=8905c024b6d052f083a3d11d0a169b3c2735c8a1

if [ ! -f "$LS/lshpack.c" ]; then
    git clone https://github.com/litespeedtech/ls-hpack "$LS"
    git -C "$LS" checkout "$LSHPACK_COMMIT"
    git -C "$LS" submodule update --init --recursive   # deps/xxhash
fi

"$CC" --target=wasm32-wasip2 -O2 -DXXH_NAMESPACE=lshpack_ \
    -I "$LS/deps/xxhash" -c "$LS/deps/xxhash/xxhash.c" -o /tmp/xxhash.o
"$CC" --target=wasm32-wasip2 -O2 \
    -DXXH_HEADER_NAME='"xxhash.h"' -DXXH_NAMESPACE=lshpack_ -DLS_HPACK_USE_LARGE_TABLES=1 \
    -I "$LS" -I "$LS/deps/xxhash" -I "$LS/compat/queue" \
    -c "$LS/lshpack.c" -o /tmp/lshpack.o

echo "built /tmp/{lshpack,xxhash}.o"
