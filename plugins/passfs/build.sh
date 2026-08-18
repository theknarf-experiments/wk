#!/usr/bin/env bash
# Build libfuse's passthrough.c (UNMODIFIED upstream example, fuse-3.16.2)
# into a wk:fs provider component. Upstream it mirrors the directory under
# the mountpoint; in wk "the underlying filesystem" is the node's OWN vfs —
# so this node re-exports its filesystem (image layers, wired volumes, its
# private writes) to every node wired to it. Share-your-rootfs as a wire.
#
# The only non-stock pieces are the shared ../libfuse-compat shim and, like
# hellofuse, sources fetched at build (GPLv2, not vendored).
#
# Requires wasi-sdk (set WASI_SDK, default ~/wasi-sdk), wasm-tools, wit-bindgen.
set -euo pipefail
cd "$(dirname "$0")"
WASI_SDK="${WASI_SDK:-$HOME/wasi-sdk}"
CLANG="$WASI_SDK/bin/clang"

for f in passthrough.c passthrough_helpers.h; do
    if [ ! -f "$f" ]; then
        echo "fetching libfuse fuse-3.16.2 example/$f..."
        curl -fsSL "https://raw.githubusercontent.com/libfuse/libfuse/fuse-3.16.2/example/$f" -o "$f"
    fi
done

FUSECOMPAT="$(pwd)/../libfuse-compat"
FUSEGEN="$FUSECOMPAT/gen"
mkdir -p "$FUSEGEN"
wit-bindgen c --world wkfuse "$FUSECOMPAT/wit" --out-dir "$FUSEGEN"

WASMTIME_VER=46.0.1
ADAPTER="${WASI_ADAPTER:-$(find "$HOME/.cargo/registry/src" -name 'wasi_snapshot_preview1.command.wasm' 2>/dev/null | head -1)}"
if [ -z "$ADAPTER" ] || [ ! -f "$ADAPTER" ]; then
    ADAPTER="$FUSEGEN/wasi_snapshot_preview1.command.wasm"
    if [ ! -f "$ADAPTER" ]; then
        echo "fetching WASI command adapter $WASMTIME_VER..."
        curl -fsSL "https://github.com/bytecodealliance/wasmtime/releases/download/v$WASMTIME_VER/wasi_snapshot_preview1.command.wasm" -o "$ADAPTER"
    fi
fi

"$CLANG" --target=wasm32-wasip1 -O2 \
    -I"$FUSECOMPAT" -I"$FUSEGEN" \
    passthrough.c "$FUSECOMPAT/fuse_shim.c" "$FUSEGEN/wkfuse.c" "$FUSEGEN/wkfuse_component_type.o" \
    -o passfs.core.wasm

wasm-tools component new passfs.core.wasm --adapt "wasi_snapshot_preview1=$ADAPTER" -o passfs.wasm
rm -f passfs.core.wasm
echo "built plugins/passfs/passfs.wasm"
