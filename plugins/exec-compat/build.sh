#!/usr/bin/env bash
# Build the wk:exec guest shim and its demo.
#
# The shim (wkexec.c/.h) gives C programs a blocking `wk_run()` — posix_spawn +
# waitpid in one call — over wk's `wk:exec` capability, so an ordinary program
# can run other programs despite WASI having no fork/exec. The demo runs the
# cross-compiled GNU coreutils multicall binary several ways, including a real
# pipeline (it carries one program's stdout into the next one's stdin).
#
# The Dockerfile packages both plus coreutils.wasm at /bin, since wk:exec reads
# the program from the *calling node's own* filesystem.
set -euo pipefail
cd "$(dirname "$0")"

WASI_SDK="${WASI_SDK:-$HOME/wasi-sdk}"
WASMTIME_VER=46.0.1
ADAPTER="${WASI_ADAPTER:-$(find "$HOME/.cargo/registry/src" -name 'wasi_snapshot_preview1.command.wasm' 2>/dev/null | head -1)}"
if [ -z "$ADAPTER" ] || [ ! -f "$ADAPTER" ]; then
    ADAPTER="./wasi_snapshot_preview1.command.wasm"
    [ -f "$ADAPTER" ] || curl -fsSL \
        "https://github.com/bytecodealliance/wasmtime/releases/download/v$WASMTIME_VER/wasi_snapshot_preview1.command.wasm" \
        -o "$ADAPTER"
fi

# Bindings for wk:exec (must match crates/wk-server/wit-exec/world.wit).
wit-bindgen c --world exec-host wit/exec.wit --out-dir gen

"$WASI_SDK/bin/clang" --target=wasm32-wasip1 -O2 -I. -Igen \
    demo.c wkexec.c gen/exec_host.c gen/exec_host_component_type.o \
    -o demo.core.wasm
wasm-tools component new demo.core.wasm --adapt "wasi_snapshot_preview1=$ADAPTER" -o execdemo.wasm
rm -f demo.core.wasm

# The demo execs coreutils, so the image ships it next door.
cp ../coreutils/coreutils.wasm coreutils.wasm
echo "built plugins/exec-compat/execdemo.wasm (build the image with: wk images build plugins/exec-compat/Dockerfile --tag exec-demo)"
