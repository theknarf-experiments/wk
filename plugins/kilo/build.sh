#!/usr/bin/env bash
# Build antirez's kilo (UNMODIFIED upstream kilo.c) into a wasi:cli command
# component that wk runs in a terminal node.
#
# The only non-stock pieces are in compat/ — a <termios.h> that bridges raw mode
# to wk's terminal (WASI has no terminal-attributes interface yet) and a
# <sys/ioctl.h> that adds TIOCGWINSZ so kilo falls back to its ESC[6n size query.
# Everything else is wasi-libc (signal via its own emulation). No kilo.c edits.
#
# Requires wasi-sdk (set WASI_SDK, default ~/wasi-sdk) and wasm-tools.
set -euo pipefail
cd "$(dirname "$0")"
WASI_SDK="${WASI_SDK:-$HOME/wasi-sdk}"
ADAPTER="${WASI_ADAPTER:-$(find "$HOME/.cargo/registry/src" -name 'wasi_snapshot_preview1.command.wasm' 2>/dev/null | head -1)}"

"$WASI_SDK/bin/clang" --target=wasm32-wasi -O2 \
    -D_WASI_EMULATED_SIGNAL \
    -Icompat \
    kilo.c -lwasi-emulated-signal \
    -o kilo.core.wasm

wasm-tools component new kilo.core.wasm --adapt "$ADAPTER" -o kilo.wasm
rm -f kilo.core.wasm
echo "built plugins/kilo/kilo.wasm"
