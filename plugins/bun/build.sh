#!/usr/bin/env bash
# Port REAL Bun — the post-rewrite Rust workspace (May 2026, PR #30412) — to
# wasm32-wasip2, in phases:
#
#   1. (this script, WIP) the JSC-free "pure slice": bun_transpiler /
#      bun_bundler and their ~60-crate dependency closure, compiled with the
#      SAME pinned nightly bun uses, as a wk terminal component that
#      transpiles TS/JSX for real.
#   2. (moonshot) JavaScriptCore itself — cloop/LLInt interpreter, no JIT —
#      via wasi-sdk, then bun's *_jsc bridge crates over it.
#
# Status: WORKS. `bun-transpile demo.ts` emits real Bun transpiler output
# (TS stripped, const enums inlined, JSX automatic runtime) under wasmtime.
# The port knowledge lives in patches/ + src/wk_cli/wasi_shims.rs; two
# runtime-debugging lessons worth keeping:
#   * rustc links its OWN bundled wasi-libc (not wasi-sdk's) — it predates
#     F_DUPFD, so dup() fails EINVAL and the dir iterator reopens "."
#     through the dirfd instead. Never add the sdk lib dir to the search
#     path to "fix" this: its libc.a preempts rustc's and breaks
#     __wasi_init_tp in rust's crt1.
#   * wasi-libc has TWO fcntl headers; __header_fcntl.h is the canonical
#     one (F_DUPFD=5, F_DUPFD_CLOEXEC=6 — fcntl.h's 1030 is a musl
#     leftover).
set -euo pipefail
cd "$(dirname "$0")"

WASI_SDK="${WASI_SDK:-$HOME/wasi-sdk}"
# The rewrite is merged on main; pin the revision the patches were cut against.
BUN_REV=b7a0431032129d74fa4a7e3704eaf57b92fa9136
LOLHTML_COMMIT=725ce499aa9b71e38b7a2d0a9fbb6d7294a4079e  # oven-sh/lol-html `bun` branch (scripts/build/deps/lolhtml.ts)

if [ ! -d bun ]; then
    echo "fetching bun @ $BUN_REV..."
    git clone --depth 1 https://github.com/oven-sh/bun.git bun
    ( cd bun && git fetch --depth 1 origin "$BUN_REV" && git checkout "$BUN_REV" )
    for p in patches/wk-*.patch; do
        ( cd bun && git apply "../$p" )
    done
fi

# vendor/lolhtml is a path dep cargo insists on resolving; normally fetched by
# bun's own configure (scripts/build/deps/lolhtml.ts).
if [ ! -d bun/vendor/lolhtml ]; then
    curl -fsSL "https://github.com/oven-sh/lol-html/archive/$LOLHTML_COMMIT.tar.gz" | tar xz -C bun/vendor
    mv "bun/vendor/lol-html-$LOLHTML_COMMIT" bun/vendor/lolhtml
fi

# Configure-time codegen bun_core/bun_parsers include!() — regenerated from
# bun's own generators + a templated build_options.rs (see gen-codegen.ts).
[ -f codegen/build_options.rs ] || bun gen-codegen.ts

export BUN_CODEGEN_DIR="$PWD/codegen"
export CC_wasm32_wasip2="$WASI_SDK/bin/clang"
export AR_wasm32_wasip2="$WASI_SDK/bin/llvm-ar"

# mimalloc is bun's global allocator and officially supports wasi
# (src/prim/wasi); build it from bun's own pin (scripts/build/deps/mimalloc.ts).
MIMALLOC_COMMIT=1803341d6241d8fa4b3f65fa68cb13a32ad92f04
if [ ! -f native/libmimalloc.a ]; then
    mkdir -p native
    if [ ! -d native/mimalloc ]; then
        curl -fsSL "https://github.com/oven-sh/mimalloc/archive/$MIMALLOC_COMMIT.tar.gz" | tar xz -C native
        mv "native/mimalloc-$MIMALLOC_COMMIT" native/mimalloc
    fi
    "$WASI_SDK/bin/clang" --target=wasm32-wasip2 -O2 -DNDEBUG \
        -DMI_MALLOC_OVERRIDE=0 -D_WASI_EMULATED_GETPID \
        -Inative/mimalloc/include -c native/mimalloc/src/static.c -o native/mimalloc.o
    "$WASI_SDK/bin/llvm-ar" rcs native/libmimalloc.a native/mimalloc.o
fi

cd bun
# wasm-component-ld emits a component directly; wasi_shims.rs (in wk_cli)
# supplies the JSC-tier symbols and the SIMD-kernel scalar fallbacks.
RUSTFLAGS="-L ../native -l static=mimalloc" \
    cargo +nightly-2026-07-20 build -p bun_wk_cli --target wasm32-wasip2 --profile release-dev
cd ..
cp bun/target/wasm32-wasip2/release-dev/bun-transpile.wasm bun-transpile.wasm
echo "built plugins/bun/bun-transpile.wasm (real Bun transpiler, wasm32-wasip2 component)"
echo "package it with: wk images build plugins/bun/Dockerfile --tag bun-transpile"
