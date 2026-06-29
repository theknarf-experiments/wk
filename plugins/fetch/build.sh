#!/usr/bin/env bash
# Build a tiny networked C program — standard BSD sockets (getaddrinfo / socket /
# connect / send / recv), no wk-specific code — into a wasi:cli command that wk
# runs in a terminal node. It connects to a host:port (argv, default
# example.com:80), sends an HTTP/1.0 GET, and prints the response. Proves a
# recompiled *networked* program runs on wk over wasi:sockets.
#
# Targets wasm32-wasip2 DIRECTLY (which emits a component — no adapter needed):
# outbound TCP needs the p2 sockets interface, which wasi-libc maps the BSD
# socket calls onto. The host links wasi:sockets and grants network access.
#
# Requires wasi-sdk (WASI_SDK, default ~/wasi-sdk).
set -euo pipefail
cd "$(dirname "$0")"

WASI_SDK="${WASI_SDK:-$HOME/wasi-sdk}"
# Run clang with a PATH that omits any old wasm-opt (kept consistent with the
# other plugins; the optional post-link wasm-opt is simply skipped).
CLANG_PATH="$WASI_SDK/bin:/usr/bin:/bin"

env PATH="$CLANG_PATH" "$WASI_SDK/bin/clang" --target=wasm32-wasip2 -O2 \
    fetch.c -o fetch.wasm
echo "built plugins/fetch/fetch.wasm"
