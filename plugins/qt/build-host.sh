#!/usr/bin/env bash
# Build the NATIVE host half of the Qt 6.8.4 port: plugins/qt/host.
#
# Cross-building Qt is not optional about this. qtbase/cmake/QtBuildHelpers.cmake
# does
#     if(NOT IS_DIRECTORY "${QT_HOST_PATH}")
#         message(FATAL_ERROR "You need to set QT_HOST_PATH to cross compile Qt.")
# and the host tree must be the SAME Qt version as the target tree, because the
# generated moc/rcc/qmltyperegistrar output is version-coupled. That is why we
# build it from the same 6.8.4 sources in ./src rather than using Homebrew's
# qt@6 (which is whatever version brew ships this month) — a mismatch shows up
# as baffling "Unknown property" / metatype errors deep in the target build.
#
# What lands in ./host:
#   from qtbase        moc, rcc, uic, qmake, syncqt, qlalr, qvkgen, qtpaths,
#                      cmake_automoc_parser, tracegen + the Qt6*Tools CMake
#                      packages the target build find_package()s
#   from qtdeclarative qmlcachegen, qmltyperegistrar, qmlimportscanner, qmllint,
#                      qmlformat (needed the moment qtdeclarative is
#                      cross-built for M4)
#
# Kept as short as we can make it: Release, static, examples/tests/benchmarks
# off, and every optional module off. Notably FEATURE_gui=OFF — qtdeclarative
# REQUIRES only BuildInternals+Core (its CMakeLists.txt:16 lists Gui, Network,
# Widgets, ... as OPTIONAL_COMPONENTS), so a GUI-less host still produces the
# QML tools; it just skips QtQuick, which nothing on the host needs.
#
# This script touches NOTHING wasm: no wasi-sdk, no toolchain file. It is
# ordinary native clang. (The WASI_SDK guard below exists only so that running
# it via `mise run build` — which exports WASI_SDK for the sibling scripts —
# fails loudly on a toolchain mismatch instead of half-building.)
#
# Idempotent: re-running is a no-op once ./host contains the tools. Set
# WK_QT_FORCE=1 to reconfigure and rebuild anyway.
set -euo pipefail
cd "$(dirname "$0")"

# --- toolchain guard (same shape as plugins/mupdf/build.sh) -----------------
# Not used to compile anything here, but every script in this plugin agrees on
# one SDK so `mise run build` can't silently mix two.
MISE_SDK="$HOME/.local/share/mise/installs/github-web-assembly-wasi-sdk/wasi-sdk-34-rc.2"
WASI_SDK="${WASI_SDK:-$([ -d "$MISE_SDK" ] && echo "$MISE_SDK" || echo "$HOME/wasi-sdk")}"
EXPECT="wasi-sdk-34-rc.2"
case "$WASI_SDK" in
    *"$EXPECT"*) ;;
    *)
        echo "qt/build-host: expected $EXPECT (set WASI_SDK), got: $WASI_SDK" >&2
        exit 1
        ;;
esac

QT_VER=6.8.4
QT_SERIES=6.8
SRCDIR="$PWD/src"
TARBALLS="$PWD/tarballs"
BUILDDIR="$PWD/build-host"
HOST_PREFIX="$PWD/host"
PATCHDIR="$PWD/patches"
LOGDIR="${LOGDIR:-$PWD/logs}"
JOBS="${JOBS:-$(sysctl -n hw.ncpu 2>/dev/null || nproc)}"
mkdir -p "$SRCDIR" "$TARBALLS" "$BUILDDIR" "$LOGDIR"

# The host modules, in dependency order. qtdeclarative needs a built qtbase in
# ./host to find_package(Qt6 BuildInternals Core), so the order is load-bearing.
HOST_MODULES="${WK_QT_HOST_MODULES:-qtbase qtdeclarative}"

# --- upstream, fetched not vendored -----------------------------------------
# House rule (see plugins/netsurf, plugins/mupdf): build.sh fetches upstream,
# git never carries it. Note the tarball is named "-everywhere-opensource-src-"
# on download.qt.io but extracts to "-everywhere-src-" — verified, not a typo.
fetch_module() {
    local mod="$1"
    local dir="$SRCDIR/$mod-everywhere-src-$QT_VER"
    local tar="$TARBALLS/$mod-everywhere-opensource-src-$QT_VER.tar.xz"
    if [ -d "$dir" ]; then return 0; fi
    if [ ! -f "$tar" ]; then
        echo "fetching $mod $QT_VER..."
        curl -fsSL --retry 3 -o "$tar.part" \
            "https://download.qt.io/archive/qt/$QT_SERIES/$QT_VER/submodules/$(basename "$tar")"
        mv "$tar.part" "$tar"
    fi
    echo "extracting $mod $QT_VER..."
    tar xJf "$tar" -C "$SRCDIR"
}

# --- patches ----------------------------------------------------------------
# patches/<module>-NNNN-*.patch, -p1 against that module's source root. Applied
# here too (not only in build-qtbase.sh) because ./src is ONE tree shared by the
# host and target builds and either script may run first. Every patch is written
# to be inert off wasm (new files, or guards on Q_OS_WASI / CMAKE_SYSTEM_NAME
# WASI), so the host build is unaffected by them. See patches/README.md.
apply_patches() {
    local mod="$1"
    local tree="$SRCDIR/$mod-everywhere-src-$QT_VER"
    local p
    [ -d "$PATCHDIR" ] || return 0
    for p in "$PATCHDIR/$mod"-*.patch; do
        [ -e "$p" ] || continue
        # Already applied? Then the reverse patch applies cleanly. This is what
        # makes the script re-runnable against a tree it already patched.
        if git -C "$tree" apply --reverse --check "$p" >/dev/null 2>&1; then
            echo "  patch (already applied): $(basename "$p")"
            continue
        fi
        echo "  patch: $(basename "$p")"
        git -C "$tree" apply "$p"
    done
}

# One configure+build+install of a host module. Common flags here, the
# per-module ones come in as "$@".
#
# QT_NO_APPLE_SDK_MAX_VERSION_CHECK: Qt 6.8.4 was tested against the macOS 15
# SDK and prints a ten-line warning about anything newer (26.2 on this
# machine). Nothing on this build path cares — the host tree produces code
# generators, not frameworks — and the noise hides real warnings.
build_module() {
    local mod="$1"
    local src="$SRCDIR/$mod-everywhere-src-$QT_VER"
    local bld="$BUILDDIR/$mod"
    local log="$LOGDIR/host-$mod.log"
    shift

    echo "=== host $mod $QT_VER -> $HOST_PREFIX (log: $log)"
    if [ ! -f "$bld/CMakeCache.txt" ] || [ -n "${WK_QT_FORCE:-}" ]; then
        cmake -G Ninja -S "$src" -B "$bld" \
            -DCMAKE_INSTALL_PREFIX="$HOST_PREFIX" \
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

# Skip the whole thing when the deliverables are already in place. moc proves
# qtbase installed; qmlcachegen proves qtdeclarative did. Both are looked up in
# libexec/ AND bin/: verified on this build, Qt 6.8.4 installs the code
# generators (moc, rcc, uic, syncqt, qmltyperegistrar, qmlcachegen,
# qmlimportscanner) into libexec/ and only the user-facing tools (qmake,
# qtpaths, qml, qmllint, qmlformat) into bin/. Checking bin/moc alone made this
# function always return false, so the script re-ran ninja+install every time.
host_is_complete() {
    [ -z "${WK_QT_FORCE:-}" ] || return 1
    [ -x "$HOST_PREFIX/libexec/moc" ] || [ -x "$HOST_PREFIX/bin/moc" ] || return 1
    case "$HOST_MODULES" in
        *qtdeclarative*)
            [ -x "$HOST_PREFIX/libexec/qmlcachegen" ] ||
            [ -x "$HOST_PREFIX/bin/qmlcachegen" ] || return 1
            ;;
    esac
    return 0
}
if host_is_complete; then
    echo "qt/build-host: $HOST_PREFIX already populated (WK_QT_FORCE=1 to rebuild)"
    exit 0
fi

for mod in $HOST_MODULES; do
    fetch_module "$mod"
    apply_patches "$mod"
done

for mod in $HOST_MODULES; do
    case "$mod" in
    qtbase)
        # A tools-only qtbase. Everything here is off because the host needs
        # code generators, not libraries:
        #   gui/widgets/network/sql/testlib/printsupport/dbus  — unused by any
        #     host tool we want; QtGui alone would drag in the whole Cocoa QPA.
        #   opengl/vulkan/icu/zstd/glib/openssl — external probes that only add
        #     configure time and host-machine variability.
        #   sql/odbc/psql/... are covered by FEATURE_sql=OFF.
        # uic is built unconditionally (src/tools/CMakeLists.txt:8) and
        # moc/rcc/syncqt/tracegen/cmake_automoc_parser come from
        # src/CMakeLists.txt:16-26 — none of them need Gui.
        build_module qtbase \
            -DFEATURE_gui=OFF \
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
            -DFEATURE_opengl=OFF \
            -DFEATURE_vulkan=OFF \
            -DFEATURE_framework=OFF
        ;;
    qtdeclarative)
        # QT_HOST_PATH is not used here — this IS the host build; qtdeclarative
        # finds qtbase through CMAKE_PREFIX_PATH. FEATURE_qml_debug=OFF trims
        # the QML debug server we will never run on the host.
        build_module qtdeclarative \
            -DCMAKE_PREFIX_PATH="$HOST_PREFIX" \
            -DFEATURE_qml_debug=OFF
        ;;
    *)
        build_module "$mod" -DCMAKE_PREFIX_PATH="$HOST_PREFIX"
        ;;
    esac
done

echo
echo "host Qt $QT_VER installed in $HOST_PREFIX"
# Print whichever of libexec/ or bin/ each tool actually landed in, instead of
# guessing one. The previous fixed list guessed bin/ for moc, rcc, uic and
# qmlimportscanner and so silently printed nothing for the four tools that
# matter most.
for t in moc rcc uic syncqt qmake qmlcachegen qmltyperegistrar qmlimportscanner; do
    for d in libexec bin; do
        [ -x "$HOST_PREFIX/$d/$t" ] && { echo "  $d/$t"; break; }
    done
done
echo "pass this as QT_HOST_PATH to build-qtbase.sh (it does so automatically)."
