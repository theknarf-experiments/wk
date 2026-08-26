#!/usr/bin/env bash
# Cross-build qtdeclarative 6.8.4 (QtQml + QtQuick + Quick Controls) for
# wasm32-wasip2, into ./sysroot.
#
# This is milestone M4 of plugins/qt/PORTING.md ("Quick on the software
# backend"), done here rather than there because this plugin is the thing that
# needs it and plugins/qt is being worked on concurrently.
#
# THE THREE PREREQUISITES
#   plugins/qt/sysroot   the cross qtbase (Core/Gui/Widgets) built by
#                        plugins/qt/build-qtbase.sh. Read-only to us.
#   plugins/qt/wasip2.cmake  the toolchain. Its header is the primary document
#                        for the exnref-EH flag set; do not duplicate it here.
#   ./host               a native Qt 6.8.4 WITH QtGui and qsb, built by
#                        ./build-hosttools.sh. plugins/qt/host is Gui-less and
#                        therefore has no qsb, and without qsb qtdeclarative
#                        silently skips every Qt Quick module. See that script.
#
# WHY A SEPARATE INSTALL PREFIX
# Qt modules normally install next to qtbase. We install into ./sysroot instead
# and put BOTH prefixes on CMAKE_PREFIX_PATH when building the app, because
# plugins/qt/sysroot belongs to another plugin that is under active development
# by someone else. Qt supports this (it is what QT_ADDITIONAL_PACKAGES_PREFIX_PATH
# exists for); the cost is that every downstream cmake line carries two paths.
#
# THE SOFTWARE BACKEND IS A RUNTIME CHOICE, NOT A BUILD ONE. QtQuick always
# builds its RHI/OpenGL scenegraph plus the `software` adaptation; which one
# runs is decided by QT_QUICK_BACKEND=software at startup. So this build still
# needs qsb to compile scenegraph shaders it will never load. That is why
# build-hosttools.sh has to build a whole Gui-enabled host Qt.
#
# Wasm-specific traps inherited from plugins/qt (all documented at length in
# plugins/qt/wasip2.cmake and PORTING.md): every object needs the exnref EH
# flags, wasm-opt must not be on PATH, no LTO, no threads.
#
# Long: budget half an hour. Run it detached.
# Knobs: WK_QT_RECONFIGURE=1  JOBS=N  LOGDIR=...
set -euo pipefail
cd "$(dirname "$0")"

MISE_SDK="$HOME/.local/share/mise/installs/github-web-assembly-wasi-sdk/wasi-sdk-34-rc.2"
WASI_SDK="${WASI_SDK:-$([ -d "$MISE_SDK" ] && echo "$MISE_SDK" || echo "$HOME/wasi-sdk")}"
EXPECT="wasi-sdk-34-rc.2"
case "$WASI_SDK" in
    *"$EXPECT"*) ;;
    *) echo "qt-slate/build-qtdeclarative: expected $EXPECT (set WASI_SDK), got: $WASI_SDK" >&2; exit 1 ;;
esac

QT_VER=6.8.4
QT_SERIES=6.8
QTBASE_PLUGIN="$PWD/../qt"
QTBASE_SYSROOT="$QTBASE_PLUGIN/sysroot"
TOOLCHAIN="$QTBASE_PLUGIN/wasip2.cmake"
SRCDIR="$PWD/src"
TARBALLS="$PWD/tarballs"
SHARED_TARBALLS="$QTBASE_PLUGIN/tarballs"
QTDECL_SRC="$SRCDIR/qtdeclarative-everywhere-src-$QT_VER"
PATCHDIR="$PWD/patches"
BUILD="$PWD/build-target/qtdeclarative"
SYSROOT="$PWD/sysroot"
HOST_PREFIX="${QT_HOST_PATH:-$PWD/host}"
LOGDIR="${LOGDIR:-$PWD/logs}"
JOBS="${JOBS:-$(sysctl -n hw.ncpu 2>/dev/null || nproc)}"
BUILD_PATH="$WASI_SDK/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"
mkdir -p "$SRCDIR" "$TARBALLS" "$LOGDIR" "$(dirname "$BUILD")"

if [ ! -f "$QTBASE_SYSROOT/lib/libQt6Gui.a" ]; then
    echo "qt-slate: no cross qtbase in $QTBASE_SYSROOT" >&2
    echo "  run plugins/qt/build-qtbase.sh first (it is the shared Qt for wasm)." >&2
    exit 1
fi
if [ ! -x "$HOST_PREFIX/bin/qsb" ] && [ ! -x "$HOST_PREFIX/libexec/qsb" ]; then
    echo "qt-slate: no qsb in $HOST_PREFIX -- run ./build-hosttools.sh first" >&2
    echo "  Without Qt::qsb, qtdeclarative/src/CMakeLists.txt:37 takes its else()" >&2
    echo "  branch and builds NO QtQuick at all, with only a NOTE to say so." >&2
    exit 1
fi

# --- upstream, fetched not vendored -----------------------------------------
# Our OWN extraction, not plugins/qt/src: this build applies wasi patches and
# that source tree belongs to another plugin. The tarball is reused from
# plugins/qt/tarballs when it is already there.
if [ ! -d "$QTDECL_SRC" ]; then
    tarname="qtdeclarative-everywhere-opensource-src-$QT_VER.tar.xz"
    tar_path="$TARBALLS/$tarname"
    if [ ! -f "$tar_path" ] && [ -f "$SHARED_TARBALLS/$tarname" ]; then
        tar_path="$SHARED_TARBALLS/$tarname"
    fi
    if [ ! -f "$tar_path" ]; then
        echo "fetching qtdeclarative $QT_VER..."
        curl -fsSL --retry 3 -o "$TARBALLS/$tarname.part" \
            "https://download.qt.io/archive/qt/$QT_SERIES/$QT_VER/submodules/$tarname"
        mv "$TARBALLS/$tarname.part" "$TARBALLS/$tarname"
        tar_path="$TARBALLS/$tarname"
    fi
    echo "extracting qtdeclarative $QT_VER..."
    tar xJf "$tar_path" -C "$SRCDIR"
fi

# --- patches ----------------------------------------------------------------
for p in "$PATCHDIR"/qtdeclarative-*.patch; do
    [ -e "$p" ] || continue
    if git -C "$QTDECL_SRC" apply --reverse --check "$p" >/dev/null 2>&1; then
        echo "  patch (already applied): $(basename "$p")"
        continue
    fi
    echo "  patch: $(basename "$p")"
    git -C "$QTDECL_SRC" apply "$p"
done

CFG=(
    -G Ninja -S "$QTDECL_SRC" -B "$BUILD"
    -DCMAKE_TOOLCHAIN_FILE="$TOOLCHAIN"
    -DWASI_SDK_PREFIX="$WASI_SDK"
    -DCMAKE_FIND_ROOT_PATH="$QTBASE_SYSROOT"
    -DCMAKE_PREFIX_PATH="$QTBASE_SYSROOT"
    -DQT_HOST_PATH="$HOST_PREFIX"
    # NOT redundant with QT_HOST_PATH, and omitting it fails in a way that
    # points at the wrong thing entirely:
    #
    #   Failed to find the host tool "Qt6::qmlaotstats". It is part of the
    #   Qt6QmlTools package, but the package could not be found.
    #
    # ...while ./host/lib/cmake/Qt6QmlTools sits right there. The reason is
    # QtPublicDependencyHelpers.cmake:296-310: QT_HOST_PATH_CMAKE_DIR defaults
    # to `initial_qt_host_path_cmake_dir`, i.e. the host path RECORDED IN
    # plugins/qt/sysroot AT QTBASE-CONFIGURE TIME -- plugins/qt/host/lib/cmake.
    # It only auto-computes ${QT_HOST_PATH}/lib/cmake when that recorded path
    # does not exist. So overriding QT_HOST_PATH alone leaves the two pointing
    # at DIFFERENT host trees, and qt_internal_find_tool
    # (QtToolHelpers.cmake:712-741) sets CMAKE_PREFIX_PATH from
    # QT_HOST_PATH_CMAKE_DIR while prepending QT_HOST_PATH to
    # CMAKE_FIND_ROOT_PATH -- with FIND_ROOT_PATH_MODE_PACKAGE=ONLY the
    # mismatched prefix is not under any root, gets rerooted, and matches
    # nothing. Qt's own REROOT_PATH_ISSUE_MARKER comment is about this.
    -DQT_HOST_PATH_CMAKE_DIR="$HOST_PREFIX/lib/cmake"
    -DCMAKE_INSTALL_PREFIX="$SYSROOT"
    -DCMAKE_BUILD_TYPE=Release
    -DBUILD_SHARED_LIBS=OFF
    -DQT_BUILD_EXAMPLES=OFF
    -DQT_BUILD_TESTS=OFF
    -DQT_BUILD_BENCHMARKS=OFF
    -DQT_BUILD_MANUAL_TESTS=OFF
    -DWARNINGS_ARE_ERRORS=OFF

    # The JS engine's JIT emits native machine code into mmap'd RWX pages.
    # There is no such thing on wasm — and no way to make one, since wasm code
    # is not data. The feature's CONDITION already excludes wasm32 (it only
    # matches i386/x86_64/arm/arm64), but pin it: a silently-enabled JIT here
    # would fail at link with references to the assembler backend.
    -DFEATURE_qml_jit=OFF

    # No network in the cross qtbase (plugins/qt/build-qtbase.sh: FEATURE_network=OFF),
    # so these would auto-disable; pinning them keeps the reason visible.
    -DFEATURE_qml_network=OFF
    -DFEATURE_qml_ssl=OFF
    -DFEATURE_qml_xml_http_request=OFF

    # The QML debug server listens on a TCP socket and needs threads.
    -DFEATURE_qml_debug=OFF
    -DFEATURE_qml_profiler=OFF
    -DFEATURE_qml_preview=OFF

    # Not used by this app, and each is a whole QML module's worth of build.
    -DFEATURE_quick_particles=OFF

    # Everything Slate imports must stay ON, so they are listed rather than
    # left to autodetect: QtQuick, Layouts, Controls (Material AND Universal —
    # app/qtquickcontrols2.conf selects Material, and 3 QML files import
    # Universal), plus the Basic style every other style falls back to.
    -DFEATURE_quickcontrols2_basic=ON
    -DFEATURE_quickcontrols2_material=ON
    -DFEATURE_quickcontrols2_universal=ON
    -DFEATURE_quickcontrols2_fusion=ON
)

LOG="$LOGDIR/target-qtdeclarative.log"
if [ ! -f "$BUILD/CMakeCache.txt" ] || [ -n "${WK_QT_RECONFIGURE:-}" ]; then
    echo "=== configuring qtdeclarative $QT_VER for wasm32-wasip2 (log: $LOG)"
    env PATH="$BUILD_PATH" cmake "${CFG[@]}" 2>&1 | tee "$LOG"
else
    echo "=== qtdeclarative already configured in $BUILD (WK_QT_RECONFIGURE=1 to redo)"
fi

# The single most important thing configure decided. Without Qt::qsb,
# qtdeclarative/src/CMakeLists.txt:37 takes its else() branch, prints ONE note,
# and builds QtQml with no Qt Quick at all -- which then surfaces an hour later
# as an undefined QQuickItem in the app link. Fail here instead.
if grep -q "Qt Quick modules not built" "$LOG"; then
    echo "qt-slate: configure did not find qsb, so NO Qt Quick will be built." >&2
    echo "  Run ./build-hosttools.sh (and check QT_HOST_PATH_CMAKE_DIR below)." >&2
    exit 1
fi
grep -n -A 2 "^Qt Quick:" "$BUILD/config.summary" || true

echo "=== ninja"
env PATH="$BUILD_PATH" cmake --build "$BUILD" --parallel "$JOBS" 2>&1 | tee -a "$LOG"
env PATH="$BUILD_PATH" cmake --install "$BUILD" 2>&1 | tee -a "$LOG"

echo
echo "qtdeclarative $QT_VER (wasm32-wasip2) installed in $SYSROOT"
ls "$SYSROOT/lib"/libQt6*.a 2>/dev/null | sed 's/^/  /' || echo "  (nothing -- check $LOG)"
