#!/usr/bin/env bash
# Build antirez's kilo (UNMODIFIED upstream kilo.c) into a wasi:cli command
# component that wk runs in a terminal node.
#
# The only non-stock piece is the shared ../tty-compat termios shim: wasi-libc
# has no <termios.h>/tty control, so the shim provides them and maps termios
# (raw mode, window size) onto wk's wk:tty/control capability. No kilo.c edits.
#
# Requires wasi-sdk (set WASI_SDK, default ~/wasi-sdk), wasm-tools, wit-bindgen.
set -euo pipefail
cd "$(dirname "$0")"
WASI_SDK="${WASI_SDK:-$HOME/wasi-sdk}"
CLANG="$WASI_SDK/bin/clang"

# Shared terminal shim + its wk:tty/control bindings (regenerated each build).
TTYCOMPAT="$(pwd)/../tty-compat"
TTYGEN="$TTYCOMPAT/gen"
mkdir -p "$TTYGEN"
wit-bindgen c --world terminal "$TTYCOMPAT/wit/tty.wit" --out-dir "$TTYGEN"

# WASIp1→component adapter, pinned to our wasmtime (46); fetched and cached if a
# registry copy isn't present. Named `wasi_snapshot_preview1=` so wasm-tools
# binds it regardless of the file's stem.
WASMTIME_VER=46.0.1
ADAPTER="${WASI_ADAPTER:-$(find "$HOME/.cargo/registry/src" -name 'wasi_snapshot_preview1.command.wasm' 2>/dev/null | head -1)}"
if [ -z "$ADAPTER" ] || [ ! -f "$ADAPTER" ]; then
    ADAPTER="$TTYGEN/wasi_snapshot_preview1.command.wasm"
    if [ ! -f "$ADAPTER" ]; then
        echo "fetching WASI command adapter $WASMTIME_VER..."
        curl -fsSL "https://github.com/bytecodealliance/wasmtime/releases/download/v$WASMTIME_VER/wasi_snapshot_preview1.command.wasm" -o "$ADAPTER"
    fi
fi

"$CLANG" --target=wasm32-wasip1 -O2 \
    -D_WASI_EMULATED_SIGNAL \
    -I"$TTYCOMPAT" -I"$TTYGEN" \
    kilo.c "$TTYCOMPAT/termios.c" "$TTYGEN/terminal.c" "$TTYGEN/terminal_component_type.o" \
    -lwasi-emulated-signal \
    -o kilo.core.wasm

wasm-tools component new kilo.core.wasm --adapt "wasi_snapshot_preview1=$ADAPTER" -o kilo.wasm
rm -f kilo.core.wasm
echo "built plugins/kilo/kilo.wasm"
