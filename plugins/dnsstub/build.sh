#!/usr/bin/env bash
# Build the DNS stub server (standard BSD sockets, no wk-specific code) into a
# wasi:cli command targeting wasm32-wasip2 (sockets need p2). Wire it onto a
# Network and it is an authoritative nameserver for `wk.test`; see dnsstub.c
# for why a hermetic one is needed.
set -euo pipefail
cd "$(dirname "$0")"
WASI_SDK="${WASI_SDK:-$HOME/wasi-sdk}"
CLANG_PATH="$WASI_SDK/bin:/usr/bin:/bin"
env PATH="$CLANG_PATH" "$WASI_SDK/bin/clang" --target=wasm32-wasip2 -O2 \
    -Wall -Wextra dnsstub.c -o dnsstub.wasm
echo "built plugins/dnsstub/dnsstub.wasm"
