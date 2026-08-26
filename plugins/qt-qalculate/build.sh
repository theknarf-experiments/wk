#!/usr/bin/env bash
# Qalculate! 5.12.0 (the Qt GUI) as a wk node.
#
# Produces ./qalculate-qt.wasm: a wasm32-wasip2 COMPONENT that imports
# wasi:surface/graphics-context/frame-buffer and paints its windows into the
# single RGBA8 surface wk gives a node, through the wk QPA plugin.
#
# WHAT THIS SCRIPT IS: four C/C++ cross-builds, one Qt module cross-build, and
# a link.
#
#   1. gmp 6.3.0          -> ./sysroot
#   2. mpfr 4.2.2         -> ./sysroot
#   3. libxml2 2.13.8     -> ./sysroot
#   4. libqalculate 5.12.0-> ./sysroot   (the actual calculator; the Qt app is
#                                         a front end and nothing more)
#   5. qtsvg 6.8.4        -> ./sysroot   (Qt6::Svg + the qsvg imageformat and
#                                         iconengine plugins; icons.qrc is 53
#                                         SVGs and qalculateqtsettings.h's
#                                         LOAD_APP_ICON only ever names .svg)
#   6. the app            -> ./qalculate-qt.wasm
#
# WHY A PORT-LOCAL ./sysroot. plugins/qt/sysroot holds qtbase and the wk QPA
# plugin, built by plugins/qt/build-qtbase.sh and build-qpa.sh. Everything
# above installs HERE instead, so this plugin never writes into another
# plugin's tree -- several ports grow their own module set from one shared
# qtbase, exactly as plugins/qt-torrentfileeditor103 does for qt5compat/qtsvg.
# Both prefixes go on CMAKE_PREFIX_PATH and CMAKE_FIND_ROOT_PATH.
#
# WHY THIS APP IS DIFFERENT FROM THE OTHER Qt PORTS, in two sentences:
#
#   * upstream ships NO CMakeLists.txt. qalculate-qt v5.12.0 is qmake-only
#     (`qalculate-qt.pro`), and wk's Qt has no WASI qmake mkspec -- it is a
#     genuine CMake `WASI` platform. So this port supplies its own
#     cmake/CMakeLists.txt, staged into the fetched tree, plus a check below
#     that the .pro's SOURCES still match what CMake globs (upstream adding a
#     file must break the build loudly, not silently drop a translation unit).
#   * libqalculate dispatches every timed calculation through pthreads, and
#     wasi-libc's pthread_create returns ENOTSUP. Unpatched, the GUI displays
#     "aborted" for EVERY expression -- measured, not guessed. See
#     patches/libqalculate-0002-wasi-inline-threads.patch; it is the load-
#     bearing patch in this port and it also covers the Qt front end's own two
#     Thread subclasses.
#
# EVERYTHING THE PORT INHERITS FROM plugins/qt (read plugins/qt/wasip2.cmake's
# header, it is the primary document):
#
#   * the exnref EH flag set (-fwasm-exceptions -mllvm -wasm-enable-sjlj
#     -mllvm -wasm-use-legacy-eh=false). wasmtime runs with the exception
#     proposal ON and REJECTS wasi-sdk's default legacy encoding, at
#     instantiate time, for the WHOLE component -- so one stray object poisons
#     the binary and the error points nowhere near it. The toolchain file puts
#     these on every object of every language, and the autoconf builds above
#     are given the SAME flags by hand so their objects match;
#   * the wasm-opt trap: clang runs wasm-opt as an optional post-link pass and
#     the wasm-opt on PATH cannot parse exnref. Every cmake/ninja/configure
#     call here runs under a PATH that omits it;
#   * no threads (FEATURE_thread=OFF in qtbase);
#   * no dlopen: the wk QPA plugin and the qsvg plugins are STATIC and named
#     with Q_IMPORT_PLUGIN in main.cpp, which is what patches/0001 adds.
#
# FONTS. Qt 6 ships none and a wk node has no host font directory: with no
# font the app runs, QFontDatabase is empty, and every string renders as
# nothing at all. So this script STAGES one TTF into ./fonts/ and the
# CMakeLists compiles it in as a Qt resource under :/fonts, which
# QWkFontDatabase falls back to. The staged copy is gitignored -- it is
# somebody else's font.
#
# Knobs: WK_QALC_STAGES="deps qtsvg app"   JOBS=N   LOGDIR=...
#        WK_QALC_RECONFIGURE=1   QT_HOST_PATH=...
#
# Long: budget 15-25 minutes cold (the deps are ~6 of it, qtsvg ~3, the app
# ~4). Run it detached and tail ./logs.
set -euo pipefail
cd "$(dirname "$0")"

# --- toolchain guard (same shape as plugins/qt-torrentfileeditor103) --------
MISE_SDK="$HOME/.local/share/mise/installs/github-web-assembly-wasi-sdk/wasi-sdk-34-rc.2"
WASI_SDK="${WASI_SDK:-$([ -d "$MISE_SDK" ] && echo "$MISE_SDK" || echo "$HOME/wasi-sdk")}"
EXPECT="wasi-sdk-34-rc.2"
case "$WASI_SDK" in
    *"$EXPECT"*) ;;
    *) echo "qt-qalculate: expected $EXPECT (set WASI_SDK), got: $WASI_SDK" >&2; exit 1 ;;
esac

QT_VER=6.8.4
QT_SERIES=6.8
# Both Qalculate projects release together and version-track: v5.12.0 of the
# library and of the Qt GUI were tagged 8 seconds apart on 2026-07-13. Bump
# them as a pair or not at all.
QALC_VER=5.12.0
GMP_VER=6.3.0
MPFR_VER=4.2.2
XML2_VER=2.13.8
XML2_SERIES=2.13

HERE="$PWD"
QTPLUGIN="$PWD/../qt"                 # the shared Qt port: qtbase + the wk QPA
QTBASE_SYSROOT="$QTPLUGIN/sysroot"
HOST_PREFIX="${QT_HOST_PATH:-$QTPLUGIN/host}"
GFXCOMPAT="$PWD/../gfx-compat"
CLIPCOMPAT="$PWD/../clipboard-compat"

SRCDIR="$PWD/src"
TARBALLS="$PWD/tarballs"
PATCHDIR="$PWD/patches"
SYSROOT="$PWD/sysroot"                # OUR prefix: gmp/mpfr/libxml2/libqalculate + qtsvg
BUILD="$PWD/build"
GEN="$PWD/gen"                        # our own wit-bindgen output
FONTDIR="$PWD/fonts"
LOGDIR="${LOGDIR:-$PWD/logs}"
JOBS="${JOBS:-$(sysctl -n hw.ncpu 2>/dev/null || nproc)}"
BUILD_PATH="$WASI_SDK/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"
mkdir -p "$SRCDIR" "$TARBALLS" "$LOGDIR" "$BUILD" "$GEN" "$FONTDIR" "$SYSROOT"

# --- preflight --------------------------------------------------------------
if [ ! -f "$QTBASE_SYSROOT/lib/libQt6Widgets.a" ]; then
    echo "qt-qalculate: no cross Qt in $QTBASE_SYSROOT" >&2
    echo "  run plugins/qt/build-qtbase.sh first (it is the long one)." >&2
    exit 1
fi
if [ ! -f "$QTBASE_SYSROOT/lib/libqwk.a" ]; then
    echo "qt-qalculate: no wk QPA plugin at $QTBASE_SYSROOT/lib/libqwk.a" >&2
    echo "  run plugins/qt/build-qpa.sh first. Without it the app links but" >&2
    echo "  QApplication aborts with 'no Qt platform plugin could be" >&2
    echo "  initialized' and an EMPTY plugin list." >&2
    exit 1
fi
if [ ! -x "$HOST_PREFIX/libexec/moc" ] && [ ! -x "$HOST_PREFIX/bin/moc" ]; then
    echo "qt-qalculate: no host Qt at $HOST_PREFIX -- run plugins/qt/build-host.sh" >&2
    exit 1
fi

# --- patches ----------------------------------------------------------------
# patches/<prefix>-NNNN-*.patch, applied -p1 in order at that tree's root. See
# patches/README.md for the ledger and the reason each one exists.
#
# Idempotency is a STAMP FILE, not a per-patch `git apply --reverse --check`:
# once two patches touch nearby lines the reverse-check silently stops working
# (the earlier one no longer reverse-applies), and a stamp is unambiguous.
# An unstamped tree is either pristine-from-tarball or the wreckage of a failed
# patch run; either way it is thrown away and extracted again.
apply_patches() {
    local tree="$1" prefix="$2"
    [ -f "$tree/.wk-patched" ] && { echo "  patches: already applied to $(basename "$tree")"; return 0; }
    for p in "$PATCHDIR/$prefix"-*.patch; do
        [ -e "$p" ] || continue
        echo "  patch: $(basename "$p")"
        git -C "$tree" apply "$p"
    done
    touch "$tree/.wk-patched"
}

discard_unpatched_tree() {
    local tree="$1"
    if [ -d "$tree" ] && [ ! -f "$tree/.wk-patched" ]; then
        echo "  re-extracting $(basename "$tree") (no .wk-patched stamp)"
        rm -rf "$tree"
    fi
}

# fetch_tar <url> <tarball-name> <extracted-dir-name>
# Upstream is FETCHED, never vendored. The .part rename means an interrupted
# download can never be mistaken for a complete one.
fetch_tar() {
    local url="$1" tarname="$2" dir="$SRCDIR/$3"
    discard_unpatched_tree "$dir"
    [ -d "$dir" ] && return 0
    if [ ! -f "$TARBALLS/$tarname" ]; then
        echo "fetching $tarname..."
        curl -fsSL --retry 3 -o "$TARBALLS/$tarname.part" "$url"
        mv "$TARBALLS/$tarname.part" "$TARBALLS/$tarname"
    fi
    echo "extracting $tarname..."
    tar xf "$TARBALLS/$tarname" -C "$SRCDIR"
}

fetch_qt_module() {
    local name="$1"
    local dir="$SRCDIR/$name-everywhere-src-$QT_VER"
    local tar_path="$TARBALLS/$name-everywhere-opensource-src-$QT_VER.tar.xz"
    discard_unpatched_tree "$dir"
    [ -d "$dir" ] && return 0
    if [ ! -f "$tar_path" ]; then
        echo "fetching $name $QT_VER..."
        curl -fsSL --retry 3 -o "$tar_path.part" \
            "https://download.qt.io/archive/qt/$QT_SERIES/$QT_VER/submodules/$(basename "$tar_path")"
        mv "$tar_path.part" "$tar_path"
    fi
    echo "extracting $name $QT_VER..."
    tar xJf "$tar_path" -C "$SRCDIR"
}

# --- the autoconf cross environment -----------------------------------------
#
# These flags are NOT a guess: they are plugins/qt/wasip2.cmake's own
# CMAKE_C_FLAGS_INIT / CMAKE_EXE_LINKER_FLAGS_INIT written out by hand, because
# the four libraries below use autoconf and never see the toolchain file. They
# have to match EXACTLY -- an object compiled without -wasm-use-legacy-eh=false
# emits the legacy EH encoding, and wasmtime then refuses to instantiate the
# whole component with an error that names no file.
#
# The four _WASI_EMULATED_* macros are compile-time as well as link-time:
# wasi-libc's <signal.h> and <sys/mman.h> are #error headers without them.
export CC="$WASI_SDK/bin/clang --target=wasm32-wasip2"
export CXX="$WASI_SDK/bin/clang++ --target=wasm32-wasip2"
export AR="$WASI_SDK/bin/llvm-ar"
export RANLIB="$WASI_SDK/bin/llvm-ranlib"
export NM="$WASI_SDK/bin/llvm-nm"
export STRIP="$WASI_SDK/bin/llvm-strip"
WK_EH="-fwasm-exceptions -mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false"
WK_EMU="-D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_MMAN -D_WASI_EMULATED_PROCESS_CLOCKS -D_WASI_EMULATED_GETPID"
export CFLAGS="-O2 $WK_EMU $WK_EH"
export CXXFLAGS="-O2 $WK_EMU $WK_EH"
export LDFLAGS="-lunwind -lsetjmp -lwasi-emulated-signal -lwasi-emulated-mman -lwasi-emulated-process-clocks -lwasi-emulated-getpid"
# No host .pc file may ever answer a cross query. libqalculate's configure runs
# PKG_CHECK_MODULES for libxml-2.0 unconditionally, so pkg-config must be
# pointed at OUR prefix and nowhere else -- LIBDIR (not PATH) is the variable
# that REPLACES the default search list rather than prepending to it.
export PKG_CONFIG_LIBDIR="$SYSROOT/lib/pkgconfig"
export PKG_CONFIG_PATH="$SYSROOT/lib/pkgconfig"

# autoconf_build <name> <srcdir-name> <marker-lib> -- <configure args...>
autoconf_build() {
    local name="$1" srcname="$2" marker="$3"; shift 3; [ "$1" = "--" ] && shift
    local src="$SRCDIR/$srcname"
    local bld="$BUILD/$name"
    local log="$LOGDIR/$name.log"
    if [ -f "$SYSROOT/lib/$marker" ] && [ -z "${WK_QALC_RECONFIGURE:-}" ]; then
        echo "=== $name already installed ($marker)"
        return 0
    fi
    echo "=== configuring $name for wasm32-wasip2 (log: $log)"
    mkdir -p "$bld"
    ( cd "$bld" && env PATH="$BUILD_PATH" "$src/configure" \
        --host=wasm32-unknown-wasi --prefix="$SYSROOT" \
        --disable-shared --enable-static "$@" ) 2>&1 | tee "$log"
    echo "=== make $name"
    env PATH="$BUILD_PATH" make -C "$bld" -j"$JOBS" 2>&1 | tee -a "$log"
    env PATH="$BUILD_PATH" make -C "$bld" install 2>&1 | tee -a "$log"
}

# --- a Qt module cross-build (verbatim from qt-torrentfileeditor103) --------
# WARNINGS_ARE_ERRORS=OFF: wasi-sdk 34-rc.2's clang reports itself as Clang 23,
# which Qt 6.8.4 predates, and Qt's developer defaults promote its new
# diagnostics to errors.
build_qt_module() {
    local name="$1"; shift
    local src="$SRCDIR/$name-everywhere-src-$QT_VER"
    local bld="$BUILD/$name"
    local log="$LOGDIR/$name.log"
    # build.ninja, not CMakeCache.txt: a configure that FAILED still leaves a
    # cache behind, and keying off it makes the next run skip configure and
    # die with "ninja: error: loading 'build.ninja'".
    if [ ! -f "$bld/build.ninja" ] || [ -n "${WK_QALC_RECONFIGURE:-}" ]; then
        echo "=== configuring $name for wasm32-wasip2 (log: $log)"
        env PATH="$BUILD_PATH" cmake -G Ninja -S "$src" -B "$bld" \
            -DCMAKE_TOOLCHAIN_FILE="$QTPLUGIN/wasip2.cmake" \
            -DWASI_SDK_PREFIX="$WASI_SDK" \
            -DCMAKE_FIND_ROOT_PATH="$QTBASE_SYSROOT;$SYSROOT" \
            -DCMAKE_PREFIX_PATH="$QTBASE_SYSROOT;$SYSROOT" \
            -DQT_HOST_PATH="$HOST_PREFIX" \
            -DCMAKE_INSTALL_PREFIX="$SYSROOT" \
            -DCMAKE_BUILD_TYPE=Release \
            -DBUILD_SHARED_LIBS=OFF \
            -DQT_BUILD_EXAMPLES=OFF \
            -DQT_BUILD_TESTS=OFF \
            -DQT_BUILD_BENCHMARKS=OFF \
            -DWARNINGS_ARE_ERRORS=OFF \
            "$@" 2>&1 | tee "$log"
    else
        echo "=== $name already configured in $bld (WK_QALC_RECONFIGURE=1 to redo)"
    fi
    echo "=== ninja $name"
    env PATH="$BUILD_PATH" cmake --build "$bld" --parallel "$JOBS" 2>&1 | tee -a "$log"
    env PATH="$BUILD_PATH" cmake --install "$bld" 2>&1 | tee -a "$log"
}

QALC_SRC="$SRCDIR/qalculate-qt-$QALC_VER"
LQ_SRC="$SRCDIR/libqalculate-$QALC_VER"

STAGES="${WK_QALC_STAGES:-deps qtsvg app}"

for stage in $STAGES; do
case "$stage" in

deps)
    # --- gmp ------------------------------------------------------------
    # --disable-assembly: GMP's wasm32 support is the generic C mpn path and
    # nothing else -- there is no wasm assembly in gmp 6.3.0, and leaving the
    # detection on makes configure pick a host fallback that does not build.
    # Correct but slower than native; fine for a calculator, and the honest
    # cost is very large bignum work (long factorials, 2^100000).
    # --enable-cxx=no: gmpxx is unused; libqalculate talks to the C API.
    fetch_tar "https://gmplib.org/download/gmp/gmp-$GMP_VER.tar.xz" \
        "gmp-$GMP_VER.tar.xz" "gmp-$GMP_VER"
    touch "$SRCDIR/gmp-$GMP_VER/.wk-patched"   # no patches needed; stamp it so
                                               # the tree is not re-extracted
    autoconf_build gmp "gmp-$GMP_VER" libgmp.a -- --disable-assembly --enable-cxx=no

    # --- mpfr -----------------------------------------------------------
    # --disable-thread-safe: MPFR's TLS caches want __thread, and this build
    # has one thread by construction. Also drops the pthread_once in its
    # cache initialisation, which on wasip2 is a stub.
    fetch_tar "https://www.mpfr.org/mpfr-$MPFR_VER/mpfr-$MPFR_VER.tar.xz" \
        "mpfr-$MPFR_VER.tar.xz" "mpfr-$MPFR_VER"
    touch "$SRCDIR/mpfr-$MPFR_VER/.wk-patched"
    autoconf_build mpfr "mpfr-$MPFR_VER" libmpfr.a -- \
        --with-gmp="$SYSROOT" --disable-thread-safe

    # --- libxml2 --------------------------------------------------------
    # Everything optional is off. libqalculate uses libxml2 ONLY to parse the
    # unit/function/element definition XML that --enable-compiled-definitions
    # embeds in the binary as C string literals -- there is no file to read,
    # no URL to fetch, no catalog to consult and nothing to compress. Leaving
    # --with-http on would additionally drag in socket code this node has no
    # use for.
    fetch_tar "https://download.gnome.org/sources/libxml2/$XML2_SERIES/libxml2-$XML2_VER.tar.xz" \
        "libxml2-$XML2_VER.tar.xz" "libxml2-$XML2_VER"
    touch "$SRCDIR/libxml2-$XML2_VER/.wk-patched"
    autoconf_build libxml2 "libxml2-$XML2_VER" libxml2.a -- \
        --without-python --without-http --without-zlib --without-lzma \
        --without-iconv --without-icu --without-threads --without-modules \
        --without-catalog --without-debug

    # --- libqalculate ---------------------------------------------------
    # The actual calculator. Everything the GUI shows -- the unit conversions,
    # the interval arithmetic, the symbolic solver, the 1000-odd units and
    # functions -- is this library; qalculate-qt is a front end.
    #
    # --enable-compiled-definitions is what makes it a self-contained node:
    # libqalculate/Makefile.am sed-escapes data/{units,functions,variables,
    # elements,currencies,planets,prefixes,datasets}.xml into a definitions.c
    # of C string literals, so nothing has to exist on the node's vfs at
    # runtime. (With --disable-nls the .xml.in -> .xml step is a plain cp, so
    # no msgfmt/ITS host tooling is needed either.)
    #
    # --without-libcurl: currency exchange rates. plugins/curl already has a
    # wasm32-wasip2 libcurl and blocking sockets work over the fabric, so this
    # is a later milestone rather than a dead end -- but fetchExchangeRates is
    # driven by a Thread subclass and the rates would need a real HTTPS peer.
    # --without-icu: only utf8_strdown() uses it (util.cc:874), i.e.
    # case-insensitive matching of NON-ASCII identifier names. ICU 76 for
    # wasip2 does exist in this repo (plugins/bun/native/libicu*.a), but it is
    # another plugin's build output with no installed prefix, and depending on
    # it would couple this port to Bun's build tree for one string function.
    # --without-gnuplot-call: there is no gnuplot to exec (and no exec).
    # --disable-nls: no gettext catalogs on the node; support.h:11-33 already
    # defines _() as a pass-through when ENABLE_NLS is undefined.
    # --disable-textport: the `qalc` CLI, which we do not build.
    fetch_tar "https://github.com/Qalculate/libqalculate/archive/refs/tags/v$QALC_VER.tar.gz" \
        "libqalculate-$QALC_VER.tar.gz" "libqalculate-$QALC_VER"
    apply_patches "$LQ_SRC" libqalculate
    if [ ! -f "$LQ_SRC/configure" ]; then
        echo "=== autoreconf libqalculate (upstream ships no configure in the tag tarball)"
        ( cd "$LQ_SRC" && env PATH="$BUILD_PATH" autoreconf -fi ) 2>&1 | tee "$LOGDIR/libqalculate-autoreconf.log"
    fi
    autoconf_build libqalculate "libqalculate-$QALC_VER" libqalculate.a -- \
        CPPFLAGS="-I$SYSROOT/include" LDFLAGS="-L$SYSROOT/lib $LDFLAGS" \
        --without-libcurl --without-icu --without-gnuplot-call \
        --disable-nls --disable-textport --disable-unittests \
        --enable-compiled-definitions
    ;;

qtsvg)
    # Qt6::Svg + the qsvg imageformat and iconengine plugins. NOT optional
    # here, unlike in qt-torrentfileeditor103: icons.qrc is 53 .svg files and
    # qalculateqtsettings.h:31 defines
    #   LOAD_APP_ICON(x) QIcon(":/icons/apps/scalable/" x ".svg")
    # so without these two plugins every toolbar button, every menu action and
    # the window icon come out as null QIcons -- an app with no icons at all.
    fetch_qt_module qtsvg
    apply_patches "$SRCDIR/qtsvg-everywhere-src-$QT_VER" qtsvg
    build_qt_module qtsvg
    ;;

app)
    fetch_tar "https://github.com/Qalculate/qalculate-qt/archive/refs/tags/v$QALC_VER.tar.gz" \
        "qalculate-qt-$QALC_VER.tar.gz" "qalculate-qt-$QALC_VER"
    apply_patches "$QALC_SRC" qalculate-qt

    # --- stage the CMakeLists -------------------------------------------
    # Upstream has NO CMake build system: v5.12.0 is qmake-only. This file is
    # OURS, not a modification of upstream's, so it lives in cmake/ rather
    # than patches/ and is copied over the fetched tree.
    cp -f "$HERE/cmake/CMakeLists.txt" "$QALC_SRC/CMakeLists.txt"

    # ...and the divergence guard the recon asked for. cmake/CMakeLists.txt
    # GLOBs src/*.cpp; upstream's .pro enumerates them. If a version bump adds
    # or removes a source file, these two disagree and the build must stop
    # here rather than silently dropping (or silently gaining) a translation
    # unit. Compares the basenames as sorted sets.
    PRO_SRCS=$(sed -n 's/^SOURCES *+= *//p' "$QALC_SRC/qalculate-qt.pro" \
        | tr ' ' '\n' | sed 's|^src/||' | grep -v '^$' | sort)
    DIR_SRCS=$(cd "$QALC_SRC/src" && ls *.cpp | sort)
    if [ "$PRO_SRCS" != "$DIR_SRCS" ]; then
        echo "qt-qalculate: qalculate-qt.pro's SOURCES no longer match src/*.cpp." >&2
        echo "  Our CMakeLists globs the directory; upstream enumerates. Reconcile" >&2
        echo "  before building -- the difference is:" >&2
        diff <(echo "$PRO_SRCS") <(echo "$DIR_SRCS") >&2 || true
        exit 1
    fi
    echo "=== source list check: qalculate-qt.pro and src/*.cpp agree ($(echo "$DIR_SRCS" | wc -l | tr -d ' ') files)"

    # --- stage a font ----------------------------------------------------
    # First hit wins; repo-local candidates first so a machine-independent
    # font is preferred. Nothing is fetched from the network: a build must not
    # depend on a font download.
    FONT_CANDIDATES=(
        "$PWD/../doctools/tex/texlive-source/libs/gd/libgd-src/tests/freetype/DejaVuSans.ttf"
        "$QTPLUGIN/smoke/fonts/DejaVuSans.ttf"
        "$PWD/../qt-torrentfileeditor103/fonts/DejaVuSans.ttf"
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"
        "/usr/share/fonts/dejavu/DejaVuSans.ttf"
        "/Library/Fonts/Arial.ttf"
        "/System/Library/Fonts/Supplemental/Arial.ttf"
        "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf"
    )
    FONT=""
    for f in "${FONT_CANDIDATES[@]}"; do
        if [ -f "$f" ]; then FONT="$f"; break; fi
    done
    if [ -z "$FONT" ]; then
        echo "qt-qalculate: no font found. Tried:" >&2
        printf '  %s\n' "${FONT_CANDIDATES[@]}" >&2
        echo "  Qt 6 ships no fonts and a wk node has no host font dir, so a" >&2
        echo "  component built without one renders every string as nothing." >&2
        exit 1
    fi
    cp -f "$FONT" "$FONTDIR/$(basename "$FONT")"
    echo "=== font: $FONT -> fonts/$(basename "$FONT")"
    # The .qrc is GENERATED next to the staged font because which font exists
    # depends on the machine. Prefix /fonts is where QWkFontDatabase looks
    # last (after QT_QPA_FONTDIR and /usr/share/fonts in the node's VFS).
    cat > "$FONTDIR/wkfonts.qrc" <<EOF
<!DOCTYPE RCC>
<RCC version="1.0">
    <qresource prefix="/fonts">
        <file>$(basename "$FONT")</file>
    </qresource>
</RCC>
EOF

    # --- wit bindings ----------------------------------------------------
    # Regenerated every build, into OUR gen/ rather than gfx-compat/gen: the
    # shared one is disposable and several plugin builds may run at once.
    # Only the *_component_type.o files are used from here -- the shims and
    # the bindings themselves are already objects inside libqwk.a.
    echo "=== wit-bindgen (wkgfx world)"
    wit-bindgen c --world wkgfx "$GFXCOMPAT/wit" --out-dir "$GEN" >/dev/null
    echo "=== wit-bindgen (wkclipboard world)"
    wit-bindgen c --world wkclipboard "$CLIPCOMPAT/wit" --out-dir "$GEN" >/dev/null

    LOG="$LOGDIR/app.log"
    if [ ! -f "$BUILD/app/build.ninja" ] || [ -n "${WK_QALC_RECONFIGURE:-}" ]; then
        echo "=== configuring qalculate-qt $QALC_VER (log: $LOG)"
        env PATH="$BUILD_PATH" cmake -G Ninja -S "$QALC_SRC" -B "$BUILD/app" \
            -DCMAKE_TOOLCHAIN_FILE="$QTPLUGIN/wasip2.cmake" \
            -DWASI_SDK_PREFIX="$WASI_SDK" \
            -DCMAKE_FIND_ROOT_PATH="$QTBASE_SYSROOT;$SYSROOT" \
            -DCMAKE_PREFIX_PATH="$QTBASE_SYSROOT;$SYSROOT" \
            -DQT_HOST_PATH="$HOST_PREFIX" \
            -DCMAKE_BUILD_TYPE=MinSizeRel \
            -DQALC_PREFIX="$SYSROOT" \
            -DWK_QPA_LIB="$QTBASE_SYSROOT/lib/libqwk.a" \
            -DWK_SVG_PLUGINS="$SYSROOT/plugins/imageformats/libqsvg.a;$SYSROOT/plugins/iconengines/libqsvgicon.a" \
            -DWK_GFX_COMPONENT_TYPE="$GEN/wkgfx_component_type.o" \
            -DWK_CLIP_COMPONENT_TYPE="$GEN/wkclipboard_component_type.o" \
            -DWK_FONTS_QRC="$FONTDIR/wkfonts.qrc" \
            2>&1 | tee "$LOG"
    else
        echo "=== app already configured (WK_QALC_RECONFIGURE=1 to redo)"
    fi
    echo "=== ninja qalculate-qt"
    env PATH="$BUILD_PATH" cmake --build "$BUILD/app" --parallel "$JOBS" 2>&1 | tee -a "$LOG"

    # wasip2.cmake leaves CMAKE_EXECUTABLE_SUFFIX empty on purpose (Qt's
    # architecture config test depends on it), so the linked artifact has no
    # extension -- and wasm-component-ld already made it a COMPONENT at link
    # time. No wasip1 adapter, no `wasm-tools component new`.
    cp -f "$BUILD/app/qalculate-qt" "$HERE/qalculate-qt.wasm"
    echo
    ls -l "$HERE/qalculate-qt.wasm"
    echo "built plugins/qt-qalculate/qalculate-qt.wasm"
    ;;

*)
    echo "qt-qalculate: unknown stage '$stage' (want: deps qtsvg app)" >&2
    exit 1
    ;;
esac
done
