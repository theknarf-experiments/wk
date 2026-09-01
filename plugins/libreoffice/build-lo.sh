#!/usr/bin/env bash
# Stage 3 of 3: the cross build. Milestones M2 (module targets) and M3 (link).
#
# Plain `make` in ./build. Everything about the target — compiler, flags,
# platform makefile — came from build-configure.sh and from
# solenv/gbuild/platform/WASI_INTEL_GCC.mk; nothing is decided here.
#
# THIS IS THE LONG ONE, and it is also where the port is most likely to die.
# The two known ways, both already characterised in PORTING.md:
#
#   * the final link. wasm-ld / wasm-component-ld has to swallow ~200 archives,
#     thousands of objects and a component_maps.cxx referencing 1000+
#     constructors, on a 32 GB host; upstream's own static/README.wasm.md says
#     the Emscripten link "possibly needs 64GB RAM". And gbuild emits
#     -Wl,--start-group/--end-group on every DISABLE_DYNLOADING executable link
#     (unxgcc.mk:159,166), which BOTH wasm-ld and wasm-component-ld reject
#     outright — verified by running the linker. Removing them is mechanical;
#     whether wasm-ld then resolves LibreOffice's genuinely circular archive
#     graph (static.mk:26-51 describes the cycle) is unknown. Run PORTING.md's
#     experiment E1 before spending a night here.
#
#   * externals that have never seen wasi-sdk. cairo, pixman, fontconfig,
#     freetype, harfbuzz, ICU and boost all have EMSCRIPTEN arms in their
#     ExternalProject_*.mk, which is strong evidence the recipes are
#     cross-friendly, but none has been compiled for WASI. cairo/pixman is the
#     load-bearing one and its meson cross-file currently claims
#     system='linux', cpu_family from RTL_ARCH — which meson will not believe
#     against a wasm32 compiler.
#
# So: build a MODULE first. `./build-lo.sh sal` is minutes, not hours, and
# every wasi-libc gap this port has to close (pwd.h, getuid, dlsym,
# pthread_atfork, the ini path) lives in sal. That is milestone M2 and it is
# the direct analogue of the Qt port's M0. Do not run the full build until sal
# is clean.
#
# Usage: ./build-lo.sh            the whole thing (M3)
#        ./build-lo.sh sal        one module (M2 — start here)
#        ./build-lo.sh vcl        after the wk VCL backend exists (M6)
#
# Long. Run it detached and tail ./logs.
#
# Knobs: LOGDIR=...   (JOBS is deliberately ignored; see below)
set -uo pipefail
cd "$(dirname "$0")"
LO_STAGE=lo
# shellcheck source=common.sh
. ./common.sh

lo_require_src
lo_require_configured
lo_link_toolbin

# The thread shim goes on every link line this stage produces (gb_WASI_SHIM in
# WASI_INTEL_GCC.mk). Rebuilt here so the archive on disk is never stale after
# an edit to shim/wk-wasi-threads.c. Note gbuild has no dependency on files
# outside SRCDIR, so a changed shim does not by itself trigger a relink of
# anything already built — delete the binary, or `make <module>.clean`, when
# you change the shim's behaviour.
./build-shim.sh || lo_die "build-shim.sh failed"

GNUMAKE="$(lo_find_gnumake)" || lo_die "GNU Make >= 4.2 not found. Run ./preflight.sh"

# The native bootstrap has to have happened: the cross build shells out to
# wasmbridgegen, cppumaker, saxparser and friends by path.
[ -x "$LO_BUILD/workdir_for_build/LinkTarget/Executable/wasmbridgegen" ] || lo_die \
    "workdir_for_build/.../wasmbridgegen missing — run ./build-host.sh first"

TARGET="${1:-}"
goals=()
[ -n "$TARGET" ] && goals=("$TARGET")
LOG="$LO_LOGDIR/lo${TARGET:+-$TARGET}-$(date +%Y%m%d-%H%M%S).log"
echo "=== make ${TARGET:-(everything)}   (log: $LOG)"

# NO -j, same as build-host.sh: Makefile.in:87 supplies -j from
# --with-parallelism and every recursive make already carries it.
#
# PATH WITH wasi-sdk first, unlike the other two stages. Externals' libtool and
# autotools fragments call `ar`, `ranlib` and `nm` by bare name even when $AR
# is exported, and here those must be the LLVM ones — Apple's cannot read wasm
# archives. What the PATH deliberately still excludes is anything that could
# supply a wasm-opt: clang runs it as an optional post-link pass, the one on
# this machine (~/.cargo/bin/wasm-opt) cannot parse exnref, and it would
# silently corrupt the output. wasi-sdk ships none of its own, and .toolbin
# only ever contains the tools common.sh names, so with homebrew and cargo off
# the PATH the pass simply does not run. Same trap plugins/qt and
# plugins/mupdf document; the fix here is subtractive rather than a wrapper.
(
  cd "$LO_BUILD"
  env PATH="$LO_BUILD_PATH" "$GNUMAKE" ${goals[@]+"${goals[@]}"}
) 2>&1 | tee "$LOG"
rc=${PIPESTATUS[0]}
[ "$rc" -eq 0 ] || { echo "libreoffice/lo: failed (rc=$rc), see $LOG" >&2; exit 1; }

# M3's observable, when the whole build ran. RepositoryFixes.mk:37 renames the
# binary for Emscripten (soffice.js); a WASI arm should name it soffice.wasm,
# so accept either that or the bare gbuild name.
if [ -z "$TARGET" ]; then
    echo
    for cand in "$LO_BUILD/instdir/program/soffice.wasm" \
                "$LO_BUILD/instdir/program/soffice.bin" \
                "$LO_BUILD/workdir/LinkTarget/Executable/soffice.bin"; do
        if [ -f "$cand" ]; then
            echo "=== M3 candidate: $cand ($(du -h "$cand" | cut -f1))"
            if command -v wasm-tools >/dev/null 2>&1; then
                # A component, not a core module: linked wasip2-direct by
                # wasm-component-ld, so `component wit` must print a world.
                wasm-tools validate --features all "$cand" \
                    && echo "    wasm-tools validate: ok" \
                    || echo "    wasm-tools validate: FAILED"
                wasm-tools component wit "$cand" >/dev/null 2>&1 \
                    && echo "    it is a component (wasm-tools component wit parses it)" \
                    || echo "    NOT a component — check that the link went through wasm-component-ld"
            fi
            break
        fi
    done
fi
