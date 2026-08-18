#!/usr/bin/env bash
# doc-tools: real pandoc + real pdfTeX as a wasm container layered on the
# bash image — so a bash node can
#   pandoc notes.md -s -o notes.tex && pdflatex notes.tex
# entirely inside wk.
#
# pandoc is NOT built here. The GHC WebAssembly backend builds the real
# pandoc-cli upstream (haskell-wasm/pandoc-wasm, née tweag/pandoc-wasm), and CI
# deploys the artifact to GitHub Pages — the same "fetch an upstream official
# binary" category as wk's wasmtime-adapter fetch. There are no tagged
# releases, so the fetch is pinned by content hash instead of by tag: a
# redeploy upstream fails the checksum here rather than silently changing what
# the image ships.
#
# What arrives is a wasm32-wasip1 *core module* (pandoc 3.5, data files baked
# in — `pandoc -s` needs no --data-dir). wk's `wk:exec` instantiates
# *components* (crates/wk-server/src/plugin.rs, spawn_program: Component::new,
# no adaptation at exec time), so the module is componentized here with the
# WASIp1 command adapter from the exact wasmtime release wk itself pins for
# pulled modules (oci.rs, cached_adapter: 46.0.1) — the same transformation
# `ensure_component` applies to a pulled image's entrypoint.
#
# Then the images are built + tagged into wk's local store, wordpress-style:
#   bash      <- the base image (needs plugins/bash built: bash.wasm + bin/)
#   doctools  <- FROM bash + /bin/pandoc, referenced as image://doctools
#
# pdfTeX landed too: build-tex.sh cross-compiles the real pdfTeX 1.40.29 from
# TeX Live source (wasi-sdk + a small POSIX shim) and assembles a minimal
# texmf tree; the Dockerfile then dumps latex.fmt with `pdftex -ini` in a RUN
# step — wk image builds execute wasm, so the format is made by the engine
# itself, the way fmtutil would. Stages are resumable (see build-tex.sh).
set -euo pipefail
cd "$(dirname "$0")"

# ---------------------------------------------------------------- pandoc ----
PANDOC_URL="https://haskell-wasm.github.io/pandoc-wasm/pandoc.wasm"
PANDOC_SHA256="48d9ceed3ef805f6acc28e6f58c2439cdeb1f71864244fffcc155e2c045aa7fc"

# The WASIp1→component command adapter, pinned to the same wasmtime release as
# wk's oci::ensure_component (46.0.1), so exec and pull agree on the adapter.
ADAPTER_VER="46.0.1"
ADAPTER_URL="https://github.com/bytecodealliance/wasmtime/releases/download/v${ADAPTER_VER}/wasi_snapshot_preview1.command.wasm"
ADAPTER_SHA256="6a13fa0ed7af65de3468fd6172abcfe6bb74e0b7f3bd0ef06e72f51ee32bc2a2"

sha_check() {
    local file="$1" want="$2" got
    got=$(shasum -a 256 "$file" | cut -d' ' -f1)
    if [ "$got" != "$want" ]; then
        echo "$file: sha256 mismatch" >&2
        echo "  want $want" >&2
        echo "  got  $got" >&2
        echo "(upstream redeployed? verify the new artifact, then update the pin)" >&2
        return 1
    fi
}

fetch_pinned() {
    local url="$1" sha="$2" out="$3"
    if [ -f "$out" ] && sha_check "$out" "$sha" 2>/dev/null; then
        return 0
    fi
    echo "fetching $url ..."
    curl -fSL --retry 3 -o "$out.part" "$url"
    sha_check "$out.part" "$sha"
    mv "$out.part" "$out"
}

build_pandoc() {
    # The upstream core module, cached; then componentized next to it.
    fetch_pinned "$PANDOC_URL" "$PANDOC_SHA256" pandoc-core.wasm
    fetch_pinned "$ADAPTER_URL" "$ADAPTER_SHA256" adapter/wasi_snapshot_preview1.command.wasm

    if [ ! -f pandoc.wasm ] || [ pandoc-core.wasm -nt pandoc.wasm ]; then
        echo "componentizing pandoc (WASIp1 module + command adapter ${ADAPTER_VER})..."
        wasm-tools component new pandoc-core.wasm \
            --adapt wasi_snapshot_preview1=adapter/wasi_snapshot_preview1.command.wasm \
            -o pandoc.wasm
    fi

    # The command-name symlink farm, staged like bash's: `pandoc` is a plain
    # symlink onto the component; bash's own PATH search follows it and argv[0]
    # stays "pandoc".
    mkdir -p bin
    ln -sf pandoc.wasm bin/pandoc
    echo "pandoc.wasm ready ($(du -h pandoc.wasm | cut -f1))"
}

# ---------------------------------------------------------------- pdfTeX ----
# pdftex from TeX Live source (web2c) cross-compiled with wasi-sdk, +
# kpathsea + a minimal texmf tree; latex.fmt is dumped during the image build
# (a Dockerfile RUN step executes wasm). Resumable stage-by-stage.
build_tex() {
    ./build-tex.sh
}

# ---------------------------------------------------------------- images ----
build_images() {
    WK="${WK:-$(command -v wk || echo ../../target/debug/wk)}"

    if [ ! -f ../bash/bash.wasm ] || [ ! -d ../bash/bin ]; then
        echo "plugins/bash is not built — build it first:" >&2
        echo "  (cd ../bash && mise run build)" >&2
        exit 1
    fi

    echo "building the bash base image..."
    "$WK" images build --tag bash ../bash/Dockerfile

    echo "building the doctools image..."
    "$WK" images build --tag doctools ./Dockerfile

    echo "done — the example uses image://doctools"
}

case "${1:-all}" in
    pandoc) build_pandoc ;;
    tex) build_tex ;;
    images) build_images ;;
    all)
        build_pandoc
        build_tex
        build_images
        ;;
    *)
        echo "usage: $0 [pandoc|tex|images|all]" >&2
        exit 2
        ;;
esac
