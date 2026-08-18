#!/usr/bin/env bash
# Build zipfs: a read-only archive-view filesystem over miniz (UNMODIFIED
# upstream amalgamation, fetched at build) and the shared ../libfuse-compat
# shim. Wire a Volume/BindMount holding a .zip into this node; the archive's
# tree appears wherever this node is mounted.
#
# Requires wasi-sdk (set WASI_SDK, default ~/wasi-sdk), wasm-tools,
# wit-bindgen, unzip.
set -euo pipefail
cd "$(dirname "$0")"
WASI_SDK="${WASI_SDK:-$HOME/wasi-sdk}"
CLANG="$WASI_SDK/bin/clang"

MINIZ_VER=3.0.2
if [ ! -f miniz.c ] || [ ! -f miniz.h ]; then
    echo "fetching miniz $MINIZ_VER..."
    curl -fsSL "https://github.com/richgel999/miniz/releases/download/$MINIZ_VER/miniz-$MINIZ_VER.zip" -o /tmp/miniz-$MINIZ_VER.zip
    unzip -o /tmp/miniz-$MINIZ_VER.zip miniz.c miniz.h -d .
fi

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
    -I"$FUSECOMPAT" -I"$FUSEGEN" -I. \
    zipfs.c miniz.c "$FUSECOMPAT/fuse_shim.c" "$FUSEGEN/wkfuse.c" "$FUSEGEN/wkfuse_component_type.o" \
    -o zipfs.core.wasm

wasm-tools component new zipfs.core.wasm --adapt "wasi_snapshot_preview1=$ADAPTER" -o zipfs.wasm
rm -f zipfs.core.wasm
echo "built plugins/zipfs/zipfs.wasm"
