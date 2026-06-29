#!/usr/bin/env bash
# Build the UNMODIFIED upstream SQLite amalgamation (engine + CLI shell) into a
# wasi:cli command component that wk runs in a terminal node — a real SQL shell.
# Proves a large (~250k LOC) real-world C program runs on wk unchanged.
#
# With no DB argument the shell opens a transient in-memory database and reads
# SQL from stdin (interactive `sqlite>` prompt). The only non-stock piece (kilo
# principle — supply via the runtime, don't patch the app) is compat/compat.c,
# which stubs system() (WASI omits it; the shell only uses it for .system/.shell).
#
# Requires wasi-sdk (WASI_SDK, default ~/wasi-sdk) and wasm-tools. The amalgamation
# is fetched (and cached) under sqlite-amalgamation-$VER/ on first run.
set -euo pipefail
cd "$(dirname "$0")"

WASI_SDK="${WASI_SDK:-$HOME/wasi-sdk}"
VER=3530300            # SQLite 3.53.3
YEAR=2026
DIR="sqlite-amalgamation-$VER"
ADAPTER="${WASI_ADAPTER:-$(find "$HOME/.cargo/registry/src" -name 'wasi_snapshot_preview1.command.wasm' 2>/dev/null | head -1)}"

# wasi-sdk's clang runs an optional wasm-opt post-link step; run clang with a
# PATH that omits wasm-opt (kept consistent with the other plugins). wasm-tools
# still runs under the normal PATH below.
CLANG_PATH="$WASI_SDK/bin:/usr/bin:/bin"

if [ ! -d "$DIR" ]; then
    echo "fetching SQLite $VER..."
    curl -fsSL "https://www.sqlite.org/$YEAR/$DIR.zip" -o "$DIR.zip"
    unzip -oq "$DIR.zip"
    rm -f "$DIR.zip"
fi

env PATH="$CLANG_PATH" "$WASI_SDK/bin/clang" --target=wasm32-wasip1 -O2 \
    -DSQLITE_THREADSAFE=0 -DSQLITE_OMIT_LOAD_EXTENSION -DSQLITE_OMIT_WAL \
    -DSQLITE_DISABLE_LFS \
    -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_PROCESS_CLOCKS \
    -D_WASI_EMULATED_MMAN -D_WASI_EMULATED_GETPID \
    -Wno-deprecated-declarations \
    -I"$DIR" \
    "$DIR/shell.c" "$DIR/sqlite3.c" compat/compat.c \
    -lwasi-emulated-signal -lwasi-emulated-process-clocks \
    -lwasi-emulated-mman -lwasi-emulated-getpid \
    -o sqlite3.core.wasm

wasm-tools component new sqlite3.core.wasm --adapt "$ADAPTER" -o sqlite3.wasm
rm -f sqlite3.core.wasm
echo "built plugins/sqlite/sqlite3.wasm"
