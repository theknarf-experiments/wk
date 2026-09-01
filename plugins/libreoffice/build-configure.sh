#!/usr/bin/env bash
# Stage 1 of 3: configure LibreOffice for wasm32-wasip2. Milestone M0.
#
# Applies patches/ to src/, then runs src/autogen.sh out of tree in ./build.
# One configure run produces BOTH sides: the HOST (wasm32-wasip2) config in
# build/config_host.mk, and — via a whole second, nested configure in
# build/CONF-FOR-BUILD (configure.ac:6169-6485) — the native arm64 macOS BUILD
# config in build/config_build.mk.
#
# Success looks like: `grep '^export OS=' build/config_host.mk` printing WASI.
#
# WHY OUT OF TREE, AND WHY autogen.sh RATHER THAN configure. autogen.sh chdirs
# back to the cwd (autogen.sh:25,37), runs aclocal+autoconf THERE, and generates
# the per-module Makefile stubs a builddir!=srcdir build needs (:176-208). That
# matters beyond tidiness: configure.ac:6201 does a bare `cp configure
# CONF-FOR-BUILD`, i.e. it copies `configure` from the BUILD directory, so
# invoking $SRC/configure from an empty dir dies with "cp: configure: No such
# file or directory" reported as "Running the configure script for BUILD side
# failed" — a failure that reads like a cross-compilation problem and is not.
#
# Also note what does NOT happen here: autogen.sh:321 only prepends emconfigure
# for --host=wasm*-emscripten, so our triple takes the plain path, and
# solenv/bin/run-configure's EMMAKEN_JUST_CONFIGURE hack is gated on
# OS=EMSCRIPTEN. Neither applies. Unlike the Emscripten build we need no
# compiler wrapper at all.
#
# Idempotent: re-running reconfigures. Set WK_LO_RECONFIGURE=1 to also wipe
# build/ first (needed after changing the flag wall — configure caches).
#
# Knobs: WK_LO_RECONFIGURE=1   rm -rf build/ before configuring
#        WK_LO_GUI=1           drop --disable-gui (milestone M6 and later)
#        WK_LO_DEBUG=1         --enable-symbols --enable-sal-log (see below)
#        JOBS=N  LOGDIR=...
set -euo pipefail
cd "$(dirname "$0")"
LO_STAGE=configure
# shellcheck source=common.sh
. ./common.sh

lo_require_src

echo "=== patches"
lo_apply_patches
lo_require_patches

# The wasip2 thread shim, before anything can run make. WASI_INTEL_GCC.mk names
# the archive on every link line and hard-errors when it is missing, and that
# file is included by gbuild on the HOST side of `make cross-toolset` too — so
# it has to exist from the first make invocation, not just at link time. One C
# file, about a second, idempotent. See shim/wk-wasi-threads.c for what it
# overrides and why it cannot be a patch to src/.
./build-shim.sh || lo_die "build-shim.sh failed"

# The two host tools that are AC_MSG_ERROR, resolved here rather than left to
# PATH order. See preflight.sh for the full probe and the brew hints.
GNUMAKE="$(lo_find_gnumake)" || lo_die "GNU Make >= 4.2 not found (configure.ac:6907). Run ./preflight.sh"
GPERF="$(lo_find_gperf)"     || lo_die "gperf >= 3.1 not found (configure.ac:8201). Run ./preflight.sh"
PYTHON="$(command -v python3)" || lo_die "python3 not found"
export GNUMAKE GPERF PYTHON

lo_link_toolbin

[ "${WK_LO_RECONFIGURE:-0}" = "1" ] && rm -rf "$LO_BUILD"
mkdir -p "$LO_BUILD"

# Keep the host's .pc files out of reach. LibreOffice's configure will happily
# conclude that a *macOS* libxml2/fontconfig/freetype is a usable system library
# for a wasm32 target if pkg-config answers. Everything we need is built from
# external/ anyway, and pointing PKG_CONFIG_LIBDIR at an empty directory is the
# autotools equivalent of plugins/qt/wasip2.cmake:303 blanking
# PKG_CONFIG_EXECUTABLE. Harmless for the BUILD sub-configure, which unsets
# PKG_CONFIG_LIBDIR and PKG_CONFIG_PATH itself (configure.ac:6208).
mkdir -p "$LO_BUILD/.no-pkgconfig"
export PKG_CONFIG_LIBDIR="$LO_BUILD/.no-pkgconfig"
unset PKG_CONFIG_PATH || true

# --- the flag wall -----------------------------------------------------------
# Every flag below was checked against src/configure.ac. Order is grouped by
# what it does, not alphabetical.

args=(
  # ---- the target -----------------------------------------------------------
  # $host_os comes out as exactly "wasip2". Unpatched this hits the `*)`
  # fallthrough at configure.ac:1265; the patch adds a `wasi*)` arm beside
  # `emscripten)` at :1247 (which is why the arm must glob, so wasi/wasip1/
  # wasip3 land there too).
  "--host=$LO_HOST_TRIPLE"

  # ---- what to build --------------------------------------------------------
  # Upstream-supported (declared configure.ac:2262, implemented :4324). Clears
  # ENABLE_WASM_STRIP_ACCESSIBILITY and ENABLE_WASM_STRIP_BASIC_DRAW_MATH_IMPRESS,
  # which pulls animations/sd/sdext/slideshow/starmath into
  # RepositoryModule_host.mk and drops sw+swext and sc+scaddins+sccomp+basctl
  # entirely. The single biggest lever on link time and module size.
  "--with-wasm-module=impress"

  # Subsumes about two dozen individual --disable flags. configure.ac:3398-3430
  # sets, from this one switch: avmedia, cups, dbus, dconf, database-connectivity,
  # extensions (+integration, +update), gio, gpgmepp, ldap, libcmis, coinmp,
  # lotuswordpro, lpsolve, nss, odk, online-update, opencl, pdfimport, randr,
  # report-builder, scripting, sdremote (+bluetooth), skia, xmlhelp, zxing,
  # with_galleries=no, with_templates=no, with_x=no — and forces with_fonts=yes.
  # Do NOT list those again below; a redundant flag is a flag somebody will
  # later "clean up" without knowing it was load-bearing.
  #
  # Two consequences worth knowing. (a) enable_sdremote=no removes the Impress
  # Remote, which is where 7 of sd/'s 8 thread-creation sites live — a free win
  # for the threadless story. (b) it also defines ENABLE_WASM_STRIP_PREMULTIPLY
  # (config_host.mk.in:270 wires it straight to ENABLE_WASM_STRIP), which
  # switches vcl/headless/CairoCommon.cxx:558,775 to NON-premultiplied alpha.
  # That is the behaviour the wk compositor wants; note it beside
  # --enable-cairo-rgba below, they are two halves of the same pixel decision.
  "--enable-wasm-strip"

  # ---- no dlopen, one statically-linked binary -------------------------------
  # MUST be explicit for a WASI _os. The auto-off at configure.ac:3406 is
  # `... -o "$_os" != Emscripten || enable_dynamic_loading=no` and the forced
  # list at :3485 is iOS/Android/Emscripten only, so a WASI build would
  # otherwise fall through to :3488 and enable it. Everything downstream hangs
  # off this: the static component registration, the single-VCL-plugin rule at
  # :12561, ICU's --with-data-packaging=static, and Library targets becoming
  # .a files instead of shared objects.
  #
  # It also force-sets, at :3496-3503, enable_database_connectivity=no,
  # enable_nss=no, enable_odk=no, enable_python=no, enable_skia=no and
  # with_java=no for any non-Apple/non-Android/non-Windows target.
  "--disable-dynamic-loading"

  # Derive the UNO constructor map from the build instead of hand-maintaining
  # native-code.py's group lists. This is the Emscripten model, not the
  # iOS/Android one: postprocess/CustomTarget_components.mk -> constructors.py
  # -> services_constructors.list -> native-code.py -c -> component_maps.cxx.
  # Requires DISABLE_DYNLOADING (configure.ac:3510) and constrains --with-locales
  # to unset/en/ALL (:3511). 1064 of LibreOffice's 1189 implementations already
  # carry constructor=, and the 125 that do not are Java/Python loaders, tests,
  # Windows/Base-only components, plus i18npool and svtools — which are exactly
  # the two entries hardcoded in native-code.py's factory list.
  "--enable-customtarget-components"

  # NOT --enable-mergelibs, and this is a correction to the original brief:
  # upstream's wasm path does not use it (distro-configs/LibreOfficeWASM32.conf
  # is five lines and never mentions it; the emscripten) arm never sets it).
  # It buys nothing under DISABLE_DYNLOADING, where every Library is already a
  # .a, and it actively breaks things: Library.mk:172 renames merged libraries
  # to "merged", both i18npool and svt are in gb_MERGE_LIBRARY_LIST, and
  # native-code.py:21-24 hardcodes libi18npoollo.a / libsvtlo.a in its factory
  # map. 76 services would fail with CannotActivateFactoryException —
  # partially and confusingly, because constructor-based implementations ignore
  # the uri and would keep working.
  #
  # `configure --help` only advertises the --enable form
  # (--enable-mergelibs=yes/no/more, configure.ac:1667). --disable-mergelibs is
  # the same AC_ARG_ENABLE with the value "no", which configure.ac:15805
  # handles; it is not an undocumented spelling, just an unlisted one.
  "--disable-mergelibs"

  # ---- pixels ---------------------------------------------------------------
  # Makes the svp/headless cairo surface byte order [r,g,b,a] instead of the
  # little-endian ARGB32 default [b,g,r,a]: include/vcl/CairoFormats.hxx:36-42
  # under ENABLE_CAIRO_RGBA gives SVP_CAIRO_RED 0 / GREEN 1 / BLUE 2 / ALPHA 3.
  # That is exactly what plugins/gfx-compat/wkgfx.h documents wkgfx_present as
  # taking ("RGBA8: bytes [r, g, b, a] in memory order"), so the wk VCL
  # backend's present becomes a blit rather than a per-pixel swizzle of an 8 MB
  # buffer every frame.
  #
  # It is a real library change, not just VCL accessors:
  # external/cairo/UnpackedTarball_cairo.mk:37 applies cairo.GL_RGBA.patch,
  # which remaps CAIRO_FORMAT_ARGB32 onto PIXMAN_a8b8g8r8 inside the bundled
  # cairo. Upstream's own rationale is LibreOffice Online's canvas ImageData —
  # the same problem we have. Incompatible with --with-system-cairo
  # (configure.ac:2773), which we are not using. This is the exact analogue of
  # plugins/netsurf picking NSFB_FMT_XBGR8888 so update() is a memcpy.
  "--enable-cairo-rgba"

  # ---- externals and the build machine ---------------------------------------
  # Keep the ~149 external tarballs outside build/, so `rm -rf build` (or
  # WK_LO_RECONFIGURE=1) does not re-download hundreds of megabytes.
  "--with-external-tar=$LO_TARBALLS"

  # LibreOffice adds `-j $(PARALLELISM)` itself (Makefile.in:87) and passes it
  # down into external sub-builds, so the build scripts must NOT also pass -j.
  "--with-parallelism=$LO_JOBS"

  # Cosmetic, but it makes an About box or a crash log identifiable as ours.
  "--with-vendor=wk"

  # Redundant three times over (the DISABLE_DYNLOADING block at :3502 and
  # --disable-scripting both force it), but explicit because the failure mode
  # is expensive: with ENABLE_JAVA non-empty, configure.ac:6234 starts demanding
  # --with-jdk-home and forwarding Java options to the BUILD side.
  "--without-java"

  # The BUILD-side (native macOS) sub-configure. This string is appended LAST
  # on its command line (configure.ac:6309-6314), after sub_conf_defaults and
  # after every derived option, so it wins every conflict.
  #
  # Why these four: configure.ac:6252 forwards exactly them, but only
  # `if test "$_os" = "Emscripten"`. A WASI host does not qualify, so we pass
  # them by hand. The reason they exist at all is that the build side's choices
  # propagate into the cross build — SYSTEM_LIBXML and SYSTEM_LIBXSLT are in
  # DIRECT_FOR_BUILD_SETTINGS (configure.ac:6379) and get re-exported as
  # *_FOR_BUILD — so a native build using macOS's libxml2 while the cross build
  # uses the internal one is a mismatch waiting to surface in a generated tool.
  #
  # HONEST CAVEAT: it is not proven that a WASI host needs all four; it may be
  # Emscripten-specific. It is the conservative copy. If the native bootstrap
  # turns out to be dominated by building libxml2/freetype/fontconfig/zlib
  # natively, dropping them is a legitimate experiment — but re-run M1 fully
  # after doing so, do not trust an incremental build.
  #
  # Assembled as ONE string in $build_opts below and appended once: this is an
  # AC_ARG_WITH, so passing --with-build-platform-configure-options twice makes
  # the second occurrence REPLACE the first rather than add to it.
)

build_opts="--without-system-libxml --without-system-fontconfig --without-system-freetype --without-system-zlib"
# The BUILD-side sub-configure runs the same bogus-pkg-config check and does NOT
# inherit the host side's options, so the approval has to be given twice. The
# host-side one is added to args below; this is its BUILD-side twin, and leaving
# it out fails the whole configure at the very end with the check's message
# naming .toolbin/pkg-config.
build_opts="$build_opts --enable-bogus-pkg-config"

# --disable-gui is the STAGING point, not the destination (PORTING.md decision
# 10). It is only reachable at all because the WASI host arm copies Emscripten's
# `using_x11=yes` fib — configure.ac:5949 makes --disable-gui itself require
# using_x11=yes. Keep it through M0-M5 (headless .pptx->PDF, svp->PNG); drop it
# at M6, when vcl/wk/ exists and the arm sets R="wk", because configure.ac:12561
# refuses DISABLE_DYNLOADING + a GUI unless exactly one VCL plugin is
# registered. Switching exercises a materially different module graph, so
# expect to re-debug configure once when you do.
if [ "${WK_LO_GUI:-0}" = "1" ]; then
    echo "=== GUI build requested (WK_LO_GUI=1): omitting --disable-gui"
else
    args+=("--disable-gui")
fi

# Symbols and sal logging. Off by default because they inflate an already very
# large module, but worth turning on for M2-M4: on wasip2 every reachable
# condition-variable wait is an abort() rather than a hang, so a build with
# names in the backtrace is how the threadless tail gets enumerated at all.
# --enable-sal-log additionally turns on the SAL_INFO in
# cppuhelper/source/shlib.cxx, which is where the first two expected failures
# live (CannotActivateFactoryException "unknown constructor name", and a
# DeploymentException on the services.rdb path).
#
# Two settings, because the two costs are not the same. WK_LO_DEBUG=log gets the
# logging alone, which is what you want once the thing links: DWARF for a module
# this size is many hundreds of megabytes for the linker to carry and write.
# WK_LO_DEBUG=1 gets both.
case "${WK_LO_DEBUG:-0}" in
    1)   args+=("--enable-symbols" "--enable-sal-log") ;;
    log) args+=("--enable-sal-log") ;;
esac

# ccache, if the machine has it, on BOTH sides. The forwarding is documented at
# static/README.wasm.md:130-137 and implemented at configure.ac:6238. Without
# it every flag experiment re-pays the whole native bootstrap.
if command -v ccache >/dev/null 2>&1; then
    args+=("--enable-ccache")
    build_opts="$build_opts --enable-ccache"
else
    echo "=== note: ccache absent; every reconfigure will re-pay the native bootstrap"
fi

# Approving our OWN pkg-config, not the system's. configure.ac:7178 refuses one
# whose origin it cannot place, and Homebrew's pkgconf always reports two
# virtual packages (pkg-config, pkgconf) that :7175's grep does not filter — so
# the "no packages in the default searchpath" escape never fires here even with
# the path emptied. toolwrap/pkg-config IS emptied: PKG_CONFIG_LIBDIR points at
# an empty directory, so what this approves is a pkg-config that can see
# nothing, which is exactly the property the check defends. The alternative,
# no pkg-config at all, kills liblangtag's own configure during
# `make cross-toolset`.
args+=("--enable-bogus-pkg-config")

args+=("--with-build-platform-configure-options=$build_opts")

# ---- deliberately NOT passed -------------------------------------------------
#  --disable-pch        already the default off Windows (configure.ac:6774), so
#                       passing it says nothing. Worth REVISITING once M3 links:
#                       PCH is a large fraction of LibreOffice's build time, and
#                       the hard error at :6777 is scoped to _os=Emscripten
#                       ("missing Sj/Lj support with nEH in clang") — whether
#                       clang 23 with -mllvm -wasm-enable-sjlj can do PCH is
#                       untested. Try --enable-pch=base on a spare evening.
#  --with-locales=en    the default (unset) is already one of the three values
#                       --enable-customtarget-components permits. `en` would
#                       select i18npool.en.component (7 implementations instead
#                       of 67) and is a genuine size lever — but it is a change
#                       with an unmeasured blast radius, so not on the first
#                       build. Trim locales at the VFS layer instead.
#  --with-package-format the Emscripten value drives
#                       instsetoo_native/CustomTarget_emscripten-install.mk,
#                       which builds soffice.data + qtloader.js. We package
#                       instdir/ into an OCI layer ourselves. Leaving PKGFORMAT
#                       empty is the intent; whether that yields a usable
#                       instdir/ is unverified (PORTING.md, unknowns).
#  --disable-cups etc.  subsumed by --enable-wasm-strip; see above.
#  CFLAGS/CXXFLAGS/     NEVER set these. gbuild treats a non-empty CFLAGS as a
#  LDFLAGS              REPLACEMENT for its own -g/-O handling
#                       (LinkTarget.mk:66,68,72), not an addition. The compile
#                       flags ride in CC/CXX (common.sh explains why that is
#                       also the only route that reaches the externals); the
#                       link flags belong in WASI_INTEL_GCC.mk.

# --- run ---------------------------------------------------------------------
LOG="$LO_LOGDIR/configure-$(date +%Y%m%d-%H%M%S).log"
echo "=== configure"
printf '    %s\n' "${args[@]}"
echo "    log: $LOG"

# PATH WITHOUT wasi-sdk, on purpose. Every cross tool is passed by absolute
# path below, and configure.ac:6205-6208 unsets CC/CXX/AR/NM/RANLIB/STRIP/
# OBJDUMP before running the BUILD-side sub-configure, which then autodetects
# from PATH. With wasi-sdk on PATH that sub-configure picks a wasm32 clang for
# the NATIVE bootstrap and the failure surfaces hours later, in the middle of
# `make cross-toolset`, as a link error about a native tool.
# The toolchain goes in as ARGUMENTS, not as environment. autogen.sh records
# its argv in autogen.lastrun and replays it whenever anything re-runs
# configure — and the top-level Makefile does exactly that, unprompted, as soon
# as configure.ac is newer than config_host.mk, which applying a patch makes
# true. Passed as environment (which is how this script did it until it bit),
# the assignments are absent from that replay, so make silently reconfigures
# the CROSS build with plain `gcc` and the next compile fails somewhere with no
# hint of why. autoconf treats `CC=...` on the command line as a precious
# variable, so this is its supported route, not a trick.
args+=(
  "CC=$LO_CC"
  "CXX=$LO_CXX"
  "AR=$LO_AR"
  "NM=$LO_NM"
  "RANLIB=$LO_RANLIB"
  "STRIP=$LO_STRIP"
  "OBJDUMP=$LO_OBJDUMP"
  "CC_FOR_BUILD=$LO_CC_FOR_BUILD"
  "CXX_FOR_BUILD=$LO_CXX_FOR_BUILD"
  "GNUMAKE=$GNUMAKE"
  "GPERF=$GPERF"
  "PYTHON=$PYTHON"
)

set +e
(
  cd "$LO_BUILD"
  env PATH="$LO_HOST_PATH" \
      PKG_CONFIG_LIBDIR="$PKG_CONFIG_LIBDIR" \
      "$LO_SRC/autogen.sh" "${args[@]}"
) 2>&1 | tee "$LOG"
# tee eats the exit status; PIPESTATUS is why this script is bash, not sh, and
# why `set -e` is off across the pipeline (with pipefail it would exit here
# before the diagnostic below could run).
rc=${PIPESTATUS[0]}
set -e
[ "$rc" -eq 0 ] || lo_die "configure failed (rc=$rc), see $LOG"

os="$(sed -n 's/^export OS=//p' "$LO_BUILD/config_host.mk" | head -1)"
echo
echo "=== configured: OS=$os  (M0 wants WASI)"
echo "    host  config: $LO_BUILD/config_host.mk"
echo "    build config: $LO_BUILD/config_build.mk"
echo "    next: ./build-host.sh   (the native bootstrap; measure it with time)"
