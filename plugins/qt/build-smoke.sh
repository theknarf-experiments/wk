#!/usr/bin/env bash
# Build plugins/qt/qt-smoke.wasm — a real Qt Widgets app as a wk node.
#
# Depends on ./build-qtbase.sh (the cross Qt in ./sysroot) and ./build-qpa.sh
# (libqwk.a, the wk platform plugin). Runs ./build-qpa.sh itself if the
# archive is missing, because forgetting it produces a link error about
# qt_static_plugin_QWkIntegrationPlugin that reads like a typo.
#
# FONTS. Qt 6 ships none, and a wk node has no host font directory to fall
# back on: with no font the app runs, QFontDatabase is empty, and every string
# renders as nothing at all. So this script STAGES one TTF into smoke/fonts/
# and CMake compiles it into the component as a Qt resource under :/fonts,
# which is where QWkFontDatabase looks last. The staged copy is gitignored —
# it is somebody else's font, and which one exists depends on the machine.
# A node that mounts a real font directory can still set QT_QPA_FONTDIR and
# skip the resource entirely.
#
# The wasm-opt trap applies here as everywhere in this plugin: clang runs
# wasm-opt as an optional post-link pass, the wasm-opt on PATH cannot parse
# exnref, and CMake bakes absolute compiler paths into build.ninja — so the
# scrubbed PATH must cover the build step, not just configure.
set -euo pipefail
cd "$(dirname "$0")"

MISE_SDK="$HOME/.local/share/mise/installs/github-web-assembly-wasi-sdk/wasi-sdk-34-rc.2"
WASI_SDK="${WASI_SDK:-$([ -d "$MISE_SDK" ] && echo "$MISE_SDK" || echo "$HOME/wasi-sdk")}"
EXPECT="wasi-sdk-34-rc.2"
case "$WASI_SDK" in
    *"$EXPECT"*) ;;
    *) echo "qt/build-smoke: expected $EXPECT (set WASI_SDK), got: $WASI_SDK" >&2; exit 1 ;;
esac

SYSROOT="$PWD/sysroot"
HOST_PREFIX="${QT_HOST_PATH:-$PWD/host}"
BUILD="$PWD/build-target/smoke"
GFXCOMPAT="$PWD/../gfx-compat"
CLIPCOMPAT="$PWD/../clipboard-compat"
FONTDIR="$PWD/smoke/fonts"
LOGDIR="${LOGDIR:-$PWD/logs}"
JOBS="${JOBS:-$(sysctl -n hw.ncpu 2>/dev/null || nproc)}"
BUILD_PATH="$WASI_SDK/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"
mkdir -p "$LOGDIR" "$FONTDIR"

if [ ! -f "$SYSROOT/lib/libQt6Widgets.a" ]; then
    echo "qt/build-smoke: no cross Qt in $SYSROOT -- run ./build-qtbase.sh first" >&2
    exit 1
fi
if [ ! -f "$SYSROOT/lib/libqwk.a" ]; then
    echo "=== libqwk.a missing; building the QPA plugin first"
    WASI_SDK="$WASI_SDK" ./build-qpa.sh
fi

# --- stage a font --------------------------------------------------------
# First hit wins. Repo-local candidates come first so a machine-independent
# font is preferred when one is present; the macOS and Linux system paths are
# the fallback. Nothing here is fetched from the network: a build must not
# depend on a font download.
FONT_CANDIDATES=(
    "$PWD/../doctools/tex/texlive-source/libs/gd/libgd-src/tests/freetype/DejaVuSans.ttf"
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
    echo "qt/build-smoke: no font found. Tried:" >&2
    printf '  %s\n' "${FONT_CANDIDATES[@]}" >&2
    echo "  Qt 6 ships no fonts and a wk node has no host font dir, so a" >&2
    echo "  component built without one renders every string as nothing." >&2
    echo "  Put a .ttf at smoke/fonts/ and re-run, or extend FONT_CANDIDATES." >&2
    exit 1
fi
STAGED="$FONTDIR/$(basename "$FONT")"
cp -f "$FONT" "$STAGED"
echo "=== font: $FONT -> smoke/fonts/$(basename "$FONT")"

# --- bindings ------------------------------------------------------------
mkdir -p "$GFXCOMPAT/gen"
wit-bindgen c --world wkgfx "$GFXCOMPAT/wit" --out-dir "$GFXCOMPAT/gen" >/dev/null
mkdir -p "$CLIPCOMPAT/gen"
wit-bindgen c --world wkclipboard "$CLIPCOMPAT/wit" --out-dir "$CLIPCOMPAT/gen" >/dev/null

# --- configure + build ---------------------------------------------------
LOG="$LOGDIR/target-smoke.log"
echo "=== configuring qt-smoke (log: $LOG)"
env PATH="$BUILD_PATH" cmake -G Ninja -S "$PWD/smoke" -B "$BUILD" \
    -DCMAKE_TOOLCHAIN_FILE="$PWD/wasip2.cmake" \
    -DWASI_SDK_PREFIX="$WASI_SDK" \
    -DCMAKE_FIND_ROOT_PATH="$SYSROOT" \
    -DCMAKE_PREFIX_PATH="$SYSROOT" \
    -DQT_HOST_PATH="$HOST_PREFIX" \
    -DCMAKE_BUILD_TYPE=Release \
    -DWK_GFX_COMPAT="$GFXCOMPAT" \
    -DWK_CLIP_COMPAT="$CLIPCOMPAT" \
    -DWK_QPA_LIB="$SYSROOT/lib/libqwk.a" \
    -DWK_SMOKE_FONT="$STAGED" \
    2>&1 | tee "$LOG"

echo "=== ninja"
env PATH="$BUILD_PATH" cmake --build "$BUILD" --parallel "$JOBS" 2>&1 | tee -a "$LOG"

# wasip2.cmake deliberately leaves CMAKE_EXECUTABLE_SUFFIX empty (Qt's
# architecture config test depends on it), so the linked artifact is `qt-smoke`
# with no extension. wasm-component-ld already made it a COMPONENT at link
# time — the wasip1 + adapter + `wasm-tools component new` dance that
# plugins/gfx-smoke does is not needed on this target.
cp -f "$BUILD/qt-smoke" "$PWD/qt-smoke.wasm"

echo
ls -l "$PWD/qt-smoke.wasm"
echo "built plugins/qt/qt-smoke.wasm"
