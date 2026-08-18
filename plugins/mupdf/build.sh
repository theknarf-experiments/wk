#!/usr/bin/env bash
# Build UNMODIFIED upstream MuPDF (the library, not its X11/GL viewers) into a
# wk PDF reader node: libmupdf.a + libmupdf-third.a cross-compiled to
# wasm32-wasip1 by MuPDF's own Makefile (it vendors ALL its thirdparty deps —
# freetype, harfbuzz, jbig2dec, openjpeg, zlib, lcms2, mujs, gumbo, ... — in
# its source tarball), plus our thin viewer main (viewer_wk.c) drawing pages
# through the shared ../gfx-compat shim. The FluidSynth port's shape: engine
# fetched pinned + a wk platform file, no upstream edits.
#
# Cross knobs (see mupdf's Makerules): an OS= value it doesn't recognize
# (`wk-wasi`) falls through every platform section to the generic path — no
# pkg-config probing, no objcopy, no pthread — and we supply CC/CXX/AR.
# The base-14 fonts stay embedded (generated/%.c via scripts/hexdump.sh, plain
# bash, cross-safe); tofu=yes tofu_cjk=yes drops the Noto/SIL/CJK extras
# (~44 MB of fonts) that would bloat the wasm.
#
# setjmp: fitz's fz_try/fz_catch (and mujs) are setjmp/longjmp all the way
# down, which lowers to wasm exception handling. wasi-sdk emits the legacy EH
# by default but wasmtime only takes exnref, so every compile gets
# `-mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false` (the lua/curl
# recipe), no LTO, and clang runs with a PATH that omits wasm-opt (it can't
# parse exnref and would corrupt the link output's optional post-pass).
#
# Requires wasi-sdk (set WASI_SDK; defaults to the mise-pinned install),
# wasm-tools, wit-bindgen, curl.
set -euo pipefail
cd "$(dirname "$0")"

# Default to the mise-pinned toolchain when present; ~/wasi-sdk may be stale.
MISE_SDK="$HOME/.local/share/mise/installs/github-web-assembly-wasi-sdk/wasi-sdk-34-rc.2"
WASI_SDK="${WASI_SDK:-$([ -d "$MISE_SDK" ] && echo "$MISE_SDK" || echo "$HOME/wasi-sdk")}"
# Same guard as bash/fluidsynth: the other C plugins are built and tested
# against exactly this SDK, and a silent mismatch wastes an afternoon.
EXPECT="wasi-sdk-34-rc.2"
case "$WASI_SDK" in
    *"$EXPECT"*) ;;
    *)
        echo "mupdf: expected $EXPECT (set WASI_SDK), got: $WASI_SDK" >&2
        exit 1
        ;;
esac
CLANG="$WASI_SDK/bin/clang"
CLANGXX="$WASI_SDK/bin/clang++"

# wasi-sdk's clang runs wasm-opt as an optional post-link step, but the
# wasm-opt on PATH can't parse the new exnref EH we emit. Run clang with a
# PATH that omits it so the pass is skipped; other tools run under the normal
# PATH.
CLANG_PATH="$WASI_SDK/bin:/usr/bin:/bin"

# The sjlj/EH flags every object in this build needs (fitz + mujs longjmp
# through them; harfbuzz is C++ compiled alongside and must agree).
SJLJ="-mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false"

# MuPDF, pinned: the "-source" tarball that includes thirdparty/ + generated/.
# AGPL — fetched at build like doom's engine sources, not vendored here.
MUPDF_VER=1.24.9
MUPDF_DIR="mupdf-$MUPDF_VER-source"
if [ ! -d "$MUPDF_DIR" ]; then
    echo "fetching mupdf $MUPDF_VER..."
    curl -fsSL "https://mupdf.com/downloads/archive/mupdf-$MUPDF_VER-source.tar.gz" | tar xz
fi

# Shared gfx shim + its wasi-gfx bindings (regenerated each build).
GFXCOMPAT="$(pwd)/../gfx-compat"
GFXGEN="$GFXCOMPAT/gen"
mkdir -p "$GFXGEN"
wit-bindgen c --world wkgfx "$GFXCOMPAT/wit" --out-dir "$GFXGEN"

# WASIp1→component adapter, pinned to our wasmtime (46); fetched and cached if
# a registry copy isn't present (same dance as doom's build).
WASMTIME_VER=46.0.1
ADAPTER="${WASI_ADAPTER:-$(find "$HOME/.cargo/registry/src" -name 'wasi_snapshot_preview1.command.wasm' 2>/dev/null | head -1)}"
if [ -z "$ADAPTER" ] || [ ! -f "$ADAPTER" ]; then
    ADAPTER="$GFXGEN/wasi_snapshot_preview1.command.wasm"
    if [ ! -f "$ADAPTER" ]; then
        echo "fetching WASI command adapter $WASMTIME_VER..."
        curl -fsSL "https://github.com/bytecodealliance/wasmtime/releases/download/v$WASMTIME_VER/wasi_snapshot_preview1.command.wasm" -o "$ADAPTER"
    fi
fi

# The library, by mupdf's own Makefile ("libs" = libmupdf.a + libmupdf-third.a
# only — mutool/muraster/viewers are never built, so no X11/GL/curl/pthread).
# build=release is -O2 -DNDEBUG; every HAVE_* that could probe the host is
# pinned off.
env PATH="$CLANG_PATH" make -C "$MUPDF_DIR" -j"$(sysctl -n hw.ncpu 2>/dev/null || nproc)" libs \
    OS=wk-wasi build=release verbose=yes shared=no \
    CC="$CLANG" CXX="$CLANGXX" AR="$WASI_SDK/bin/llvm-ar" \
    RANLIB="$WASI_SDK/bin/llvm-ranlib" LD="$WASI_SDK/bin/wasm-ld" \
    HAVE_X11=no HAVE_GLUT=no HAVE_CURL=no HAVE_PTHREAD=no \
    HAVE_OBJCOPY=no HAVE_LIBCRYPTO=no \
    tofu=yes tofu_cjk=yes \
    XCFLAGS="$SJLJ -D_WASI_EMULATED_SIGNAL -D_GNU_SOURCE -DTOFU -DTOFU_CJK -include $(pwd)/compat/wk_compat.h" \
    XCXXFLAGS="$SJLJ -D_WASI_EMULATED_SIGNAL -D_GNU_SOURCE -fno-exceptions"

# tofu/tofu_cjk are "build_suffix" options — they land in the output dir name.
MUPDF_OUT="$MUPDF_DIR/build/release-tofu-tofu_cjk"

# Our viewer main over the gfx shim: C objects compiled with clang, then
# clang++ drives the link (harfbuzz's C++ objects want the c++abi bits even
# with exceptions off).
mkdir -p obj
for src in viewer_wk.c compat/compat.c "$GFXCOMPAT/wkgfx.c" "$GFXGEN/wkgfx.c"; do
    out="obj/$(basename "${src%.c}")"
    [ "$src" = "$GFXGEN/wkgfx.c" ] && out="obj/wkgfx_gen"
    env PATH="$CLANG_PATH" "$CLANG" --target=wasm32-wasip1 -O2 \
        $SJLJ -D_WASI_EMULATED_SIGNAL -Icompat \
        -I"$MUPDF_DIR/include" -I"$GFXCOMPAT" -I"$GFXGEN" \
        -c "$src" -o "$out.o"
done
env PATH="$CLANG_PATH" "$CLANGXX" --target=wasm32-wasip1 -O2 \
    obj/viewer_wk.o obj/compat.o obj/wkgfx.o obj/wkgfx_gen.o "$GFXGEN/wkgfx_component_type.o" \
    "$MUPDF_OUT/libmupdf.a" "$MUPDF_OUT/libmupdf-third.a" \
    -lsetjmp -lwasi-emulated-signal \
    -Wl,-z,stack-size=8388608 \
    -o mupdf-view.core.wasm

wasm-tools component new mupdf-view.core.wasm --adapt "wasi_snapshot_preview1=$ADAPTER" -o mupdf-view.wasm
rm -f mupdf-view.core.wasm
echo "built plugins/mupdf/mupdf-view.wasm"
