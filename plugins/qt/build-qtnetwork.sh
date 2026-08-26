#!/usr/bin/env bash
# Build plugins/qt/qt-qtnetwork.wasm — the QtNetwork-over-the-fabric asset.
#
# Distinct from ./build-net.sh, and the pair is deliberate:
#
#   qt-net.wasm        raw BSD sockets + QSocketNotifier. Proves the DISPATCHER
#                      wakes on an fd. Deliberately does not link QtNetwork, so
#                      it keeps working whatever happens to the module.
#   qt-qtnetwork.wasm  QHostInfo + QTcpSocket + QNetworkAccessManager. Proves
#                      the MODULE. Needs libQt6Network.a in the sysroot, i.e.
#                      build-qtbase.sh with -DFEATURE_network=ON.
#
# Two things here fail with error messages that read like typos if you forget
# them, so the script checks for both: libQt6Network.a (missing => cmake says
# "Found package configuration file ... but it set Qt6_FOUND to FALSE", naming
# no component) and libqwk.a (missing => an undefined
# qt_static_plugin_QWkIntegrationPlugin).
#
# The wasm-opt trap applies here as everywhere in this plugin: clang runs
# wasm-opt as an optional post-link pass, the wasm-opt on PATH cannot parse
# exnref, and CMake bakes absolute compiler paths into build.ninja — so the
# scrubbed PATH must cover the build step, not just configure.
set -euo pipefail
cd "$(dirname "$0")"

MISE_SDK="$HOME/.local/share/mise/installs/github-web-assembly-wasi-sdk/wasi-sdk-34-rc.2"
WASI_SDK="${WASI_SDK:-$([ -d "$MISE_SDK" ] && echo "$MISE_SDK" || echo "$HOME/wasi-sdk")}"
# Not merely a "use the toolchain we tested with" check. The QPA plugin's
# frame-fd shim (gfx-compat/wkgfx_poll.c) transcribes wasi-libc's PRIVATE
# descriptor-table layout, exactly as plugins/pipe-compat does; against a
# different wasi-libc that is silent memory corruption, not a link error. And
# patches/qtbase-0008 hard-codes which half of <sys/socket.h> wasi-libc
# compiles out.
EXPECT="wasi-sdk-34-rc.2"
case "$WASI_SDK" in
    *"$EXPECT"*) ;;
    *) echo "qt/build-qtnetwork: expected $EXPECT (set WASI_SDK), got: $WASI_SDK" >&2; exit 1 ;;
esac

SYSROOT="$PWD/sysroot"
HOST_PREFIX="${QT_HOST_PATH:-$PWD/host}"
BUILD="$PWD/build-target/qtnetwork"
GFXCOMPAT="$PWD/../gfx-compat"
CLIPCOMPAT="$PWD/../clipboard-compat"
LOGDIR="${LOGDIR:-$PWD/logs}"
JOBS="${JOBS:-$(sysctl -n hw.ncpu 2>/dev/null || nproc)}"
BUILD_PATH="$WASI_SDK/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"
mkdir -p "$LOGDIR"

if [ ! -f "$SYSROOT/lib/libQt6Gui.a" ]; then
    echo "qt/build-qtnetwork: no cross Qt in $SYSROOT -- run ./build-qtbase.sh first" >&2
    exit 1
fi
if [ ! -f "$SYSROOT/lib/libQt6Network.a" ]; then
    echo "qt/build-qtnetwork: $SYSROOT has no libQt6Network.a" >&2
    echo "  build-qtbase.sh must be run with FEATURE_network=ON (it is, since" >&2
    echo "  the network work landed) -- if the sysroot predates that, re-run it" >&2
    echo "  with WK_QT_RECONFIGURE=1." >&2
    exit 1
fi
if [ ! -f "$SYSROOT/lib/libqwk.a" ]; then
    echo "=== libqwk.a missing; building the QPA plugin first"
    WASI_SDK="$WASI_SDK" ./build-qpa.sh
fi

mkdir -p "$GFXCOMPAT/gen"
wit-bindgen c --world wkgfx "$GFXCOMPAT/wit" --out-dir "$GFXCOMPAT/gen" >/dev/null
mkdir -p "$CLIPCOMPAT/gen"
wit-bindgen c --world wkclipboard "$CLIPCOMPAT/wit" --out-dir "$CLIPCOMPAT/gen" >/dev/null

LOG="$LOGDIR/target-qtnetwork.log"
echo "=== configuring qt-qtnetwork (log: $LOG)"
env PATH="$BUILD_PATH" cmake -G Ninja -S "$PWD/qtnetwork" -B "$BUILD" \
    -DCMAKE_TOOLCHAIN_FILE="$PWD/wasip2.cmake" \
    -DWASI_SDK_PREFIX="$WASI_SDK" \
    -DCMAKE_FIND_ROOT_PATH="$SYSROOT" \
    -DCMAKE_PREFIX_PATH="$SYSROOT" \
    -DQT_HOST_PATH="$HOST_PREFIX" \
    -DCMAKE_BUILD_TYPE=Release \
    -DWK_GFX_COMPAT="$GFXCOMPAT" \
    -DWK_CLIP_COMPAT="$CLIPCOMPAT" \
    -DWK_QPA_LIB="$SYSROOT/lib/libqwk.a" \
    2>&1 | tee "$LOG"

echo "=== ninja"
env PATH="$BUILD_PATH" cmake --build "$BUILD" --parallel "$JOBS" 2>&1 | tee -a "$LOG"

# wasip2.cmake leaves CMAKE_EXECUTABLE_SUFFIX empty, and wasm-component-ld
# already made this a COMPONENT at link time.
cp -f "$BUILD/qt-qtnetwork" "$PWD/qt-qtnetwork.wasm"

echo
ls -l "$PWD/qt-qtnetwork.wasm"
echo "built plugins/qt/qt-qtnetwork.wasm"
