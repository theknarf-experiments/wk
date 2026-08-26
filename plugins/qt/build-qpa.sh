#!/usr/bin/env bash
# Build the wk QPA plugin (plugins/qt/qpa) against the cross Qt in ./sysroot.
#
# Produces sysroot/lib/libqwk.a — a STATIC Qt platform plugin. There is no
# dlopen on wasm, so an app links this archive, names the plugin once with
# Q_IMPORT_PLUGIN(QWkIntegrationPlugin), and selects it with QT_QPA_PLATFORM=wk.
# ./build-smoke.sh is the worked example.
#
# Three things about this build are worth knowing before changing it:
#
#   * OUT OF TREE, on purpose. Everything the plugin needs is already
#     installed in ./sysroot, including Qt6FbSupportPrivate — fbconvenience is
#     built for every platform unconditionally. Building in-tree would mean a
#     qtbase patch plus a qtbase rebuild for every edit to our own code.
#     patches/ is for changes to UPSTREAM Qt; this is not upstream Qt.
#
#   * CMAKE_FIND_ROOT_PATH and CMAKE_PREFIX_PATH must BOTH name the sysroot.
#     wasip2.cmake pins the find root to the wasi sysroot so no host library
#     can leak in, and find_package obeys the find root — without the extra
#     entry you get "Could not find a package configuration file provided by
#     Qt6" while Qt6Config.cmake sits right there. (This is also why
#     wasip2.cmake uses list(APPEND) rather than set() for that variable.)
#
#   * the wasm-opt trap, same as build-qtbase.sh: clang runs wasm-opt as an
#     optional post-link pass and the wasm-opt on PATH cannot parse exnref, so
#     every cmake and ninja invocation runs with it off the PATH. CMake bakes
#     absolute compiler paths into build.ninja, so this must hold for the BUILD
#     step too, not just configure.
set -euo pipefail
cd "$(dirname "$0")"

MISE_SDK="$HOME/.local/share/mise/installs/github-web-assembly-wasi-sdk/wasi-sdk-34-rc.2"
WASI_SDK="${WASI_SDK:-$([ -d "$MISE_SDK" ] && echo "$MISE_SDK" || echo "$HOME/wasi-sdk")}"
EXPECT="wasi-sdk-34-rc.2"
case "$WASI_SDK" in
    *"$EXPECT"*) ;;
    *) echo "qt/build-qpa: expected $EXPECT (set WASI_SDK), got: $WASI_SDK" >&2; exit 1 ;;
esac

SYSROOT="$PWD/sysroot"
HOST_PREFIX="${QT_HOST_PATH:-$PWD/host}"
BUILD="$PWD/build-target/qpa"
GFXCOMPAT="$PWD/../gfx-compat"
LOGDIR="${LOGDIR:-$PWD/logs}"
JOBS="${JOBS:-$(sysctl -n hw.ncpu 2>/dev/null || nproc)}"
BUILD_PATH="$WASI_SDK/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"
mkdir -p "$LOGDIR"

if [ ! -f "$SYSROOT/lib/libQt6Gui.a" ]; then
    echo "qt/build-qpa: no cross Qt in $SYSROOT -- run ./build-qtbase.sh first" >&2
    exit 1
fi
if [ ! -d "$SYSROOT/lib/cmake/Qt6FbSupportPrivate" ]; then
    echo "qt/build-qpa: $SYSROOT has no Qt6FbSupportPrivate package." >&2
    echo "  QFbScreen/QFbWindow/QFbBackingStore ARE the guest-side compositor;" >&2
    echo "  without them this plugin has nothing to composite with." >&2
    exit 1
fi

# wit-bindgen output for the wkgfx world, regenerated every build exactly like
# every other gfx-compat consumer (plugins/gfx-smoke/build.sh is the smallest).
# gen/ is shared and disposable; it is never the source of truth.
echo "=== wit-bindgen (wkgfx world)"
mkdir -p "$GFXCOMPAT/gen"
wit-bindgen c --world wkgfx "$GFXCOMPAT/wit" --out-dir "$GFXCOMPAT/gen"

LOG="$LOGDIR/target-qpa.log"
echo "=== configuring the wk QPA plugin (log: $LOG)"
env PATH="$BUILD_PATH" cmake -G Ninja -S "$PWD/qpa" -B "$BUILD" \
    -DCMAKE_TOOLCHAIN_FILE="$PWD/wasip2.cmake" \
    -DWASI_SDK_PREFIX="$WASI_SDK" \
    -DCMAKE_FIND_ROOT_PATH="$SYSROOT" \
    -DCMAKE_PREFIX_PATH="$SYSROOT" \
    -DQT_HOST_PATH="$HOST_PREFIX" \
    -DCMAKE_INSTALL_PREFIX="$SYSROOT" \
    -DCMAKE_BUILD_TYPE=Release \
    -DWK_GFX_COMPAT="$GFXCOMPAT" \
    2>&1 | tee "$LOG"

echo "=== ninja"
env PATH="$BUILD_PATH" cmake --build "$BUILD" --parallel "$JOBS" 2>&1 | tee -a "$LOG"
env PATH="$BUILD_PATH" cmake --install "$BUILD" 2>&1 | tee -a "$LOG"

echo
ls -l "$SYSROOT/lib/libqwk.a"
