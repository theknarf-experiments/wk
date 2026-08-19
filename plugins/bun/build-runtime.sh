#!/usr/bin/env bash
# Build the FULL bun runtime — real Bun on JavaScriptCore, wasm32-wasip2 —
# into plugins/bun/bun-run.wasm (~181 MB component). This is the hermetic
# orchestration of the recipe link/README.md used to only *document*: every
# stage that once lived in /tmp or a session scratchpad now lands under the
# gitignored native/runtime-build/ so a fresh clone (macOS or Linux) can run
# it end to end.
#
#   ./build-runtime.sh          # or: mise run build-runtime
#
# The transpiler slice (`mise run build` → build.sh → bun-transpile.wasm)
# is unchanged and stays the default; this build is opt-in — it is enormous
# (JSC + ICU + ~270 C++ binding TUs + a dozen vendored C libraries).
#
# Stages (each idempotent — reruns skip work already done):
#   1. bun source fetch + wk patches        (skipped when bun/ exists, like build.sh)
#   2. vendored C library sources → native/ (pinned commits, skipped when present)
#   3. JSC + ICU                            (build-jsc.sh, skipped when libs exist)
#   4. mimalloc + staged sysroot libs       (same recipe as build.sh)
#   5. configure-time codegen               (gen-codegen.ts, needs a host bun)
#   6. cargo build -p bun_bin               → libbun_rust.a
#   7. vendored C libs → runtime-build/vlib (link/build_*.sh, re-rooted)
#   8. link shims + C++ bindings/gen objects (link/build-shims.sh, build_cxx_objects.sh)
#   9. InternalModuleRegistryConstants imrc.o (its .size is COMPUTED, see below)
#  10. final link                           (link/link_all.sh) → bun-run.wasm
#  11. sanity: size + component imports/exports (wasm-tools, if available)
#
# Package it afterwards with:
#   wk images build plugins/bun/runtime.Dockerfile --tag bun-run
set -euo pipefail
cd "$(dirname "$0")"

# ── Toolchain ──────────────────────────────────────────────────────────────
# wasi-sdk is mise-pinned; refuse anything but the pinned version — the pipe
# shim (pipe-compat) transcribes wasi-libc descriptor-table internals from
# exactly this SDK's libc (same guard as plugins/bash/build.sh).
MISE_SDK="$HOME/.local/share/mise/installs/github-web-assembly-wasi-sdk/wasi-sdk-34-rc.2"
WASI_SDK="${WASI_SDK:-$([ -d "$MISE_SDK" ] && echo "$MISE_SDK" || echo "$HOME/wasi-sdk")}"
EXPECT="wasi-sdk-34-rc.2"
case "$WASI_SDK" in
    *"$EXPECT"*) ;;
    *) echo "build-runtime: expected $EXPECT (set WASI_SDK), got: $WASI_SDK" >&2; exit 1 ;;
esac
export WASI_SDK

# Host tools that must be resolved BEFORE the PATH sanitization below:
# a host bun (only needed when codegen/ is missing) and wasm-tools (only for
# the final sanity check).
BUN_HOST="$(command -v bun || true)"
WASM_TOOLS="$(command -v wasm-tools || true)"

# The sjlj-lowered objects (JSC cloop, the C++ bindings, libjpeg-turbo) use
# wasm exception instructions that wasm-opt does not understand; wasi-sdk's
# clang runs wasm-opt as an optional post-link pass if it can find one. Keep
# it OFF the PATH for the whole build (the lua/curl BUILD_PATH trick).
# ~/.cargo/bin stays: rustup's cargo shim resolves `+nightly-<pin>`.
export PATH="$WASI_SDK/bin:$HOME/.cargo/bin:/usr/bin:/bin"

for tool in git curl python3 cargo; do
    command -v "$tool" >/dev/null 2>&1 || { echo "build-runtime: missing host tool: $tool" >&2; exit 1; }
done

# ── Layout ─────────────────────────────────────────────────────────────────
# Everything the build produces (objects, archives, lists, logs) lives under
# native/runtime-build/ — repo-relative, gitignored, no /tmp, no user paths.
# The env contract below is shared with every link/build_*.sh script.
export BUN_PLUGIN="$PWD"
export BUN_NATIVE="$PWD/native"
export BUN="$PWD/bun"
export WORK="$BUN_NATIVE/runtime-build"
export VLIB="$WORK/vlib"
export OBJ="$WORK/obj"
mkdir -p "$WORK" "$VLIB" "$OBJ" "$WORK/lists" "$WORK/logs" "$WORK/bunobj"

# ── 1. bun source (post-rewrite Rust workspace) + wk patches ──────────────
# Same guarded fetch as build.sh: when bun/ already exists it is the source
# of truth (it may carry local wip commits) and is NOT touched.
BUN_REV=b7a0431032129d74fa4a7e3704eaf57b92fa9136
LOLHTML_COMMIT=725ce499aa9b71e38b7a2d0a9fbb6d7294a4079e  # oven-sh/lol-html `bun` branch
if [ ! -d bun ]; then
    echo "== fetching bun @ $BUN_REV"
    git clone --depth 1 https://github.com/oven-sh/bun.git bun
    ( cd bun && git fetch --depth 1 origin "$BUN_REV" && git checkout "$BUN_REV" )
    for p in patches/wk-*.patch; do
        ( cd bun && git apply "../$p" )
    done
fi
if [ ! -d bun/vendor/lolhtml ]; then
    mkdir -p bun/vendor
    curl -fsSL "https://github.com/oven-sh/lol-html/archive/$LOLHTML_COMMIT.tar.gz" | tar xz -C bun/vendor
    mv "bun/vendor/lol-html-$LOLHTML_COMMIT" bun/vendor/lolhtml
fi

# ── 2. vendored C library sources → native/ ────────────────────────────────
# Pinned to the commits the original port used (bun's own scripts/build/deps
# pins where one exists). Shallow git fetch for the big ones, tarballs for
# the rest. Each is skipped when the directory already exists.
fetch_git() { # dir repo commit
    [ -d "native/$1" ] && return 0
    echo "== fetching native/$1 @ $3"
    git init -q "native/$1"
    ( cd "native/$1" && git remote add origin "https://github.com/$2.git" && \
      git fetch --depth 1 origin "$3" && git checkout -q FETCH_HEAD )
}
fetch_tar() { # dir repo commit
    [ -d "native/$1" ] && return 0
    echo "== fetching native/$1 @ $3"
    curl -fsSL "https://github.com/$2/archive/$3.tar.gz" | tar xz -C native
    mv "native/$(basename "$2")-$3" "native/$1"
}
mkdir -p native
fetch_git boringssl  oven-sh/boringssl        1a41b9025c2c0a37edd07ff10f6944f03e028522
fetch_git zstd       facebook/zstd            f8745da6ff1ad1e7bab384bd1f9d742439278e99
fetch_git libarchive libarchive/libarchive    ded82291ab41d5e355831b96b0e1ff49e24d8939
fetch_git zlib       zlib-ng/zlib-ng          12731092979c6d07f42da27da673a9f6c7b13586
fetch_git cares      c-ares/c-ares            c7a3138dcfe3bb0eaaf10c0c24c36dc66dc790ab
fetch_git libdeflate ebiggers/libdeflate      c8c56a20f8f621e6a966b716b31f1dedab6a41e3
fetch_git hdrhistogram HdrHistogram/HdrHistogram_c be60a9987ee48d0abf0d7b6a175bad8d6c1585d1
# brotli is pinned by bun itself (scripts/build/deps/brotli.ts).
if [ ! -d native/brotli ]; then
    BROTLI_COMMIT=$(grep -oE 'commit: "[a-f0-9]{40}"' bun/scripts/build/deps/brotli.ts | head -1 | grep -oE '[a-f0-9]{40}')
    fetch_git brotli google/brotli "$BROTLI_COMMIT"
fi
fetch_tar libspng      randy408/libspng            fb768002d4288590083a476af628e51c3f1d47cd
fetch_tar libjpeg-turbo libjpeg-turbo/libjpeg-turbo e352b02f794f701407b39af08576035ba3360d60
fetch_tar highway      google/highway              2607d3b5b0113992fe84d3848859eae13b3b52c1
if [ ! -d native/nodejs-headers ]; then
    echo "== fetching native/nodejs-headers (node v26.3.0)"
    curl -fsSL "https://nodejs.org/dist/v26.3.0/node-v26.3.0-headers.tar.gz" | tar xz -C native
    mv native/node-v26.3.0 native/nodejs-headers
fi
# lshpack is fetched by link/build_lshpack.sh (its own pin), mimalloc below,
# webkit + icu by build-jsc.sh.

# ── 3. JSC + ICU ───────────────────────────────────────────────────────────
if [ ! -f native/jsc-build/lib/libJavaScriptCore.a ]; then
    echo "== JSC + ICU for wasi not built yet — running ./build-jsc.sh (slow, resumable)"
    ./build-jsc.sh
fi

# ── 4. mimalloc + staged sysroot libs (same recipe as build.sh) ───────────
MIMALLOC_COMMIT=1803341d6241d8fa4b3f65fa68cb13a32ad92f04
if [ ! -f native/libmimalloc.a ]; then
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
# Ask the driver where each archive is rather than assembling the path (same
# as build.sh): wasi-sdk 34 splits the C++ runtime into
# lib/wasm32-wasip2/{eh,noeh}/ while the wasi-emulated libs stay flat, so one
# hardcoded directory finds some of them and not others.
for lib in libc++.a libc++abi.a libsetjmp.a libwasi-emulated-getpid.a \
           libwasi-emulated-signal.a libwasi-emulated-mman.a \
           libwasi-emulated-process-clocks.a; do
    [ -f "native/$lib" ] && continue
    src="$("$WASI_SDK/bin/clang++" --target=wasm32-wasip2 -print-file-name="$lib")"
    [ -f "$src" ] || { echo "build-runtime: $lib not found in $WASI_SDK's sysroot" >&2; exit 1; }
    cp "$src" native/
done

# ── 5. configure-time codegen ─────────────────────────────────────────────
if [ ! -f codegen/build_options.rs ]; then
    [ -n "$BUN_HOST" ] || { echo "build-runtime: codegen/ missing and no host bun on PATH (brew install oven-sh/bun/bun)" >&2; exit 1; }
    "$BUN_HOST" gen-codegen.ts
fi
export BUN_CODEGEN_DIR="$PWD/codegen"

# ── 6. cargo build of the runtime crate closure ───────────────────────────
# bun_bin (not bun_wk_cli — that is the transpiler slice) with the SAME
# pinned nightly bun uses (bun/rust-toolchain.toml). The final link is
# external, so no -C link-arg here; the product is libbun_rust.a.
# -A dead_code/unused-*: wasi feature-stubs leave helpers dead on this
# target only (same rationale as build.sh).
echo "== cargo build -p bun_bin (wasm32-wasip2, release-dev)"
# `cargo +TOOLCHAIN` bypasses rust-toolchain.toml, targets list included, and
# rustup gives a freshly-synced channel host std only — ask for the cross
# target (a no-op once present) or the build dies in compiler_builtins with
# "can't find crate for `core`".
BUN_NIGHTLY=nightly-2026-07-20
rustup target add wasm32-wasip2 --toolchain "$BUN_NIGHTLY" >/dev/null
( cd bun && \
  CC_wasm32_wasip2="$WASI_SDK/bin/clang" AR_wasm32_wasip2="$WASI_SDK/bin/llvm-ar" \
  RUSTFLAGS="-A dead_code -A unused-variables -A unused-imports -A unused-mut -A unreachable-code" \
  cargo "+$BUN_NIGHTLY" build -p bun_bin --target wasm32-wasip2 --profile release-dev )
[ -f bun/target/wasm32-wasip2/release-dev/libbun_rust.a ] || { echo "build-runtime: libbun_rust.a missing after cargo build" >&2; exit 1; }

# ── 7. vendored C libraries → $VLIB ───────────────────────────────────────
# Each script is idempotent on its own archive; FORCE_VLIB=1 rebuilds all.
if [ "${FORCE_VLIB:-0}" = 1 ]; then rm -rf "$VLIB"; mkdir -p "$VLIB"; fi
[ -f "$VLIB/libbssl_crypto.a" ] || { bash link/build_boringssl.sh; bash link/build_bssl_ssl.sh; }
[ -f "$VLIB/libusockets.a" ]    || bash link/build_usockets.sh
[ -f "$VLIB/libzstd.a" ] && [ -f "$VLIB/libbrotli.a" ] || bash link/build-vendored.sh
bash link/build_vendored_extra.sh   # cares/archive/zlib/deflate/sqlite/llhttp/spng/jpeg/hdr (per-lib skips inside)

# ── 8. link shims + C++ bindings/codegen objects ──────────────────────────
bash link/build-shims.sh            # link/*.c shims + picohttpparser/wk:exec/pipe + lshpack
bash link/build_cxx_objects.sh      # ~560-TU bindings sweep, gen_*/mod_*, uws, root_certs, simdutf, imrc

# ── 10. final link ────────────────────────────────────────────────────────
echo "== final link (log: $WORK/logs/link.log)"
bash link/link_all.sh
ARTIFACT="$WORK/bun-run.wasm"
[ -s "$ARTIFACT" ] || { echo "build-runtime: link produced no artifact — tail of link.log:" >&2; tail -40 "$WORK/logs/link.log" >&2; exit 1; }
cp "$ARTIFACT" bun-run.wasm

# ── 11. sanity ────────────────────────────────────────────────────────────
SIZE=$(wc -c < bun-run.wasm | tr -d ' ')
echo "== built plugins/bun/bun-run.wasm ($SIZE bytes)"
if [ -n "$WASM_TOOLS" ]; then
    WIT="$("$WASM_TOOLS" component wit bun-run.wasm 2>/dev/null || true)"
    for want in "wasi:sockets" "wk:exec" "wasi:cli/run"; do
        if echo "$WIT" | grep -q "$want"; then echo "   wit: $want OK"; else echo "   wit: $want MISSING" >&2; exit 1; fi
    done
else
    echo "   (wasm-tools not found — skipping component wit sanity check)"
fi
echo "package it with: wk images build plugins/bun/runtime.Dockerfile --tag bun-run"
