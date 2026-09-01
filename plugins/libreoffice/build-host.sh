#!/usr/bin/env bash
# Stage 2 of 3: the NATIVE bootstrap. Milestone M1.
#
# `make cross-toolset` (Makefile.in:314-318) fetches the external tarballs for
# both sides and then runs `make gb_Side=build -f Makefile.gbuild build-tools`,
# producing the ~26 code generators that must run on arm64 macOS during the
# cross build. Nothing here compiles a single byte of wasm.
#
# WHAT IT ACTUALLY BUILDS, because "native LibreOffice bootstrap" sounds much
# worse than it is. solenv/gbuild/extensions/pre_BuildTools.mk:13-52 lists
# gb_BUILD_TOOLS: bestreversemap, cfgex, cpp, cppumaker, gencoll_rule,
# genconv_dict, helpex, idxdict, javamaker, makedepend, propex, saxparser,
# svidl, treex, ulfex, unoidl-check, unoidl-write, xrmex — plus, when the WASI
# host pushes the EMSCRIPTEN token into BUILD_TYPE_FOR_HOST, embindmaker and
# **wasmbridgegen**. Their transitive library set is essentially sal + cppu +
# cppuhelper + codemaker/registry/store/unoidl + ICU: on the order of 300-400
# translation units, not thousands. climaker is MSC-only and gengal is off
# because --enable-wasm-strip clears with_galleries.
#
# wasmbridgegen is the one that matters. bridges/CustomTarget_gcc3_wasm.mk:18-28
# runs it to generate generated-cxx.cxx, generated-asm.s and the exports list
# for the UNO C++ bridge's synthesized vtable thunks. No wasmbridgegen, no
# bridge, no LibreOffice. It is also where this port's replacement for
# upstream's EM_JS symbol lookup will be emitted (PORTING.md).
#
# ESTIMATE, NOT MEASUREMENT: 20-40 minutes on this 10-core box without ccache,
# dominated by ICU's native build (external/icu/ExternalProject_icu.mk:85 makes
# the cross build depend on a native --with-cross-build tree) and by unpacking
# tarballs single-threaded. MEASURE IT with `time` on the first real run and
# put the number in PORTING.md; nobody has ever run this.
#
# Long. Run it detached and tail ./logs rather than under a foreground timeout.
#
# Idempotent: gbuild is incremental, so re-running after a successful run is
# close to a no-op.
#
# Knobs: JOBS=N (only affects the tarball fetch; -j comes from
#        --with-parallelism, see below)  LOGDIR=...
set -uo pipefail
cd "$(dirname "$0")"
LO_STAGE=host
# shellcheck source=common.sh
. ./common.sh

lo_require_src
lo_require_configured
lo_link_toolbin

GNUMAKE="$(lo_find_gnumake)" || lo_die "GNU Make >= 4.2 not found. Run ./preflight.sh"

LOG="$LO_LOGDIR/host-$(date +%Y%m%d-%H%M%S).log"
echo "=== make cross-toolset   (native bootstrap; log: $LOG)"

# NO -j. Makefile.in:87 defines PARALLELISM_OPTION := -j $(PARALLELISM) and
# every recursive $(MAKE) already carries it, from --with-parallelism. Adding
# our own would oversubscribe a build that is already nesting make three deep.
#
# PATH WITHOUT wasi-sdk. This stage produces arm64 macOS executables, and
# `which -a clang` on this machine resolves wasi-sdk's wasm32-wasip1 clang
# before Apple's. config_build.mk names the native compiler explicitly (it came
# from CC_FOR_BUILD at configure time), but the externals built for the build
# side run their own configure scripts, and one of those picking up a wasm
# cross compiler is a failure that surfaces as an unreadable link error an hour
# in. Keeping the SDK off PATH removes the possibility rather than relying on
# every external to honour $CC.
(
  cd "$LO_BUILD"
  env PATH="$LO_HOST_PATH" "$GNUMAKE" cross-toolset
) 2>&1 | tee "$LOG"
rc=${PIPESTATUS[0]}
[ "$rc" -eq 0 ] || { echo "libreoffice/host: failed (rc=$rc), see $LOG" >&2; exit 1; }

# The observable for M1. gb_Side=build output lives under workdir_for_build/
# (configure.ac:6446-6455 rewrites WORKDIR to workdir_for_build for the build
# side), and the executables under LinkTarget/Executable there.
bridgegen="$LO_BUILD/workdir_for_build/LinkTarget/Executable/wasmbridgegen"
echo
if [ -x "$bridgegen" ]; then
    echo "=== M1: $bridgegen exists"
else
    cat >&2 <<EOF
=== M1 NOT reached: $bridgegen is missing.

  Four places gate the wasm bootstrap tools on the literal string EMSCRIPTEN in
  BUILD_TYPE_FOR_HOST:
    solenv/gbuild/extensions/pre_BuildTools.mk:19
    Repository.mk:36
    RepositoryModule_build.mk:50
    static/Module_static.mk:30
  and configure.ac:1549-1550 is the only producer
  (BUILD_TYPE="\$BUILD_TYPE EMSCRIPTEN", inside \`if test "\$_os" = "Emscripten"\`).
  The WASI host arm must either push the same token or the four sites must
  learn a new one. Prefer the latter so embindmaker — Emscripten-only JS
  bindings — can be dropped. See PORTING.md.
EOF
    exit 1
fi
echo "    next: ./build-lo.sh   (the cross build; this is the long one)"
