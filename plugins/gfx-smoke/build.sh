#!/usr/bin/env bash
# Build the gfx-compat smoke guest: a C main() that opens a wasi-gfx surface
# through the shared ../gfx-compat shim and paints an animated gradient with
# an input-driven square. This is gfx-compat's test asset, the way hellofuse
# is libfuse-compat's.
#
# Requires wasi-sdk (set WASI_SDK; defaults to the mise-pinned install),
# wasm-tools, wit-bindgen.
set -euo pipefail
cd "$(dirname "$0")"

# Default to the mise-pinned toolchain when present; ~/wasi-sdk may be stale.
MISE_SDK="$HOME/.local/share/mise/installs/github-web-assembly-wasi-sdk/wasi-sdk-34-rc.2"
WASI_SDK="${WASI_SDK:-$([ -d "$MISE_SDK" ] && echo "$MISE_SDK" || echo "$HOME/wasi-sdk")}"
CLANG="$WASI_SDK/bin/clang"

# Shared gfx shim + its wasi-gfx bindings (regenerated each build).
GFXCOMPAT="$(pwd)/../gfx-compat"
GFXGEN="$GFXCOMPAT/gen"
mkdir -p "$GFXGEN"
wit-bindgen c --world wkgfx "$GFXCOMPAT/wit" --out-dir "$GFXGEN"

# WASIp1→component adapter, pinned to our wasmtime (46); fetched and cached if a
# registry copy isn't present. Named `wasi_snapshot_preview1=` so wasm-tools
# binds it regardless of the file's stem.
WASMTIME_VER=46.0.1
ADAPTER="${WASI_ADAPTER:-$(find "$HOME/.cargo/registry/src" -name 'wasi_snapshot_preview1.command.wasm' 2>/dev/null | head -1)}"
if [ -z "$ADAPTER" ] || [ ! -f "$ADAPTER" ]; then
    ADAPTER="$GFXGEN/wasi_snapshot_preview1.command.wasm"
    if [ ! -f "$ADAPTER" ]; then
        echo "fetching WASI command adapter $WASMTIME_VER..."
        curl -fsSL "https://github.com/bytecodealliance/wasmtime/releases/download/v$WASMTIME_VER/wasi_snapshot_preview1.command.wasm" -o "$ADAPTER"
    fi
fi

"$CLANG" --target=wasm32-wasip1 -O2 \
    -I"$GFXCOMPAT" -I"$GFXGEN" \
    main.c "$GFXCOMPAT/wkgfx.c" "$GFXGEN/wkgfx.c" "$GFXGEN/wkgfx_component_type.o" \
    -o gfx-smoke.core.wasm

wasm-tools component new gfx-smoke.core.wasm --adapt "wasi_snapshot_preview1=$ADAPTER" -o gfx-smoke.wasm
rm -f gfx-smoke.core.wasm
echo "built plugins/gfx-smoke/gfx-smoke.wasm"
