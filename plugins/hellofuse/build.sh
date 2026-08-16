#!/usr/bin/env bash
# Build libfuse's hello.c (UNMODIFIED upstream example, fuse-3.16.2) into a
# wk:fs provider component: the canonical FUSE hello-world filesystem as a wk
# node. Wire it into any app and `cat <mount>/hello` reads "Hello World!\n" —
# served by real libfuse example code that thinks it's talking to a kernel.
#
# The only non-stock piece is the shared ../libfuse-compat shim: wasi has no
# <fuse.h>, so the shim provides the libfuse 3.x high-level API and maps it
# onto wk's wk:fs/provider capability (fuse_main == the serve loop). No
# hello.c edits.
#
# Requires wasi-sdk (set WASI_SDK, default ~/wasi-sdk), wasm-tools, wit-bindgen.
set -euo pipefail
cd "$(dirname "$0")"
WASI_SDK="${WASI_SDK:-$HOME/wasi-sdk}"
CLANG="$WASI_SDK/bin/clang"

# The upstream example, fetched verbatim (GPLv2 — fetched at build like
# bash's sources, not vendored into this repo).
if [ ! -f hello.c ]; then
    echo "fetching libfuse fuse-3.16.2 example/hello.c..."
    curl -fsSL "https://raw.githubusercontent.com/libfuse/libfuse/fuse-3.16.2/example/hello.c" -o hello.c
fi

# Shared libfuse shim + its wk:fs bindings (regenerated each build).
FUSECOMPAT="$(pwd)/../libfuse-compat"
FUSEGEN="$FUSECOMPAT/gen"
mkdir -p "$FUSEGEN"
wit-bindgen c --world wkfuse "$FUSECOMPAT/wit" --out-dir "$FUSEGEN"

# WASIp1→component adapter, pinned to our wasmtime (46); fetched and cached if a
# registry copy isn't present. Named `wasi_snapshot_preview1=` so wasm-tools
# binds it regardless of the file's stem.
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
    hello.c "$FUSECOMPAT/fuse_shim.c" "$FUSEGEN/wkfuse.c" "$FUSEGEN/wkfuse_component_type.o" \
    -o hellofuse.core.wasm

wasm-tools component new hellofuse.core.wasm --adapt "wasi_snapshot_preview1=$ADAPTER" -o hellofuse.wasm
rm -f hellofuse.core.wasm
echo "built plugins/hellofuse/hellofuse.wasm"
