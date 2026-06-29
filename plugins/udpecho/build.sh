#!/usr/bin/env bash
# Build the UDP echo demo (standard BSD sockets, no wk-specific code) into a
# wasi:cli command targeting wasm32-wasip2 (sockets need p2). Runs on wk's
# userspace network fabric.
set -euo pipefail
cd "$(dirname "$0")"
WASI_SDK="${WASI_SDK:-$HOME/wasi-sdk}"
CLANG_PATH="$WASI_SDK/bin:/usr/bin:/bin"
env PATH="$CLANG_PATH" "$WASI_SDK/bin/clang" --target=wasm32-wasip2 -O2 \
    echo.c -o udpecho.wasm
echo "built plugins/udpecho/udpecho.wasm"
