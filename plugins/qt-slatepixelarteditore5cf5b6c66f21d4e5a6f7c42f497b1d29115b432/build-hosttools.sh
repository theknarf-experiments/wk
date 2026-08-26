#!/usr/bin/env bash
# Build the NATIVE host Qt this plugin needs, into ./host.
#
# WHY THIS EXISTS AT ALL — plugins/qt/host is not enough for Qt Quick
# ------------------------------------------------------------------
# plugins/qt/build-host.sh deliberately builds a tools-only host Qt with
# FEATURE_gui=OFF ("qtdeclarative REQUIRES only BuildInternals+Core"). That is
# correct for Widgets, and it is exactly what M0-M3 needed. It is NOT enough to
# cross-build Qt Quick, because of one line in qtdeclarative:
#
#   src/CMakeLists.txt:37   if(TARGET Qt::Gui AND TARGET Qt::qsb AND QT_FEATURE_qml_animation)
#       ... add_subdirectory(quick) ... quicktemplates ... quickcontrols ...
#   else()
#       "Qt Quick modules not built due to not finding the qtshadertools 'qsb' tool."
#
# `Qt::qsb` is a HOST executable, imported from the Qt6ShaderToolsTools package
# found under QT_HOST_PATH (qtdeclarative/CMakeLists.txt:20-40 prepends
# QT_HOST_PATH to CMAKE_PREFIX_PATH *specifically* to find it). It compiles
# QtQuick's built-in scenegraph shaders into .qsb resources at build time.
#
# We do not need those shaders at RUNTIME — the node runs
# QT_QUICK_BACKEND=software, which never touches the RHI — but there is no
# configure switch that says "build Quick without shaders", so qsb has to exist
# or the whole Quick half of qtdeclarative silently does not get built. And qsb
# cannot be built without QtGui: qtshadertools/CMakeLists.txt:25 is a hard
#   if(NOT TARGET Qt::Gui) -> "Skipping the build" -> return()
# because QShader/QShaderBaker live in QtGui's rhi layer.
#
# So this plugin needs a host Qt with Gui, and plugins/qt/host does not have
# one. Rather than reconfigure the shared host tree (another agent is building
# against it concurrently, and turning Gui on there would drag the whole Cocoa
# QPA into a tree whose entire point is to stay small), we build a SECOND,
# self-contained host tree here and point QT_HOST_PATH at it. It is the only
# thing in this plugin that duplicates work, and the duplication is what keeps
# plugins/qt untouched.
#
# What lands in ./host, and why each module is here:
#   qtbase          FEATURE_gui=ON -- moc/rcc/uic/syncqt/qmake AND libQt6Gui,
#                   which is the only reason qtshadertools will configure.
#   qtshadertools   qsb. The point of the exercise.
#   qtdeclarative   qmlcachegen, qmltyperegistrar, qmlimportscanner, qmllint.
#                   Built with FEATURE_qml_animation=OFF so this host build
#                   SKIPS all of QtQuick/Controls natively (that same
#                   src/CMakeLists.txt:37 condition, used the other way round):
#                   we want the QML code generators, not a native Quick.
#
# QT_HOST_PATH must be ONE directory, which is why all three go in the same
# prefix instead of leaning on plugins/qt/host for the QML tools.
#
# SOURCE: reused read-only from plugins/qt/src (build-host.sh/build-qtbase.sh
# fetch and patch it there; the Qt tarballs are ~700 MB and re-fetching them
# per plugin would be silly). If that tree is absent we fetch our own copy into
# ./src. Either way nothing here writes to plugins/qt.
#
# Idempotent: re-running is a no-op once ./host has qsb. WK_QT_FORCE=1 rebuilds.
# LONG: budget an hour. Run it detached and tail logs/.
set -euo pipefail
cd "$(dirname "$0")"

QT_VER=6.8.4
QT_SERIES=6.8
SHARED_SRC="$PWD/../qt/src"
SRCDIR="$([ -d "$SHARED_SRC" ] && echo "$SHARED_SRC" || echo "$PWD/src")"
TARBALLS="$PWD/../qt/tarballs"
[ -d "$TARBALLS" ] || TARBALLS="$PWD/tarballs"
BUILDDIR="$PWD/build-host"
HOST_PREFIX="$PWD/host"
LOGDIR="${LOGDIR:-$PWD/logs}"
JOBS="${JOBS:-$(sysctl -n hw.ncpu 2>/dev/null || nproc)}"
mkdir -p "$SRCDIR" "$TARBALLS" "$BUILDDIR" "$LOGDIR"

if [ -x "$HOST_PREFIX/libexec/qsb" ] || [ -x "$HOST_PREFIX/bin/qsb" ]; then
    if [ -z "${WK_QT_FORCE:-}" ] &&
       { [ -x "$HOST_PREFIX/libexec/qmlcachegen" ] || [ -x "$HOST_PREFIX/bin/qmlcachegen" ]; }; then
        echo "qt-slate/build-hosttools: $HOST_PREFIX already populated (WK_QT_FORCE=1 to rebuild)"
        exit 0
    fi
fi

fetch_module() {
    local mod="$1"
    local dir="$SRCDIR/$mod-everywhere-src-$QT_VER"
    local tar="$TARBALLS/$mod-everywhere-opensource-src-$QT_VER.tar.xz"
    [ -d "$dir" ] && return 0
    if [ ! -f "$tar" ]; then
        echo "fetching $mod $QT_VER..."
        curl -fsSL --retry 3 -o "$tar.part" \
            "https://download.qt.io/archive/qt/$QT_SERIES/$QT_VER/submodules/$(basename "$tar")"
        mv "$tar.part" "$tar"
    fi
    echo "extracting $mod $QT_VER..."
    tar xJf "$tar" -C "$SRCDIR"
}

build_module() {
    local mod="$1"; shift
    local src="$SRCDIR/$mod-everywhere-src-$QT_VER"
    local bld="$BUILDDIR/$mod"
    local log="$LOGDIR/host-$mod.log"
    echo "=== host $mod $QT_VER -> $HOST_PREFIX (log: $log)"
    if [ ! -f "$bld/CMakeCache.txt" ] || [ -n "${WK_QT_FORCE:-}" ]; then
        cmake -G Ninja -S "$src" -B "$bld" \
            -DCMAKE_INSTALL_PREFIX="$HOST_PREFIX" \
            -DCMAKE_PREFIX_PATH="$HOST_PREFIX" \
            -DCMAKE_BUILD_TYPE=Release \
            -DBUILD_SHARED_LIBS=OFF \
            -DQT_BUILD_EXAMPLES=OFF \
            -DQT_BUILD_TESTS=OFF \
            -DQT_BUILD_BENCHMARKS=OFF \
            -DQT_BUILD_MANUAL_TESTS=OFF \
            -DWARNINGS_ARE_ERRORS=OFF \
            -DQT_NO_APPLE_SDK_MAX_VERSION_CHECK=ON \
            "$@" 2>&1 | tee "$log"
    else
        echo "  (configured already; WK_QT_FORCE=1 to reconfigure)"
    fi
    cmake --build "$bld" --parallel "$JOBS" 2>&1 | tee -a "$log"
    cmake --install "$bld" 2>&1 | tee -a "$log"
}

for mod in qtbase qtshadertools qtdeclarative; do fetch_module "$mod"; done

# qtbase: Gui ON (the whole reason for this tree), everything else that costs
# configure time or host-machine variability OFF. Widgets is off because
# neither qsb nor the QML tools link it; the CROSS build gets Widgets from
# plugins/qt/sysroot, which is a different tree entirely.
build_module qtbase \
    -DFEATURE_gui=ON \
    -DFEATURE_widgets=OFF \
    -DFEATURE_network=OFF \
    -DFEATURE_sql=OFF \
    -DFEATURE_testlib=OFF \
    -DFEATURE_printsupport=OFF \
    -DFEATURE_dbus=OFF \
    -DFEATURE_icu=OFF \
    -DFEATURE_zstd=OFF \
    -DFEATURE_glib=OFF \
    -DFEATURE_openssl=OFF \
    -DFEATURE_vulkan=OFF \
    -DFEATURE_framework=OFF

# qtshadertools: the qsb tool plus bundled glslang and SPIRV-Cross.
build_module qtshadertools

# qtdeclarative: the QML code generators only. qml_animation=OFF takes the
# else() branch of src/CMakeLists.txt:37, so no native QtQuick is built here.
build_module qtdeclarative \
    -DFEATURE_qml_debug=OFF \
    -DFEATURE_qml_animation=OFF

echo
echo "host Qt $QT_VER (native) installed in $HOST_PREFIX"
for t in moc rcc uic syncqt qsb qmlcachegen qmltyperegistrar qmlimportscanner; do
    for d in libexec bin; do
        [ -x "$HOST_PREFIX/$d/$t" ] && { echo "  $d/$t"; break; }
    done
done
