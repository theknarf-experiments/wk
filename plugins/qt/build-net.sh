#!/usr/bin/env bash
# Build plugins/qt/qt-net.wasm — the QSocketNotifier test asset.
#
# A Qt node that waits on a TCP socket and nothing else, so that the ONE thing
# that can wake its event loop is the fd. It is what the wk-server test
# `qt_socket_notifier_wakes_on_the_fabric` runs against plugins/netserve; see
# net/main.cpp for the argument, and qpa/qwkeventdispatcher.h for how the
# frame, the timers and the sockets end up in a single wasi:io/poll.
#
# Same shape as ./build-smoke.sh, minus the font staging (this node never
# paints, so it needs no glyphs). Depends on ./build-qpa.sh for libqwk.a and
# runs it if the archive is missing, because forgetting produces a link error
# about qt_static_plugin_QWkIntegrationPlugin that reads like a typo.
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
# different wasi-libc that is silent memory corruption, not a link error.
EXPECT="wasi-sdk-34-rc.2"
case "$WASI_SDK" in
    *"$EXPECT"*) ;;
    *) echo "qt/build-net: expected $EXPECT (set WASI_SDK), got: $WASI_SDK" >&2; exit 1 ;;
esac

SYSROOT="$PWD/sysroot"
HOST_PREFIX="${QT_HOST_PATH:-$PWD/host}"
BUILD="$PWD/build-target/net"
GFXCOMPAT="$PWD/../gfx-compat"
CLIPCOMPAT="$PWD/../clipboard-compat"
LOGDIR="${LOGDIR:-$PWD/logs}"
JOBS="${JOBS:-$(sysctl -n hw.ncpu 2>/dev/null || nproc)}"
BUILD_PATH="$WASI_SDK/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"
mkdir -p "$LOGDIR"

if [ ! -f "$SYSROOT/lib/libQt6Gui.a" ]; then
    echo "qt/build-net: no cross Qt in $SYSROOT -- run ./build-qtbase.sh first" >&2
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

LOG="$LOGDIR/target-net.log"
echo "=== configuring qt-net (log: $LOG)"
env PATH="$BUILD_PATH" cmake -G Ninja -S "$PWD/net" -B "$BUILD" \
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
cp -f "$BUILD/qt-net" "$PWD/qt-net.wasm"

echo
ls -l "$PWD/qt-net.wasm"
echo "built plugins/qt/qt-net.wasm"
