#!/usr/bin/env bash
# Build httpfs: a read-only network filesystem over HTTP, whose network is
# wk's fabric — BSD sockets in the guest ride the userspace netstack, so a
# base URL like http://filesrv:8080 dials another node (or a host-side
# fabric endpoint) by name. Built on the shared ../libfuse-compat shim.
#
# wasip2 target: sockets don't exist in the wasip1 adapter world, so this
# builds like curl/netserve do — clang emits the component directly and
# wasm-component-ld merges the shim's wk:fs world in.
#
# Requires wasi-sdk (set WASI_SDK, default ~/wasi-sdk), wit-bindgen.
set -euo pipefail
cd "$(dirname "$0")"
WASI_SDK="${WASI_SDK:-$HOME/wasi-sdk}"
CLANG="$WASI_SDK/bin/clang"

FUSECOMPAT="$(pwd)/../libfuse-compat"
FUSEGEN="$FUSECOMPAT/gen"
mkdir -p "$FUSEGEN"
wit-bindgen c --world wkfuse "$FUSECOMPAT/wit" --out-dir "$FUSEGEN"

"$CLANG" --target=wasm32-wasip2 -O2 \
    -I"$FUSECOMPAT" -I"$FUSEGEN" \
    httpfs.c "$FUSECOMPAT/fuse_shim.c" "$FUSEGEN/wkfuse.c" "$FUSEGEN/wkfuse_component_type.o" \
    -o httpfs.wasm
echo "built plugins/httpfs/httpfs.wasm"
