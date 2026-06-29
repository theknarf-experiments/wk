#!/usr/bin/env bash
# Build the UNMODIFIED upstream Lua 5.4.7 interpreter into a wasi:cli command
# component that wk runs in a terminal node — a real REPL / script interpreter.
#
# The point: Lua's error handling (error/pcall) is setjmp/longjmp, which lowers
# to the WebAssembly exception-handling proposal. wasi-sdk emits the *legacy* EH
# by default, but wasmtime (our runtime) only supports the new `exnref` model,
# so we compile with `-mllvm -wasm-use-legacy-eh=false` (+ -wasm-enable-sjlj) and
# the host enables `Config::wasm_exceptions`. No Lua source is edited: the only
# non-stock pieces are compat/compat.c (three libc stubs WASI omits) and the
# -DL_tmpnam override (LUA_TMPNAMBUFSIZE is config-overridable by design).
#
# Requires wasi-sdk (WASI_SDK, default ~/wasi-sdk) and wasm-tools. Lua source is
# fetched (and cached) under lua-5.4.7/ on first run.
set -euo pipefail
cd "$(dirname "$0")"

WASI_SDK="${WASI_SDK:-$HOME/wasi-sdk}"
LUA_VER=5.4.7
LUA_DIR="lua-$LUA_VER"
ADAPTER="${WASI_ADAPTER:-$(find "$HOME/.cargo/registry/src" -name 'wasi_snapshot_preview1.command.wasm' 2>/dev/null | head -1)}"

# wasi-sdk's clang runs wasm-opt as an optional post-link step, but the wasm-opt
# on PATH can't parse the new exnref EH we emit ("bad node code"). Run clang with
# a PATH that omits it, so the (optional) wasm-opt pass is simply skipped;
# wasm-tools still runs under the normal PATH below.
CLANG_PATH="$WASI_SDK/bin:/usr/bin:/bin"

if [ ! -d "$LUA_DIR" ]; then
    echo "fetching Lua $LUA_VER..."
    curl -fsSL "https://www.lua.org/ftp/$LUA_DIR.tar.gz" -o "$LUA_DIR.tar.gz"
    tar xzf "$LUA_DIR.tar.gz"
    rm -f "$LUA_DIR.tar.gz"
fi

# All core + library sources except luac.c (that's the separate compiler main;
# lua.c is the interpreter main we want).
SRC=$(ls "$LUA_DIR"/src/*.c | grep -v '/luac.c$' | tr '\n' ' ')

env PATH="$CLANG_PATH" "$WASI_SDK/bin/clang" --target=wasm32-wasip1 -O2 \
    -mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false \
    -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_PROCESS_CLOCKS -DL_tmpnam=32 \
    -Wno-deprecated-declarations \
    -I"$LUA_DIR/src" \
    $SRC compat/compat.c \
    -lsetjmp -lwasi-emulated-signal -lwasi-emulated-process-clocks \
    -o lua.core.wasm

wasm-tools component new lua.core.wasm --adapt "$ADAPTER" -o lua.wasm
rm -f lua.core.wasm
echo "built plugins/lua/lua.wasm"
