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

patch_stamp() { cat patches/wk-*.patch | cksum | awk '{print $1, $2}'; }

if [ ! -d bun ]; then
    echo "fetching bun @ $BUN_REV..."
    git clone --depth 1 https://github.com/oven-sh/bun.git bun
    ( cd bun && git fetch --depth 1 origin "$BUN_REV" && git checkout "$BUN_REV" )
    for p in patches/wk-*.patch; do
        ( cd bun && git apply "../$p" )
    done
    patch_stamp > bun/.wk-patch-stamp
elif [ "$(patch_stamp)" != "$(cat bun/.wk-patch-stamp 2>/dev/null)" ]; then
    # Patches are applied at clone time only, so an existing tree silently keeps
    # whatever was current when it was made — and cargo's complaint about the
    # resulting half-applied workspace ("failed to load manifest for workspace
    # member src/wk_cli") points nowhere near the cause. Re-applying over a tree
    # that already has most hunks isn't safe to automate, so stop and say so.
    echo "build.sh: patches/wk-*.patch changed since bun/ was checked out." >&2
    echo "  Re-clone to pick them up:  rm -rf plugins/bun/bun && mise run build" >&2
    echo "  (native/ is untouched by this — JSC and ICU do not get rebuilt.)" >&2
    exit 1
fi

# vendor/lolhtml is a path dep cargo insists on resolving; normally fetched by
# bun's own configure (scripts/build/deps/lolhtml.ts).
if [ ! -d bun/vendor/lolhtml ]; then
    # A fresh bun clone has no vendor/ dir (its entries are submodules a plain
    # checkout never materializes) — only a previously-built tree has one.
    mkdir -p bun/vendor
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
    # No MI_MALLOC_OVERRIDE: mimalloc's gates are defined()-based, so even
    # =0 overrides malloc/free and collides with wasi-libc's allocator.
    "$WASI_SDK/bin/clang" --target=wasm32-wasip2 -O2 -DNDEBUG \
        -D_WASI_EMULATED_GETPID \
        -Inative/mimalloc/include -c native/mimalloc/src/static.c -o native/mimalloc.o
    "$WASI_SDK/bin/llvm-ar" rcs native/libmimalloc.a native/mimalloc.o
fi

# JSC + support archives (build-jsc.sh must have run). Everything goes on the
# FINAL link line via -C link-arg — `-l static=` in RUSTFLAGS would make every
# rlib try to bundle the archives. -L points at jsc-build/lib directly because
# cmake emits THIN archives whose member paths are relative to that directory.
# libc++/libc++abi + the wasi-emulated libs are copied out of the wasi-sdk
# sysroot by build-jsc.sh's stage step (never -L the sysroot itself: its
# libc.a preempts rustc's and breaks the crt). ICU keeps its own -L: the cross
# build installs libicu{i18n,uc,data}.a under native/icu-wasi/install/lib and
# nothing stages them next to JSC's archives.
if [ ! -f native/jsc-build/lib/libJavaScriptCore.a ]; then
    echo "== JSC + ICU for wasi not built yet — running ./build-jsc.sh" >&2
    echo "== (first run takes a long while: ICU host tools, ICU cross, then" >&2
    echo "==  all of JavaScriptCore; the script is stage-resumable)" >&2
    ./build-jsc.sh
fi
# Ask the driver where each archive is instead of assembling the path by hand:
# wasi-sdk 34 splits the C++ runtime into lib/wasm32-wasip2/{eh,noeh}/ while the
# wasi-emulated libs stay flat, so a single hardcoded directory finds some of
# them and not others. -print-file-name also picks the SAME eh/noeh variant the
# JSC link resolved, since build-jsc.sh drives this same clang with no
# -fwasm-exceptions (its sjlj lowering rides on -mllvm flags).
for lib in libc++.a libc++abi.a libsetjmp.a libwasi-emulated-getpid.a \
           libwasi-emulated-signal.a libwasi-emulated-mman.a \
           libwasi-emulated-process-clocks.a; do
    [ -f "native/$lib" ] && continue
    src="$("$WASI_SDK/bin/clang++" --target=wasm32-wasip2 -print-file-name="$lib")"
    [ -f "$src" ] || { echo "build.sh: $lib not found in $WASI_SDK's sysroot" >&2; exit 1; }
    cp "$src" native/
done

N="$PWD/native"

# bun compiles with its OWN pinned nightly, requested via `cargo +TOOLCHAIN`,
# so the repo's rust-toolchain.toml — including its `targets` list — governs
# none of it. rustup installs only the host std for a freshly-synced channel,
# so ask for the cross target explicitly; it's a no-op once present.
BUN_NIGHTLY=nightly-2026-07-20
rustup target add wasm32-wasip2 --toolchain "$BUN_NIGHTLY" >/dev/null

cd bun
# wasm-component-ld emits a component directly; wasi_shims.rs (in wk_cli)
# supplies the JSC-tier symbols and the SIMD-kernel scalar fallbacks;
# jsc_api.rs drives evaluation (--run) over JSC's C API. 8 MB stack: JSC's
# reserved zone alone is bigger than wasm-ld's 64 KB default.
# -A dead_code/unused-*: bun_runtime stubs many features on wasi (spawn,
# sockets, netif, PTY, fs.watch), leaving helpers dead on this target only —
# legitimate for a feature-stubbed cross-compile, and NOT source-level
# allow()s (the repo hook rejects those).
RUSTFLAGS="-A dead_code -A unused-variables -A unused-imports -A unused-mut -A unreachable-code \
    -C link-arg=-L$N -C link-arg=-L$N/jsc-build/lib \
    -C link-arg=-L$N/icu-wasi/install/lib \
    -C link-arg=-lmimalloc -C link-arg=-lJavaScriptCore -C link-arg=-lWTF \
    -C link-arg=-lbmalloc -C link-arg=-licui18n -C link-arg=-licuuc \
    -C link-arg=-licudata -C link-arg=-lc++ -C link-arg=-lc++abi \
    -C link-arg=-lsetjmp -C link-arg=-lwasi-emulated-getpid \
    -C link-arg=-lwasi-emulated-signal -C link-arg=-lwasi-emulated-mman \
    -C link-arg=-lwasi-emulated-process-clocks \
    -C link-arg=-z -C link-arg=stack-size=8388608" \
    cargo "+$BUN_NIGHTLY" build -p bun_wk_cli --target wasm32-wasip2 --profile release-dev
cd ..
cp bun/target/wasm32-wasip2/release-dev/bun-transpile.wasm bun-transpile.wasm
echo "built plugins/bun/bun-transpile.wasm (real Bun transpiler, wasm32-wasip2 component)"
echo "package it with: wk images build plugins/bun/Dockerfile --tag bun-transpile"
