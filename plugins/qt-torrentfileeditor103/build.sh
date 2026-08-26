#!/usr/bin/env bash
# torrent-file-editor 1.0.3 — a REAL Qt 6 Widgets application as a wk node.
#
# Produces ./torrent-file-editor.wasm: a wasm32-wasip2 COMPONENT that imports
# wasi:surface/graphics-context/frame-buffer and paints its windows into the
# single RGBA8 surface wk gives a node, through the wk QPA plugin.
#
# WHAT THIS SCRIPT IS, in one line: three cross-builds and a link.
#
#   1. qt5compat  -> ./sysroot   (Qt6::Core5Compat — QTextCodec + QRegExp;
#                                 upstream needs both and they left QtCore in
#                                 Qt 6, so this module is NOT optional)
#   2. qtsvg      -> ./sysroot   (Qt6::Svg + the qsvg image/icon plugins, for
#                                 the app's SVG window icon)
#   3. the app    -> ./torrent-file-editor.wasm
#
# WHY A SECOND SYSROOT. plugins/qt/sysroot holds qtbase, built by
# plugins/qt/build-qtbase.sh. The two Qt modules above are extra repos, and
# they install into ./sysroot HERE rather than into plugins/qt/sysroot, so this
# plugin never writes into another plugin's tree — several ports can grow their
# own module set from one shared qtbase. Both prefixes go on CMAKE_PREFIX_PATH
# and CMAKE_FIND_ROOT_PATH; Qt6Config.cmake in the qtbase prefix finds
# Qt6Core5CompatConfig.cmake here through the ordinary CMAKE_PREFIX_PATH search
# (it is why Qt's own docs call this a "Qt module install to a separate
# prefix").
#
# EVERYTHING THE PORT INHERITS FROM plugins/qt, and the traps that come with it
# (read plugins/qt/wasip2.cmake's header, it is the primary document):
#
#   * the exnref EH flag set (-fwasm-exceptions -mllvm -wasm-enable-sjlj
#     -mllvm -wasm-use-legacy-eh=false). wasmtime runs with the exception
#     proposal ON and REJECTS wasi-sdk's default legacy encoding, at
#     instantiate time, for the WHOLE component — so one stray object poisons
#     the binary and the error points nowhere near it. The toolchain file puts
#     these on every object of every language;
#   * the wasm-opt trap: clang runs wasm-opt as an optional post-link pass and
#     the wasm-opt on PATH cannot parse exnref. Every cmake/ninja call here
#     runs under a PATH that omits it — including the BUILD step, because
#     CMake bakes absolute tool paths into build.ninja;
#   * no threads (FEATURE_thread=OFF in qtbase). See the honest note below;
#   * no dlopen: the wk QPA plugin, the image-format plugins and now the qsvg
#     plugins are all STATIC and are named with Q_IMPORT_PLUGIN in main.cpp,
#     which is what patches/0002 adds.
#
# WHAT DEGRADES, and it is bounded: mainwindow.cpp:789 moves a Worker to a
# QThread to SHA1 piece hashes when CREATING a torrent from a folder. With
# FEATURE_thread=OFF that thread never runs, so that one progress dialog would
# sit at 0%. OPENING, inspecting, editing and saving an existing .torrent — the
# app's whole reason to exist — never touches that path.
#
# FONTS. Qt 6 ships none and a wk node has no host font directory: with no font
# the app runs, QFontDatabase is empty, and every string renders as nothing at
# all. So this script STAGES one TTF into ./fonts/ and patches/0002 compiles it
# in as a Qt resource under :/fonts, which QWkFontDatabase falls back to. The
# staged copy is gitignored — it is somebody else's font.
#
# Knobs: WK_TFE_STAGES="qt5compat qtsvg app"   JOBS=N   LOGDIR=...
#        WK_TFE_RECONFIGURE=1   QT_HOST_PATH=...
#
# Long: budget 20-40 minutes cold. Run it detached and tail ./logs.
set -euo pipefail
cd "$(dirname "$0")"

# --- toolchain guard (same shape as plugins/mupdf/build.sh) ------------------
MISE_SDK="$HOME/.local/share/mise/installs/github-web-assembly-wasi-sdk/wasi-sdk-34-rc.2"
WASI_SDK="${WASI_SDK:-$([ -d "$MISE_SDK" ] && echo "$MISE_SDK" || echo "$HOME/wasi-sdk")}"
EXPECT="wasi-sdk-34-rc.2"
case "$WASI_SDK" in
    *"$EXPECT"*) ;;
    *) echo "qt-tfe: expected $EXPECT (set WASI_SDK), got: $WASI_SDK" >&2; exit 1 ;;
esac

QT_VER=6.8.4
QT_SERIES=6.8
TFE_VER=1.0.3

HERE="$PWD"
QTPLUGIN="$PWD/../qt"                 # the shared Qt port: qtbase + the wk QPA
QTBASE_SYSROOT="$QTPLUGIN/sysroot"
HOST_PREFIX="${QT_HOST_PATH:-$QTPLUGIN/host}"
GFXCOMPAT="$PWD/../gfx-compat"

SRCDIR="$PWD/src"
TARBALLS="$PWD/tarballs"
PATCHDIR="$PWD/patches"
SYSROOT="$PWD/sysroot"                # OUR module prefix (qt5compat, qtsvg)
BUILD="$PWD/build"
GEN="$PWD/gen"                        # our own wit-bindgen output
FONTDIR="$PWD/fonts"
LOGDIR="${LOGDIR:-$PWD/logs}"
JOBS="${JOBS:-$(sysctl -n hw.ncpu 2>/dev/null || nproc)}"
BUILD_PATH="$WASI_SDK/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"
mkdir -p "$SRCDIR" "$TARBALLS" "$LOGDIR" "$BUILD" "$GEN" "$FONTDIR"

# --- preflight --------------------------------------------------------------
if [ ! -f "$QTBASE_SYSROOT/lib/libQt6Widgets.a" ]; then
    echo "qt-tfe: no cross Qt in $QTBASE_SYSROOT" >&2
    echo "  run plugins/qt/build-qtbase.sh first (it is the long one)." >&2
    exit 1
fi
if [ ! -f "$QTBASE_SYSROOT/lib/libqwk.a" ]; then
    echo "qt-tfe: no wk QPA plugin at $QTBASE_SYSROOT/lib/libqwk.a" >&2
    echo "  run plugins/qt/build-qpa.sh first. Without it the app links but" >&2
    echo "  QApplication aborts with 'no Qt platform plugin could be" >&2
    echo "  initialized' and an EMPTY plugin list." >&2
    exit 1
fi
if [ ! -x "$HOST_PREFIX/libexec/moc" ] && [ ! -x "$HOST_PREFIX/bin/moc" ]; then
    echo "qt-tfe: no host Qt at $HOST_PREFIX -- run plugins/qt/build-host.sh" >&2
    exit 1
fi

# --- upstream, fetched not vendored -----------------------------------------
# Qt submodule tarballs are "-everywhere-opensource-src-" upstream and extract
# to "-everywhere-src-". Not a typo; same as plugins/qt/build-qtbase.sh.
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

TFE_SRC="$SRCDIR/torrent-file-editor-$TFE_VER"
fetch_app() {
    discard_unpatched_tree "$TFE_SRC"
    [ -d "$TFE_SRC" ] && return 0
    local tar_path="$TARBALLS/tfe-v$TFE_VER.tar.gz"
    if [ ! -f "$tar_path" ]; then
        echo "fetching torrent-file-editor v$TFE_VER..."
        curl -fsSL --retry 3 -o "$tar_path.part" \
            "https://github.com/torrent-file-editor/torrent-file-editor/archive/refs/tags/v$TFE_VER.tar.gz"
        mv "$tar_path.part" "$tar_path"
    fi
    echo "extracting torrent-file-editor v$TFE_VER..."
    tar xzf "$tar_path" -C "$SRCDIR"
}

# --- patches ----------------------------------------------------------------
# patches/<prefix>-NNNN-*.patch, applied -p1 in order at that tree's root. See
# patches/README.md for the ledger and the reason each one exists.
#
# Idempotency is a STAMP FILE, not a per-patch `git apply --reverse --check`.
# The reverse-check idiom (which plugins/qt/build-qtbase.sh uses) silently
# stops working the moment two patches touch nearby lines: patch 0002 adds
# lines inside 0001's context, so after both are applied 0001 no longer
# reverse-applies and the script would try to apply it again and die with
# "patch does not apply". A stamp is unambiguous, and re-extracting an
# unstamped tree means a half-applied tree can never linger.
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

# An unstamped tree is either pristine-from-tarball or the wreckage of a failed
# patch run; either way, throw it away and extract again.
discard_unpatched_tree() {
    local tree="$1"
    if [ -d "$tree" ] && [ ! -f "$tree/.wk-patched" ]; then
        echo "  re-extracting $(basename "$tree") (no .wk-patched stamp)"
        rm -rf "$tree"
    fi
}

# --- a Qt module cross-build ------------------------------------------------
# Every extra Qt repo is configured the same way: our toolchain, the qtbase
# prefix to find Qt6 in, OUR prefix to install into, and the host tools.
#
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
    if [ ! -f "$bld/build.ninja" ] || [ -n "${WK_TFE_RECONFIGURE:-}" ]; then
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
        echo "=== $name already configured in $bld (WK_TFE_RECONFIGURE=1 to redo)"
    fi
    echo "=== ninja $name"
    env PATH="$BUILD_PATH" cmake --build "$bld" --parallel "$JOBS" 2>&1 | tee -a "$log"
    env PATH="$BUILD_PATH" cmake --install "$bld" 2>&1 | tee -a "$log"
}

STAGES="${WK_TFE_STAGES:-qt5compat qtsvg app}"

for stage in $STAGES; do
case "$stage" in

qt5compat)
    # Qt6::Core5Compat. Upstream uses QTextCodec (bencodemodel.cpp,
    # mainwindow.cpp) and QRegExp/QRegExpValidator (mainwindow.cpp,
    # lineeditwidget.cpp, aboutdlg.cpp); both left QtCore in Qt 6 and live
    # here. The Core5Compat module depends on qtbase alone — its only optional
    # extras (ICU, iconv, the Quick imports) are all off for us already: the
    # imports subdir is skipped because there is no Qt::Quick target, and
    # QT_FEATURE_icu/iconv came back false from qtbase's own config tests.
    fetch_qt_module qt5compat
    apply_patches "$SRCDIR/qt5compat-everywhere-src-$QT_VER" qt5compat
    #
    # FEATURE_iconv=OFF, and NOT because iconv is missing. wasi-libc inherits
    # musl's iconv, so qt5compat's config test genuinely passes and the module
    # builds qiconvcodec.cpp happily. The problem is the two-prefix layout:
    # Core5CompatDependencies.cmake then records `WrapIconv` as a third-party
    # dependency, and FindWrapIconv.cmake is installed into OUR prefix while
    # the module path an app inherits from Qt6Config points at the QTBASE
    # prefix -- so every app linking Core5Compat dies with "Qt6Core5Compat
    # could not be found because dependency WrapIconv could not be found"
    # while the module sits right there. Turning the feature off removes the
    # dependency instead of papering over the search path (the app's own
    # CMakeLists overwrites CMAKE_MODULE_PATH, so -D cannot fix it from
    # outside anyway).
    # What is lost: only the iconv-backed codecs. QTextCodec keeps its
    # built-in tables -- UTF-*, Latin-1, every ISO-8859-*, the windows-125x
    # family, KOI8, and (QT_FEATURE_big_codecs) Big5/GBK/EUC-JP/EUC-KR/
    # Shift-JIS -- which is the whole of what a .torrent's `encoding` field
    # realistically names.
    build_qt_module qt5compat -DFEATURE_iconv=OFF
    ;;

qtsvg)
    # Qt6::Svg + the qsvg imageformat and iconengine plugins. The app's
    # resources.qrc carries icons/app.svg (its window icon); every other icon
    # in it is a PNG, so this module is the smallest of the three builds and
    # the least load-bearing — but upstream's CMakeLists requires it whenever
    # BUILD_SHARED is on, and turning that off to dodge it would be a patch
    # that changes the app's link list for no gain.
    fetch_qt_module qtsvg
    apply_patches "$SRCDIR/qtsvg-everywhere-src-$QT_VER" qtsvg
    build_qt_module qtsvg
    ;;

app)
    # --- stage a font ----------------------------------------------------
    # First hit wins; repo-local candidates first so a machine-independent
    # font is preferred. Nothing is fetched from the network: a build must not
    # depend on a font download.
    FONT_CANDIDATES=(
        "$PWD/../doctools/tex/texlive-source/libs/gd/libgd-src/tests/freetype/DejaVuSans.ttf"
        "$QTPLUGIN/smoke/fonts/DejaVuSans.ttf"
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
        echo "qt-tfe: no font found. Tried:" >&2
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
    # Only wkgfx_component_type.o is used from here — the shim and the
    # bindings themselves are already objects inside libqwk.a.
    echo "=== wit-bindgen (wkgfx world)"
    wit-bindgen c --world wkgfx "$GFXCOMPAT/wit" --out-dir "$GEN" >/dev/null

    fetch_app
    apply_patches "$TFE_SRC" torrent-file-editor

    # --- configure + build ----------------------------------------------
    #
    # CMAKE_BUILD_TYPE=MinSizeRel, deliberately. Upstream's CMakeLists:93 adds
    # -Werror when the build type is exactly Release (or its typo'd
    # "RelWithDbInfo"), and Clang 23 finds plenty to say about a codebase
    # targeting much older compilers. MinSizeRel is -Os -DNDEBUG, skips that
    # branch entirely, and needs no patch to upstream's warning policy.
    #
    # QT6_BUILD=ON: cmake/QtMajorVersion.cmake otherwise runs `qmake -query`
    # off the host PATH to guess a Qt version, which on this machine would
    # find Homebrew's Qt.
    LOG="$LOGDIR/app.log"
    if [ ! -f "$BUILD/app/build.ninja" ] || [ -n "${WK_TFE_RECONFIGURE:-}" ]; then
        echo "=== configuring torrent-file-editor $TFE_VER (log: $LOG)"
        env PATH="$BUILD_PATH" cmake -G Ninja -S "$TFE_SRC" -B "$BUILD/app" \
            -DCMAKE_TOOLCHAIN_FILE="$QTPLUGIN/wasip2.cmake" \
            -DWASI_SDK_PREFIX="$WASI_SDK" \
            -DCMAKE_FIND_ROOT_PATH="$QTBASE_SYSROOT;$SYSROOT" \
            -DCMAKE_PREFIX_PATH="$QTBASE_SYSROOT;$SYSROOT" \
            -DQT_HOST_PATH="$HOST_PREFIX" \
            -DCMAKE_BUILD_TYPE=MinSizeRel \
            -DQT6_BUILD=ON \
            -DWK_QPA_LIB="$QTBASE_SYSROOT/lib/libqwk.a" \
            -DWK_SVG_PLUGINS="$SYSROOT/plugins/imageformats/libqsvg.a;$SYSROOT/plugins/iconengines/libqsvgicon.a" \
            -DWK_GFX_COMPONENT_TYPE="$GEN/wkgfx_component_type.o" \
            -DWK_FONTS_QRC="$FONTDIR/wkfonts.qrc" \
            2>&1 | tee "$LOG"
    else
        echo "=== app already configured (WK_TFE_RECONFIGURE=1 to redo)"
    fi
    echo "=== ninja torrent-file-editor"
    env PATH="$BUILD_PATH" cmake --build "$BUILD/app" --parallel "$JOBS" 2>&1 | tee -a "$LOG"

    # wasip2.cmake leaves CMAKE_EXECUTABLE_SUFFIX empty on purpose (Qt's
    # architecture config test depends on it), so the linked artifact has no
    # extension — and wasm-component-ld already made it a COMPONENT at link
    # time. No wasip1 adapter, no `wasm-tools component new`.
    cp -f "$BUILD/app/torrent-file-editor" "$HERE/torrent-file-editor.wasm"
    echo
    ls -l "$HERE/torrent-file-editor.wasm"
    echo "built plugins/qt-torrentfileeditor103/torrent-file-editor.wasm"
    ;;

*)
    echo "qt-tfe: unknown stage '$stage' (want: qt5compat qtsvg app)" >&2
    exit 1
    ;;
esac
done
