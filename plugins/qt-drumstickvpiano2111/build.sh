#!/usr/bin/env bash
# drumstick-vpiano 2.11.1 — a REAL Qt 6 MIDI application as a wk node, wired
# into wk's MIDI fabric.
#
# Produces ./drumstick-vpiano.wasm: a wasm32-wasip2 COMPONENT that imports
# BOTH wasi:surface (it paints its window into the one RGBA8 surface wk gives
# a node, through the wk QPA plugin) AND wk:midi/midi (it sends the notes you
# click and lights up the notes another node sends it). wk gives any component
# importing wk:midi/midi a pair of MIDI ports on the canvas — see
# crates/wk-server/src/plugin.rs `component_imports_midi` — so the node comes
# up wireable with no server change at all.
#
# WHAT THIS SCRIPT IS, in one line: one cross-build and a link. Unlike
# plugins/qt-torrentfileeditor103 there is NO port-local Qt sysroot: drumstick
# needs nothing beyond the Core/Gui/Widgets already in plugins/qt/sysroot.
# Drumstick::RT links Qt Core alone, Drumstick::Widgets links Qt Widgets alone,
# the PianoKeybd resource is PNGs (so no Qt Svg), and Core5Compat is only
# wanted by Drumstick::File, which BUILD_FILE=OFF turns off.
#
# THE PORT IN ONE PARAGRAPH. Drumstick reaches MIDI hardware through plugins
# implementing MIDIInput/MIDIOutput, and on this platform every backend it
# ships is gated off — so patches/0002 adds a `wk` pair over wk:midi, modelled
# line for line on upstream's own net-in/net-out. The hard part was that
# drumstick's input backends are all either threaded or QSocketNotifier-based
# and neither exists here: FEATURE_thread=OFF, and wk:midi has no pollable at
# all (`input.receive()` is a non-blocking queue pop). wk-in therefore pumps on
# a 1 ms Qt::PreciseTimer, which costs nothing extra because QWkEventDispatcher
# already folds Qt's timer deadline into its single wasi:io/poll frame wait.
# Read patches/0002's header before changing any of that.
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
#   * no threads (FEATURE_thread=OFF). vpiano is the rare Qt app that does not
#     care: it has no worker and no transport of its own, it is purely
#     reactive. Nothing degrades;
#   * no dlopen: the wk QPA plugin and the two wk MIDI backends are all STATIC
#     archives named with Q_IMPORT_PLUGIN in vpianomain.cpp (patches/0003).
#
# FONTS. Qt 6 ships none and a wk node has no host font directory: with no font
# the app runs, QFontDatabase is empty, and every string renders as nothing at
# all — including the note name on every key. So this script STAGES one TTF
# into ./fonts/ and patches/0003 compiles it in as a Qt resource under :/fonts,
# which QWkFontDatabase falls back to. The staged copy is gitignored — it is
# somebody else's font.
#
# Knobs: JOBS=N   LOGDIR=...   WK_VPIANO_RECONFIGURE=1   QT_HOST_PATH=...
#
# Budget ~5-10 minutes cold (drumstick itself is small; the Qt link is the
# slow part). Run it detached and tail ./logs.
set -euo pipefail
cd "$(dirname "$0")"

# --- toolchain guard (same shape as plugins/qt-torrentfileeditor103) ---------
MISE_SDK="$HOME/.local/share/mise/installs/github-web-assembly-wasi-sdk/wasi-sdk-34-rc.2"
WASI_SDK="${WASI_SDK:-$([ -d "$MISE_SDK" ] && echo "$MISE_SDK" || echo "$HOME/wasi-sdk")}"
EXPECT="wasi-sdk-34-rc.2"
case "$WASI_SDK" in
    *"$EXPECT"*) ;;
    *) echo "qt-vpiano: expected $EXPECT (set WASI_SDK), got: $WASI_SDK" >&2; exit 1 ;;
esac

DS_TAG=RELEASE_2_11_1

HERE="$PWD"
QTPLUGIN="$PWD/../qt"                 # the shared Qt port: qtbase + the wk QPA
QTBASE_SYSROOT="$QTPLUGIN/sysroot"
HOST_PREFIX="${QT_HOST_PATH:-$QTPLUGIN/host}"
GFXCOMPAT="$PWD/../gfx-compat"
MIDICOMPAT="$PWD/../midi-compat"
CLIPCOMPAT="$PWD/../clipboard-compat"

SRCDIR="$PWD/src"
TARBALLS="$PWD/tarballs"
PATCHDIR="$PWD/patches"
SHIMDIR="$PWD/shim"                   # OUR wk:midi C shim, both directions
BUILD="$PWD/build"
GEN="$PWD/gen"                        # our own wit-bindgen output
FONTDIR="$PWD/fonts"
LOGDIR="${LOGDIR:-$PWD/logs}"
JOBS="${JOBS:-$(sysctl -n hw.ncpu 2>/dev/null || nproc)}"
BUILD_PATH="$WASI_SDK/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"
NM="$WASI_SDK/bin/llvm-nm"
mkdir -p "$SRCDIR" "$TARBALLS" "$LOGDIR" "$BUILD" "$GEN" "$FONTDIR"

# --- preflight --------------------------------------------------------------
# plugins/qt is NOT a mise `depends` of this plugin (see mise.toml): naming it
# would make a repo-wide `mise run build-plugins` sweep silently start an
# hours-long Qt build. So say plainly what is missing instead.
if [ ! -f "$QTBASE_SYSROOT/lib/libQt6Widgets.a" ]; then
    echo "qt-vpiano: no cross Qt in $QTBASE_SYSROOT" >&2
    echo "  run plugins/qt/build-qtbase.sh first (it is the long one)." >&2
    exit 1
fi
if [ ! -f "$QTBASE_SYSROOT/lib/libqwk.a" ]; then
    echo "qt-vpiano: no wk QPA plugin at $QTBASE_SYSROOT/lib/libqwk.a" >&2
    echo "  run plugins/qt/build-qpa.sh first. Without it the app links but" >&2
    echo "  QApplication aborts with 'no Qt platform plugin could be" >&2
    echo "  initialized' and an EMPTY plugin list." >&2
    exit 1
fi
if [ ! -x "$HOST_PREFIX/libexec/moc" ] && [ ! -x "$HOST_PREFIX/bin/moc" ]; then
    echo "qt-vpiano: no host Qt at $HOST_PREFIX -- run plugins/qt/build-host.sh" >&2
    exit 1
fi
# pkg-config, passed EXPLICITLY. drumstick's top-level CMakeLists has an
# unconditional `find_package(PkgConfig REQUIRED)`, and inside a cross build
# that find_program fails even with pkg-config plainly on PATH: something
# between wasip2.cmake's CMAKE_FIND_ROOT_PATH_MODE_PROGRAM=NEVER and Qt's own
# find-root handling puts it back to ONLY, so the search is confined to the Qt
# sysroot, where of course there is no host binary. Naming the executable
# sidesteps the whole question and needs no patch to upstream.
#
# Nothing here actually USES pkg-config: its only callers in drumstick are the
# ALSA, PulseAudio and PipeWire probes, all of which are off. It is a
# REQUIRED-check and nothing more.
PKGCONFIG="${PKG_CONFIG:-$(command -v pkg-config 2>/dev/null || true)}"
if [ -z "$PKGCONFIG" ]; then
    echo "qt-vpiano: no pkg-config on PATH. drumstick's CMakeLists requires the" >&2
    echo "  program even though every consumer of it is switched off here." >&2
    echo "  brew install pkg-config (or set PKG_CONFIG=/path/to/pkg-config)." >&2
    exit 1
fi

# --- upstream, fetched not vendored -----------------------------------------
DS_SRC="$SRCDIR/drumstick-$DS_TAG"

# An unstamped tree is either pristine-from-tarball or the wreckage of a failed
# patch run; either way, throw it away and extract again.
discard_unpatched_tree() {
    local tree="$1"
    if [ -d "$tree" ] && [ ! -f "$tree/.wk-patched" ]; then
        echo "  re-extracting $(basename "$tree") (no .wk-patched stamp)"
        rm -rf "$tree"
    fi
}

fetch_app() {
    discard_unpatched_tree "$DS_SRC"
    [ -d "$DS_SRC" ] && return 0
    local tar_path="$TARBALLS/drumstick-$DS_TAG.tar.gz"
    if [ ! -f "$tar_path" ]; then
        echo "fetching drumstick $DS_TAG..."
        curl -fsSL --retry 3 -o "$tar_path.part" \
            "https://github.com/pedrolcl/drumstick/archive/refs/tags/$DS_TAG.tar.gz"
        mv "$tar_path.part" "$tar_path"
    fi
    echo "extracting drumstick $DS_TAG..."
    tar xzf "$tar_path" -C "$SRCDIR"
}

# --- patches ----------------------------------------------------------------
# patches/drumstick-NNNN-*.patch, applied -p1 in order at the tree's root. See
# patches/README.md for the ledger and the reason each one exists.
#
# Idempotency is a STAMP FILE, not a per-patch `git apply --reverse --check`:
# patch 0004 adds lines inside 0003's context, so after both are applied 0003
# no longer reverse-applies and a reverse-check would try to apply it again and
# die with "patch does not apply". A stamp is unambiguous, and re-extracting an
# unstamped tree means a half-applied tree can never linger.
#
# GIT_CEILING_DIRECTORIES is not paranoia, it is the difference between the
# patches applying and NOT applying, silently, with exit status 0. The
# extracted tree sits INSIDE the wk repository's working tree, so plain
# `git -C "$tree" apply` discovers wk's .git, decides the patch's paths are
# relative to the REPO ROOT, notices that `library/widgets/CMakeLists.txt` is
# outside the current subdirectory, and prints "Skipped patch ..." only under
# --verbose. The build then proceeds against pristine sources and fails a
# minute later somewhere unrelated. Naming a ceiling stops the upward search
# at src/, git apply runs outside any repository, and the paths are resolved
# against the cwd the way the patch intends.
apply_patches() {
    local tree="$1"
    [ -f "$tree/.wk-patched" ] && { echo "  patches: already applied to $(basename "$tree")"; return 0; }
    for p in "$PATCHDIR"/drumstick-*.patch; do
        [ -e "$p" ] || continue
        echo "  patch: $(basename "$p")"
        GIT_CEILING_DIRECTORIES="$SRCDIR" git -C "$tree" apply "$p"
    done
    touch "$tree/.wk-patched"
}

# --- stage a font -----------------------------------------------------------
# First hit wins; repo-local candidates first so a machine-independent font is
# preferred. Nothing is fetched from the network: a build must not depend on a
# font download.
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
    echo "qt-vpiano: no font found. Tried:" >&2
    printf '  %s\n' "${FONT_CANDIDATES[@]}" >&2
    echo "  Qt 6 ships no fonts and a wk node has no host font dir, so a" >&2
    echo "  component built without one renders every string as nothing." >&2
    exit 1
fi
cp -f "$FONT" "$FONTDIR/$(basename "$FONT")"
echo "=== font: $FONT -> fonts/$(basename "$FONT")"
# The .qrc is GENERATED next to the staged font because which font exists
# depends on the machine. Prefix /fonts is where QWkFontDatabase looks last
# (after QT_QPA_FONTDIR and /usr/share/fonts in the node's VFS).
cat > "$FONTDIR/wkfonts.qrc" <<EOF
<!DOCTYPE RCC>
<RCC version="1.0">
    <qresource prefix="/fonts">
        <file>$(basename "$FONT")</file>
    </qresource>
</RCC>
EOF

# --- wit bindings -----------------------------------------------------------
# Regenerated every build into OUR gen/, never into the shared plugins'
# gen/ directories: those are disposable and several plugin builds may be
# running at once.
#
# TWO worlds go into ONE component, which is the thing this port had to
# establish. Each *_component_type.o carries only a `component-type` custom
# section that wasm-component-ld reads to build the import list; they MERGE,
# and the result imports wasi:surface and wk:midi/midi together. Verify with:
#   wasm-tools component wit drumstick-vpiano.wasm | head
echo "=== wit-bindgen (wkgfx world)"
wit-bindgen c --world wkgfx "$GFXCOMPAT/wit" --out-dir "$GEN" >/dev/null
echo "=== wit-bindgen (wkmidi world)"
wit-bindgen c --world wkmidi "$MIDICOMPAT/wit" --out-dir "$GEN" >/dev/null

# Only wkgfx_component_type.o is used from the gfx side: the gfx shim and its
# bindings are already objects INSIDE libqwk.a (llvm-ar t will show you
# wkgfx.c.obj there). The MIDI side is the opposite — nothing else in the
# component has compiled it, so shim/wkmidiio.c and gen/wkmidi.c are both
# handed to the drumstick build as WK_MIDI_SHIM_SRCS.
WK_LINK_OBJS="$GEN/wkgfx_component_type.o;$GEN/wkmidi_component_type.o"

# ...and wk:clipboard, IF this libqwk.a wants it. The QPA grew a clipboard
# bridge (QWkClipboard) that lives inside libqwk.a along with its shim, so an
# app linking that QPA has an undefined
# __component_type_object_force_link_wkclipboard whether or not it ever copies
# anything. Probing the archive rather than assuming keeps this port building
# against a libqwk.a from either side of that change.
if "$NM" --undefined-only "$QTBASE_SYSROOT/lib/libqwk.a" 2>/dev/null \
        | grep -q __component_type_object_force_link_wkclipboard; then
    if [ ! -d "$CLIPCOMPAT/wit" ]; then
        echo "qt-vpiano: libqwk.a needs the wk:clipboard bindings but" >&2
        echo "  $CLIPCOMPAT/wit is missing. Rebuild plugins/qt/build-qpa.sh" >&2
        echo "  or restore plugins/clipboard-compat." >&2
        exit 1
    fi
    echo "=== wit-bindgen (wkclipboard world -- libqwk.a asks for it)"
    wit-bindgen c --world wkclipboard "$CLIPCOMPAT/wit" --out-dir "$GEN" >/dev/null
    WK_LINK_OBJS="$WK_LINK_OBJS;$GEN/wkclipboard_component_type.o"
fi

fetch_app
apply_patches "$DS_SRC"

# --- configure + build ------------------------------------------------------
#
# CMAKE_BUILD_TYPE=MinSizeRel (-Os -DNDEBUG), matching the other Qt ports.
#
# WARNINGS_ARE_ERRORS=OFF: wasi-sdk 34-rc.2's clang reports itself as Clang 23,
# which drumstick 2.11.1 predates.
#
# What each OFF costs, so none of them is a mystery later:
#   BUILD_ALSA      Linux-only sequencer library. Also the only place a
#                   QThread appears anywhere in drumstick.
#   BUILD_FILE      SMF/WRK/RMI file readers. They are the sole reason
#                   drumstick asks for Qt6Core5Compat, and vpiano does not use
#                   them — it has no player.
#   USE_NETWORK     the net-in/net-out backends are QUdpSocket over multicast.
#                   Nothing stops them working in principle (wk's fabric backs
#                   real BSD sockets), but they would need socket notifiers on
#                   a multicast socket, and the wk backends are the point here.
#   USE_FLUIDSYNTH  a fluidsynth-in-process backend. wk's answer is better: a
#                   SEPARATE plugins/fluidsynth node on the other end of a
#                   canvas wire, which is exactly what the harness asserts.
#   USE_SONIVOX / USE_PULSEAUDIO / USE_PIPEWIRE / USE_DBUS   Linux audio and
#                   RealtimeKit; none exist on wasi.
#   BUILD_DOCS      needs Doxygen.
#   BUILD_TESTING   `include(CTest)` defaults it ON, and the tests need
#                   Qt6::Test, which plugins/qt builds with FEATURE_testlib=OFF.
LOG="$LOGDIR/app.log"
if [ ! -f "$BUILD/app/build.ninja" ] || [ -n "${WK_VPIANO_RECONFIGURE:-}" ]; then
    echo "=== configuring drumstick $DS_TAG for wasm32-wasip2 (log: $LOG)"
    env PATH="$BUILD_PATH" cmake -G Ninja -S "$DS_SRC" -B "$BUILD/app" \
        -DCMAKE_TOOLCHAIN_FILE="$QTPLUGIN/wasip2.cmake" \
        -DWASI_SDK_PREFIX="$WASI_SDK" \
        -DCMAKE_FIND_ROOT_PATH="$QTBASE_SYSROOT" \
        -DCMAKE_PREFIX_PATH="$QTBASE_SYSROOT" \
        -DQT_HOST_PATH="$HOST_PREFIX" \
        -DCMAKE_BUILD_TYPE=MinSizeRel \
        -DBUILD_SHARED_LIBS=OFF \
        -DSTATIC_DRUMSTICK=ON \
        -DBUILD_ALSA=OFF \
        -DBUILD_FILE=OFF \
        -DBUILD_RT=ON \
        -DBUILD_WIDGETS=ON \
        -DBUILD_UTILS=ON \
        -DBUILD_DOCS=OFF \
        -DBUILD_TESTING=OFF \
        -DBUILD_FRAMEWORKS=OFF \
        -DUSE_NETWORK=OFF \
        -DUSE_FLUIDSYNTH=OFF \
        -DUSE_SONIVOX=OFF \
        -DUSE_PULSEAUDIO=OFF \
        -DUSE_PIPEWIRE=OFF \
        -DUSE_DBUS=OFF \
        -DWARNINGS_ARE_ERRORS=OFF \
        -DPKG_CONFIG_EXECUTABLE="$PKGCONFIG" \
        -DWK_MIDI_SHIM_SRCS="$SHIMDIR/wkmidiio.c;$GEN/wkmidi.c" \
        -DWK_MIDI_SHIM_INCLUDES="$SHIMDIR;$GEN" \
        -DWK_QPA_LIB="$QTBASE_SYSROOT/lib/libqwk.a" \
        -DWK_COMPONENT_TYPE_OBJS="$WK_LINK_OBJS" \
        -DWK_FONTS_QRC="$FONTDIR/wkfonts.qrc" \
        2>&1 | tee "$LOG"
else
    echo "=== app already configured (WK_VPIANO_RECONFIGURE=1 to redo)"
fi

echo "=== ninja drumstick-vpiano"
env PATH="$BUILD_PATH" cmake --build "$BUILD/app" --parallel "$JOBS" 2>&1 | tee -a "$LOG"

# wasip2.cmake leaves CMAKE_EXECUTABLE_SUFFIX empty on purpose (Qt's
# architecture config test depends on it), so the linked artifact has no
# extension — and wasm-component-ld already made it a COMPONENT at link time.
# No wasip1 adapter, no `wasm-tools component new`.
cp -f "$BUILD/app/bin/drumstick-vpiano" "$HERE/drumstick-vpiano.wasm"
echo
ls -l "$HERE/drumstick-vpiano.wasm"
echo "built plugins/qt-drumstickvpiano2111/drumstick-vpiano.wasm"

# The claim this port exists for, checked rather than assumed: ONE component,
# BOTH worlds.
if command -v wasm-tools >/dev/null 2>&1; then
    echo
    echo "=== imports"
    wasm-tools component wit "$HERE/drumstick-vpiano.wasm" | grep -E '^\s+(import|export)' || true
fi
