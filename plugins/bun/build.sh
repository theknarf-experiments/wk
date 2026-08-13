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
# Status: `cargo check -p bun_transpiler --target wasm32-wasip2` PASSES — the
# whole ~60-crate JSC-free slice type-checks, including bun_sys (readdir dir
# iterator, wasi_libc shim for the libc crate's wasip2 gaps), bun_uws_sys,
# bun_io (WasiWaker; reactor internals gated), bun_watcher (inert WasiWatcher
# backend), bun_crash_handler (no signals/dladdr arms), and the parser/
# printer/resolver/bundler themselves. Next: `cargo build` + a thin bin crate
# + componentize (expect link-time work: __bun_macro_context_* and other
# *_jsc-provided symbols need stubs; -lwasi-emulated-* for getrusage).
#
# The port knowledge, each item a compile failure first:
#   * bun assumes 64-bit everywhere it tags pointer high bits. Three separate
#     schemes hit this: bun_alloc's ZigString (bits 61-63 → moved to bits
#     29-31 on 32-bit, strings must sit below 512 MiB of linear memory),
#     bun_core's SmolStr (tag lives in the upper 64-bit word of a u128 — raw
#     accessors widened usize→u64 so the tag survives 32-bit), and
#     bun_semver's packed handle (pure u64 off/len math — only its assert was
#     over-strict).
#   * bun_windows_sys compiles on EVERY target (type aliases stay valid
#     cross-platform, deliberately); its WSADATA size assert and asm-bodied
#     teb()/peb() needed wasm arms.
#   * WASI's errno VALUES are CloudABI's alphabetical ordering — nothing like
#     Linux/Darwin. src/errno/wasi_errno.rs is generated from wasi-libc's
#     __errno_values.h; EWOULDBLOCK/EOPNOTSUPP (header aliases) and the
#     errnos WASI lacks get synthetic discriminants past the real range.
#   * `-D warnings` + deny(dead_code/unused) means every cfg hole is a hard
#     error: each new target arm must consume its bindings (`let _ = name;`).
#   * configure-time codegen (build_options.rs, {json,xml}_byte_class.rs) is
#     produced by `bun bd --configure-only`, which we never run; gen-codegen.ts
#     regenerates the byte-class tables from bun's own generators and
#     build_options.rs is templated in gen-codegen.ts. Cargo finds them via
#     BUN_CODEGEN_DIR.
#
# Requires: rustup (the pinned nightly in bun/rust-toolchain.toml +
# wasm32-wasip2 target auto-install), wasi-sdk (WASI_SDK), bun (drives
# gen-codegen.ts). Source is cloned (and cached) under bun/ on first run.
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

cd bun
# WIP: still `check` — flips to `build` + wasm-component output once the
# slice closes (bun_sys / bun_uws_sys are the frontier; see plugin notes).
cargo check -p bun_transpiler --target wasm32-wasip2
