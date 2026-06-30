#!/usr/bin/env bash
# Build the IPv6 echo demo (standard AF_INET6 BSD sockets, no wk-specific code)
# into a wasi:cli command targeting wasm32-wasip2 (sockets need p2). Runs on wk's
# userspace network fabric.
set -euo pipefail
cd "$(dirname "$0")"
WASI_SDK="${WASI_SDK:-$HOME/wasi-sdk}"
CLANG_PATH="$WASI_SDK/bin:/usr/bin:/bin"
env PATH="$CLANG_PATH" "$WASI_SDK/bin/clang" --target=wasm32-wasip2 -O2 \
    echo.c -o echo6.wasm
echo "built plugins/echo6/echo6.wasm"
