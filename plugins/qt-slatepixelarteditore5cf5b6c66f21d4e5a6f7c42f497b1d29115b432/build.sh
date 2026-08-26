#!/usr/bin/env bash
# Build ./slate.wasm — Slate, a real Qt Quick pixel-art editor, as a wk node.
#
#   upstream: https://github.com/mitchcurtis/slate
#   pinned:   e5cf5b6c66f21d4e5a6f7c42f497b1d29115b432 (2026-07-12)
#
# The pin is a COMMIT and not a tag on purpose: Slate's newest tag, v0.9.0
# (2020), still asks for Qt 5.12. The Qt 6 port is unreleased master.
#
# WHAT THIS PLUGIN IS
# -------------------
# Milestone M4 of plugins/qt/PORTING.md — "Quick on the software backend" —
# with Slate as the app. Qt Quick's normal scenegraph is the RHI (OpenGL,
# Vulkan, Metal); a wk node has an RGBA8 framebuffer and nothing else. The
# `software` adaptation renders the whole scene through QPainter instead, and
# it renders QQuickPaintedItem natively — which is what Slate's canvas, rulers,
# cursors, guides and selection overlays are. That is why Slate is the right
# app for this: its drawing surface is not a shader.
#
# THE PIECES, in the order they are needed:
#
#   plugins/qt/sysroot        the cross qtbase (Core/Gui/Widgets) + libqwk.a,
#                             the wk QPA plugin. Built by plugins/qt; we only
#                             read it.
#   ./host                    a native Qt 6.8.4 WITH QtGui, so that qsb exists.
#                             ./build-hosttools.sh explains at length why
#                             plugins/qt/host cannot be used for Qt Quick.
#   ./sysroot                 the cross qtdeclarative (QtQml, QtQuick, Quick
#                             Controls) installed by ./build-qtdeclarative.sh.
#                             A SEPARATE PREFIX from plugins/qt/sysroot, which
#                             is why every cmake line below carries two paths.
#                             CMAKE_PREFIX_PATH is NOT enough on its own:
#                             Qt6Config.cmake resolves COMPONENTS with
#                             NO_DEFAULT_PATH against its own prefix plus
#                             QT_ADDITIONAL_PACKAGES_PREFIX_PATH
#                             (Qt6Config.cmake:137-225), so without that third
#                             variable find_package(Qt6 COMPONENTS Qml) fails
#                             with "Expected Config file at
#                             plugins/qt/sysroot/.../Qt6QmlConfig.cmake does
#                             NOT exist" -- looking in the wrong prefix
#                             entirely. It is the supported way to install Qt
#                             modules into separate prefixes.
#   ./node                    our wrapper: upstream is add_subdirectory()'d and
#                             the wk-specific link lines are attached to its
#                             `app` target. See node/CMakeLists.txt.
#   ./patches                 the four changes to upstream Slate, each with the
#                             reason it exists in its header.
#
# TRAPS INHERITED FROM plugins/qt (all argued in plugins/qt/wasip2.cmake, which
# this build uses unchanged): every object needs the exnref EH flags; wasm-opt
# must not be on PATH because it cannot parse exnref and clang runs it as an
# optional post-link pass; no LTO; 8 MB stack.
#
# Long the first time (the two Qt halves are ~40 minutes together); minutes
# after that. Run it detached.
set -euo pipefail
cd "$(dirname "$0")"

MISE_SDK="$HOME/.local/share/mise/installs/github-web-assembly-wasi-sdk/wasi-sdk-34-rc.2"
WASI_SDK="${WASI_SDK:-$([ -d "$MISE_SDK" ] && echo "$MISE_SDK" || echo "$HOME/wasi-sdk")}"
EXPECT="wasi-sdk-34-rc.2"
case "$WASI_SDK" in
    *"$EXPECT"*) ;;
    *) echo "qt-slate: expected $EXPECT (set WASI_SDK), got: $WASI_SDK" >&2; exit 1 ;;
esac
export WASI_SDK

SLATE_COMMIT=e5cf5b6c66f21d4e5a6f7c42f497b1d29115b432
# lib/3rdparty/gif-h is a git SUBMODULE (.gitmodules ->
# git@github.com:mitchcurtis/gif-h.git) and a GitHub source tarball contains
# submodules as empty directories. Slate's lib/CMakeLists.txt lists
# 3rdparty/gif-h/qt-cpp/gifwriter.cpp unconditionally, so configure dies with
# "Cannot find source file". This is the SHA the pinned Slate commit records
# for it, read from
#   api.github.com/repos/mitchcurtis/slate/contents/lib/3rdparty?ref=$SLATE_COMMIT
# (a "submodule" entry's sha), which is the only way to recover it without a
# git checkout. It is a header-only GIF encoder plus a small Qt wrapper; Slate
# uses it for "export animation as GIF".
GIFH_COMMIT=7373cfa720e3126c6063c2827783ad0fdf29ffe7
QTBASE_PLUGIN="$PWD/../qt"
QTBASE_SYSROOT="$QTBASE_PLUGIN/sysroot"
QPA_LIB="$QTBASE_SYSROOT/lib/libqwk.a"
QUICK_SYSROOT="$PWD/sysroot"
HOST_PREFIX="${QT_HOST_PATH:-$PWD/host}"
GFXCOMPAT="$PWD/../gfx-compat"
SRCDIR="$PWD/src"
TARBALLS="$PWD/tarballs"
SLATE_SRC="$SRCDIR/slate-$SLATE_COMMIT"
PATCHDIR="$PWD/patches"
BUILD="$PWD/build-target/node"
FONTDIR="$PWD/node/fonts"
GENDIR="$PWD/gen"
LOGDIR="${LOGDIR:-$PWD/logs}"
JOBS="${JOBS:-$(sysctl -n hw.ncpu 2>/dev/null || nproc)}"
BUILD_PATH="$WASI_SDK/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"
mkdir -p "$SRCDIR" "$TARBALLS" "$LOGDIR" "$FONTDIR" "$GENDIR"

# --- prerequisites ----------------------------------------------------------
if [ ! -f "$QTBASE_SYSROOT/lib/libQt6Widgets.a" ]; then
    echo "qt-slate: no cross qtbase in $QTBASE_SYSROOT" >&2
    echo "  run plugins/qt/build-qtbase.sh (it is the shared Qt for wasm)." >&2
    exit 1
fi
if [ ! -f "$QPA_LIB" ]; then
    echo "qt-slate: no wk QPA plugin at $QPA_LIB" >&2
    echo "  run plugins/qt/build-qpa.sh. Without it the app links but has no" >&2
    echo "  platform plugin, and QApplication aborts with an EMPTY list of" >&2
    echo "  available platforms." >&2
    exit 1
fi
[ -x "$HOST_PREFIX/bin/qsb" ] || ./build-hosttools.sh
[ -f "$QUICK_SYSROOT/lib/libQt6Quick.a" ] || ./build-qtdeclarative.sh

# --- upstream, fetched not vendored -----------------------------------------
if [ ! -d "$SLATE_SRC" ]; then
    tar_path="$TARBALLS/slate-$SLATE_COMMIT.tar.gz"
    if [ ! -f "$tar_path" ]; then
        echo "fetching slate $SLATE_COMMIT..."
        curl -fsSL --retry 3 -o "$tar_path.part" \
            "https://github.com/mitchcurtis/slate/archive/$SLATE_COMMIT.tar.gz"
        mv "$tar_path.part" "$tar_path"
    fi
    echo "extracting slate..."
    tar xzf "$tar_path" -C "$SRCDIR"
fi

if [ ! -f "$SLATE_SRC/lib/3rdparty/gif-h/qt-cpp/gifwriter.cpp" ]; then
    gif_tar="$TARBALLS/gif-h-$GIFH_COMMIT.tar.gz"
    if [ ! -f "$gif_tar" ]; then
        echo "fetching gif-h $GIFH_COMMIT (slate submodule)..."
        curl -fsSL --retry 3 -o "$gif_tar.part" \
            "https://github.com/mitchcurtis/gif-h/archive/$GIFH_COMMIT.tar.gz"
        mv "$gif_tar.part" "$gif_tar"
    fi
    echo "extracting gif-h..."
    rm -rf "$SLATE_SRC/lib/3rdparty/gif-h"
    tar xzf "$gif_tar" -C "$SLATE_SRC/lib/3rdparty"
    mv "$SLATE_SRC/lib/3rdparty/gif-h-$GIFH_COMMIT" "$SLATE_SRC/lib/3rdparty/gif-h"
fi

# patches/slate-NNNN-*.patch, -p1 at the Slate source root. Reverse-checkable so
# a re-run is a no-op, the same convention plugins/qt/patches uses.
for p in "$PATCHDIR"/slate-*.patch; do
    [ -e "$p" ] || continue
    if git -C "$SLATE_SRC" apply --reverse --check "$p" >/dev/null 2>&1; then
        echo "  patch (already applied): $(basename "$p")"
        continue
    fi
    echo "  patch: $(basename "$p")"
    git -C "$SLATE_SRC" apply "$p"
done

# --- a font -----------------------------------------------------------------
# Qt 6 ships none and a wk node has no host font directory, so a component
# built without one runs and renders every string as nothing at all. Slate does
# carry Roboto and FontAwesome in its own .qrc, but those are the fonts it asks
# for BY NAME; the platform's DEFAULT family is a separate question, and
# picking it out of :/fonts would land on FontAwesome. So we stage one text
# font of our own under :/wkfonts. See node/wkslate.cpp.
#
# Nothing is fetched: a build must not depend on a font download. First hit
# wins; the staged copy is gitignored because it is somebody else's font.
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
    echo "qt-slate: no font found. Tried:" >&2
    printf '  %s\n' "${FONT_CANDIDATES[@]}" >&2
    echo "  Put a .ttf in node/fonts/ and re-run, or extend FONT_CANDIDATES." >&2
    exit 1
fi
STAGED="$FONTDIR/$(basename "$FONT")"
cp -f "$FONT" "$STAGED"
echo "=== font: $FONT -> node/fonts/$(basename "$FONT")"

# --- wasi-gfx bindings ------------------------------------------------------
# Only wkgfx_component_type.o is needed on the link line: libqwk.a already
# carries the compiled shim and bindings. Generated into OUR gen/ rather than
# plugins/gfx-compat/gen/ so that two plugins building at once cannot race over
# the same output directory.
echo "=== wit-bindgen (wkgfx world)"
wit-bindgen c --world wkgfx "$GFXCOMPAT/wit" --out-dir "$GENDIR" >/dev/null

# --- configure + build ------------------------------------------------------
LOG="$LOGDIR/target-slate.log"
echo "=== configuring slate (log: $LOG)"
env PATH="$BUILD_PATH" cmake -G Ninja -S "$PWD/node" -B "$BUILD" \
    -DCMAKE_TOOLCHAIN_FILE="$QTBASE_PLUGIN/wasip2.cmake" \
    -DWASI_SDK_PREFIX="$WASI_SDK" \
    -DCMAKE_FIND_ROOT_PATH="$QTBASE_SYSROOT;$QUICK_SYSROOT" \
    -DCMAKE_PREFIX_PATH="$QTBASE_SYSROOT;$QUICK_SYSROOT" \
    -DQT_ADDITIONAL_PACKAGES_PREFIX_PATH="$QUICK_SYSROOT" \
    -DQT_HOST_PATH="$HOST_PREFIX" \
    -DQT_HOST_PATH_CMAKE_DIR="$HOST_PREFIX/lib/cmake" \
    -DCMAKE_BUILD_TYPE=Release \
    -DSLATE_SRC="$SLATE_SRC" \
    -DWK_GFX_COMPAT="$GFXCOMPAT" \
    -DWK_QPA_LIB="$QPA_LIB" \
    -DWK_FONT="$STAGED" \
    2>&1 | tee "$LOG"

echo "=== ninja"
env PATH="$BUILD_PATH" cmake --build "$BUILD" --parallel "$JOBS" 2>&1 | tee -a "$LOG"

# wasip2.cmake leaves CMAKE_EXECUTABLE_SUFFIX empty (Qt's architecture config
# test depends on it), so the linked artifact is `app` with no extension.
# wasm-component-ld already made it a COMPONENT at link time — no adapter, no
# `wasm-tools component new` step.
cp -f "$BUILD/slate-upstream/app/app" "$PWD/slate.wasm"

echo
ls -l "$PWD/slate.wasm"
echo "built plugins/$(basename "$PWD")/slate.wasm"
