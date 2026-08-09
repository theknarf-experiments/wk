#!/usr/bin/env bash
# Compile a CLAP plugin (its `clap_entry` translation unit, under examples/) into
# a wk component that implements the wk:clap world, by linking it with the
# wk:clap shim and the wit-bindgen bindings. Targets wasm32-wasip2, emitting a
# reactor component directly.
#
#   ./build.sh <name>     # examples/<name>.c or .cpp -> build/<name>.wasm
#   ./build.sh            # builds every example
#
# Requires wasi-sdk (WASI_SDK), wit-bindgen and wasm-tools on PATH.
set -euo pipefail
cd "$(dirname "$0")"

WASI_SDK="${WASI_SDK:-$HOME/wasi-sdk}"
WIT_DIR="../../crates/wk-server/wit-clap"
CLANG_PATH="$WASI_SDK/bin:/usr/bin:/bin"

# Generate C bindings for the wk:clap world once (the shim implements these).
gen_bindings() {
    rm -rf gen && mkdir -p gen
    wit-bindgen c --world plugin "$WIT_DIR" --out-dir gen
}

build_one() {
    local name="$1" src cc
    if [ -f "examples/$name.cpp" ]; then
        src="examples/$name.cpp"
        cc="$WASI_SDK/bin/clang++"
    elif [ -f "examples/$name.c" ]; then
        src="examples/$name.c"
        cc="$WASI_SDK/bin/clang"
    else
        echo "no examples/$name.c or .cpp" >&2
        return 1
    fi
    mkdir -p build
    # -mexec-model=reactor: a library of exports the host drives (no main/run).
    env PATH="$CLANG_PATH" "$cc" --target=wasm32-wasip2 -O2 \
        -mexec-model=reactor -I . -I clap-include \
        shim.c "$src" gen/plugin.c gen/plugin_component_type.o \
        -o "build/$name.wasm"
    echo "built plugins/clap/build/$name.wasm"
}

gen_bindings
if [ $# -ge 1 ]; then
    build_one "$1"
else
    for f in examples/*.c examples/*.cpp; do
        [ -e "$f" ] || continue
        n="$(basename "$f")"
        build_one "${n%.*}"
    done
fi
