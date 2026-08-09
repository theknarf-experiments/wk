#!/usr/bin/env bash
# Build a CLAP plugin (its `clap_entry` translation unit) into a wk component that
# implements the wk:clap world, by linking it with the wk:clap shim and the
# wit-bindgen bindings. Targets wasm32-wasip2, which emits a component directly.
#
#   ./build.sh [plugin-source.c] [out.wasm]
#
# Requires wasi-sdk (WASI_SDK), wit-bindgen and wasm-tools on PATH.
set -euo pipefail
cd "$(dirname "$0")"

WASI_SDK="${WASI_SDK:-$HOME/wasi-sdk}"
WIT_DIR="../../crates/wk-server/wit-clap"
PLUGIN_SRC="${1:-plugin-template.c}"
OUT="${2:-clap-template.wasm}"

# 1. Generate C bindings for the wk:clap world (the shim implements these).
rm -rf gen && mkdir -p gen
wit-bindgen c --world plugin "$WIT_DIR" --out-dir gen

# 2. Compile the shim + plugin + bindings into a component.
CLANG_PATH="$WASI_SDK/bin:/usr/bin:/bin"
# `-mexec-model=reactor`: this is a library of exports the host drives (no
# `main`/`run`), not a command — so the component exports only wk:clap/plugins.
env PATH="$CLANG_PATH" "$WASI_SDK/bin/clang" --target=wasm32-wasip2 -O2 \
    -mexec-model=reactor -Wall -I . -I clap-include \
    shim.c "$PLUGIN_SRC" gen/plugin.c gen/plugin_component_type.o \
    -o "$OUT"

echo "built plugins/clap-template/$OUT"
