#!/usr/bin/env bash
# Build the NetSurf 3.11 framebuffer browser as a wk graphics node:
# plugins/netsurf/netsurf.wasm, a wasm32-wasip2 component whose window is a
# wasi-gfx surface (via ../gfx-compat) and whose HTTP rides the fabric (via
# plugins/curl's libcurl.a).
#
# Layers, in build order:
#   1. ./build-deps.sh — the whole dependency chain into ./sysroot (wasip1
#      objects; see PORTING.md for why they link into a wasip2 binary).
#   2. libnsfb 0.2.2 with a NEW `wk` surface backend (surface/wk.c, copied
#      into the upstream tree) — NetSurf's software framebuffer presented
#      through wkgfx_present, input from wkgfx_poll_event.
#   3. netsurf 3.11, TARGET=framebuffer, no JS (duktape off, so the skipped
#      nsgenbind never runs), no TLS (curl has no SSL backend), internal
#      font, png/jpeg/gif/bmp image handlers, curl+data/file/about/resource
#      fetchers. Linked wasip2-direct like plugins/bash (wasm-component-ld
#      emits the component; no adapter).
#
# Patches to upstream (all loud, all idempotent, ledger in PORTING.md):
#   * libnsfb include/libnsfb.h — add NSFB_SURFACE_WK to the surface-type
#     enum (a fixed upstream list; there is no registration-time allocation
#     of type ids). Added right after NSFB_SURFACE_NONE so NetSurf's
#     "lowest type wins" default-surface enumeration picks wk.
#   * libnsfb src/surface/Makefile — compile wk.c (the list of always-built
#     surface handlers is hardcoded).
#   * netsurf content/fetchers/curl.c — see the perl stanza below.
#
# Requires wasi-sdk (WASI_SDK, mise-pinned), wit-bindgen, host pkg-config +
# libpng (netsurf's build-machine convert_image tool links real libpng).
# Full logs in ./logs; idempotent — artifacts are skipped when present.
set -euo pipefail
cd "$(dirname "$0")"

# --- toolchain guard (same as build-deps.sh / plugins/bash) ------------------
MISE_SDK="$HOME/.local/share/mise/installs/github-web-assembly-wasi-sdk/wasi-sdk-34-rc.2"
WASI_SDK="${WASI_SDK:-$([ -d "$MISE_SDK" ] && echo "$MISE_SDK" || echo "$HOME/wasi-sdk")}"
EXPECT="wasi-sdk-34-rc.2"
case "$WASI_SDK" in
    *"$EXPECT"*) ;;
    *)
        echo "netsurf: expected $EXPECT (set WASI_SDK), got: $WASI_SDK" >&2
        exit 1
        ;;
esac

SYSROOT="$PWD/sysroot"
TARBALLS="$PWD/tarballs"
SRCDIR="$PWD/src"
LOGDIR="${LOGDIR:-$PWD/logs}"
GFXCOMPAT="$PWD/../gfx-compat"
GFXGEN="$GFXCOMPAT/gen"
CURLDIR="$PWD/../curl/curl-8.11.1"
mkdir -p "$TARBALLS" "$SRCDIR" "$LOGDIR"

# Host-side libpng flags for netsurf's convert_image build tool — computed
# BEFORE PKG_CONFIG_LIBDIR points every later pkg-config call at the wasm
# sysroot (the tool runs on this machine and needs the real thing).
HOST_LIBPNG_CFLAGS="$(pkg-config --cflags libpng)"
HOST_LIBPNG_LDFLAGS="$(pkg-config --libs libpng)"

# Same flag set as build-deps.sh, per-target-triple.
WASI_EXTRA="-mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false \
    -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_PROCESS_CLOCKS -D_WASI_EMULATED_GETPID"
CC_WASI="$WASI_SDK/bin/clang"
AR_WASI="$WASI_SDK/bin/llvm-ar"
RANLIB_WASI="$WASI_SDK/bin/llvm-ranlib"
BUILD_PATH="$WASI_SDK/bin:$PWD/.toolbin:/usr/bin:/bin"
JOBS="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)"

run_logged() { # run_logged <logname> <cmd...>
    local log="$LOGDIR/$1.log"; shift
    if ! "$@" >>"$log" 2>&1; then
        echo "FAILED — full log: $log" >&2
        return 1
    fi
}

fetch() { # fetch <url> <tarball> <srcdir-it-extracts-to>
    local url="$1" tb="$2" dir="$3"
    [ -d "$SRCDIR/$dir" ] && return 0
    if [ ! -f "$TARBALLS/$tb" ]; then
        echo "fetching $tb..."
        curl -fsSL "$url" -o "$TARBALLS/$tb.part"
        mv "$TARBALLS/$tb.part" "$TARBALLS/$tb"
    fi
    tar xzf "$TARBALLS/$tb" -C "$SRCDIR"
}

# =============================================================================
# 0. the dependency chain (idempotent; no-op once sysroot is populated)
# =============================================================================
./build-deps.sh

if [ ! -f "$CURLDIR/lib/.libs/libcurl.a" ]; then
    echo "netsurf: plugins/curl/curl-8.11.1/lib/.libs/libcurl.a missing —" >&2
    echo "  build plugins/curl first (its build.sh); see PORTING.md" >&2
    exit 1
fi

# =============================================================================
# 1. libnsfb 0.2.2 + the wk surface backend
# =============================================================================
NSFB_VER=0.2.2
NSFB_SRC="$SRCDIR/libnsfb-$NSFB_VER"
fetch "https://download.netsurf-browser.org/libs/releases/libnsfb-$NSFB_VER-src.tar.gz" \
      "libnsfb-$NSFB_VER-src.tar.gz" "libnsfb-$NSFB_VER"

# *** SOURCE PATCH 1 (loud): a surface-type id for the wk backend. libnsfb's
# surface registry keys on a fixed enum in the public header; there is no
# dynamic id. Placed immediately after NSFB_SURFACE_NONE so it is the lowest
# real type — NetSurf's framebuffer frontend defaults to the lowest-numbered
# registered surface, which must be ours, not the always-compiled ram one.
if ! grep -q NSFB_SURFACE_WK "$NSFB_SRC/include/libnsfb.h"; then
    perl -0pi -e 's/(NSFB_SURFACE_NONE = 0, \/\*\*< No surface \*\/\n)/$1    NSFB_SURFACE_WK, \/**< wk wasi-gfx surface *\/\n/' \
        "$NSFB_SRC/include/libnsfb.h"
    grep -q NSFB_SURFACE_WK "$NSFB_SRC/include/libnsfb.h" || {
        echo "netsurf: NSFB_SURFACE_WK patch failed to apply" >&2; exit 1; }
fi

# *** SOURCE PATCH 2 (loud): compile the new backend. The list of
# always-built surface handlers is hardcoded in the surface Makefile.
if ! grep -q 'wk\.c' "$NSFB_SRC/src/surface/Makefile"; then
    perl -pi -e 's/^SURFACE_HANDLER_yes := surface\.c ram\.c$/SURFACE_HANDLER_yes := surface.c ram.c wk.c/' \
        "$NSFB_SRC/src/surface/Makefile"
    grep -q 'wk\.c' "$NSFB_SRC/src/surface/Makefile" || {
        echo "netsurf: surface Makefile patch failed to apply" >&2; exit 1; }
fi

# The new backend itself: a repo file, copied in beside sdl.c/ram.c.
cp surface/wk.c "$NSFB_SRC/src/surface/wk.c"

if [ ! -f "$SYSROOT/lib/libnsfb.a" ] || [ surface/wk.c -nt "$SYSROOT/lib/libnsfb.a" ]; then
    echo "building libnsfb $NSFB_VER (with the wk surface)..."
    rm -f "$LOGDIR/nsfb.log"
    # PKG_CONFIG_LIBDIR pins surface autodetection to our sysroot: no sdl/xcb/
    # vnc/wayland there, so only surface.c+ram.c+wk.c compile. CFLAGS via env,
    # wasip1 like every other sysroot lib (they all meet at the wasip2 link).
    run_logged nsfb env PATH="$BUILD_PATH" \
        PKG_CONFIG_LIBDIR="$SYSROOT/lib/pkgconfig" \
        CC="$CC_WASI" AR="$AR_WASI" RANLIB="$RANLIB_WASI" \
        CFLAGS="--target=wasm32-wasip1 -O2 $WASI_EXTRA -I$GFXCOMPAT" \
        make -C "$NSFB_SRC" -j"$JOBS" install \
            PREFIX="$SYSROOT" HOST=wasm32-wasip1 \
            COMPONENT_TYPE=lib-static VARIANT=release
fi

# libnsfb is the LAST library on netsurf's link line (whole-archive, from
# pkg-config) — so its .pc is where the trailing wasi support libs must ride:
# setjmp lowering runtime (libpng/libjpeg/curl/netsurf all setjmp) and the
# emulated signal/clock/getpid libs. Appending them here beats fighting
# netsurf's LDFLAGS ordering (env LDFLAGS lands BEFORE every pkg-config lib).
if ! grep -q 'lwasi-emulated-mman' "$SYSROOT/lib/pkgconfig/libnsfb.pc"; then
    perl -pi -e 's/^(Libs:(?:(?! -lsetjmp).)*)( -lsetjmp.*)?$/$1 -lsetjmp -lwasi-emulated-signal -lwasi-emulated-process-clocks -lwasi-emulated-getpid -lwasi-emulated-mman/' \
        "$SYSROOT/lib/pkgconfig/libnsfb.pc"
fi

# =============================================================================
# 2. gfx-compat shim objects + bindings (regenerated every build, like doom)
# =============================================================================
mkdir -p "$GFXGEN"
wit-bindgen c --world wkgfx "$GFXCOMPAT/wit" --out-dir "$GFXGEN"
OBJDIR="$PWD/obj"
mkdir -p "$OBJDIR"
for src in "$GFXCOMPAT/wkgfx.c" "$GFXGEN/wkgfx.c"; do
    out="$OBJDIR/$(basename "$(dirname "$src")")-wkgfx.o"
    "$CC_WASI" --target=wasm32-wasip2 -O2 -I"$GFXCOMPAT" -I"$GFXGEN" \
        -c "$src" -o "$out"
done

# =============================================================================
# 3. a libcurl.pc for the existing plugins/curl build (do NOT rebuild curl)
# =============================================================================
cat > "$SYSROOT/lib/pkgconfig/libcurl.pc" <<EOF
# Generated by plugins/netsurf/build.sh: points netsurf's pkg-config probe at
# the wasm32-wasip2 libcurl.a plugins/curl already built (plain HTTP, no TLS).
prefix=$CURLDIR
Name: libcurl
Description: curl 8.11.1 for wasm32-wasip2 (plugins/curl)
Version: 8.11.1
Libs: -L\${prefix}/lib/.libs -lcurl
Cflags: -I\${prefix}/include
EOF

# =============================================================================
# 4. netsurf 3.11, TARGET=framebuffer
# =============================================================================
NS_VER=3.11
NS_SRC="$SRCDIR/netsurf-$NS_VER"
fetch "https://download.netsurf-browser.org/netsurf/releases/source/netsurf-$NS_VER-src.tar.gz" \
      "netsurf-$NS_VER-src.tar.gz" "netsurf-$NS_VER"

# *** SOURCE PATCH 3 (loud): plugins/curl's libcurl.a is built --without-zlib
# (PORTING.md), so it cannot decompress gzip responses — but netsurf asks for
# them (SETOPT(CURLOPT_ENCODING, "gzip") at fetcher init). Passing NULL makes
# curl send no Accept-Encoding at all, so honest servers reply with identity
# encoding instead of gzip bytes netsurf would render as garbage.
if grep -q 'SETOPT(CURLOPT_ENCODING, "gzip");' "$NS_SRC/content/fetchers/curl.c"; then
    perl -pi -e 's/SETOPT\(CURLOPT_ENCODING, "gzip"\);/SETOPT(CURLOPT_ENCODING, NULL); \/* wk: zlib-less curl must not advertise gzip *\//' \
        "$NS_SRC/content/fetchers/curl.c"
fi

# Reconfigure-from-scratch when the toolchain changes (bash's lesson: stale
# object files against a different sysroot fail in silent ways).
TOOLCHAIN="$("$CC_WASI" --version | head -1)"
if [ -d "$NS_SRC/build" ] && [ "$(cat "$NS_SRC/.wk-toolchain" 2>/dev/null)" != "$TOOLCHAIN" ]; then
    echo "toolchain changed; cleaning netsurf build tree"
    rm -rf "$NS_SRC/build" "$NS_SRC/nsfb"
fi
printf '%s' "$TOOLCHAIN" > "$NS_SRC/.wk-toolchain"

echo "building netsurf $NS_VER (framebuffer/wk)..."
rm -f "$LOGDIR/netsurf.log"
# CFLAGS/LDFLAGS via environment: netsurf's Makefiles append to them (a
# command-line CFLAGS would clobber every internal flag). LDFLAGS needs the
# target triple too — the link step runs $(CC) $(LDFLAGS) without CFLAGS —
# plus the wkgfx shim objects; the trailing -l libs ride in libnsfb.pc.
# PREFIX=/usr bakes /usr/share/netsurf into the resource search path (where
# the Dockerfile puts the resources). SHELL=/bin/bash because the LINKDEPS
# recipe uses `echo -n`, which macOS /bin/sh doesn't have — it would write a
# "-n"-prefixed link.d that breaks the NEXT make run with "missing separator".
run_logged netsurf env PATH="$BUILD_PATH" \
    PKG_CONFIG_LIBDIR="$SYSROOT/lib/pkgconfig" \
    CC="$CC_WASI" AR="$AR_WASI" RANLIB="$RANLIB_WASI" \
    CFLAGS="--target=wasm32-wasip2 -O2 $WASI_EXTRA -D_WASI_EMULATED_MMAN -I$SYSROOT/include" \
    LDFLAGS="--target=wasm32-wasip2 $OBJDIR/gfx-compat-wkgfx.o $OBJDIR/gen-wkgfx.o $GFXGEN/wkgfx_component_type.o" \
    make -C "$NS_SRC" -j"$JOBS" \
        SHELL=/bin/bash \
        TARGET=framebuffer PREFIX=/usr \
        NETSURF_USE_DUKTAPE=NO \
        NETSURF_USE_OPENSSL=NO \
        NETSURF_USE_CURL=YES \
        NETSURF_USE_JPEG=YES \
        NETSURF_USE_JPEGXL=NO \
        NETSURF_USE_WEBP=NO \
        NETSURF_USE_NSSVG=NO \
        NETSURF_USE_ROSPRITE=NO \
        NETSURF_USE_NSPSL=NO \
        NETSURF_FB_FONTLIB=internal \
        BUILD_LIBPNG_CFLAGS="$HOST_LIBPNG_CFLAGS" \
        BUILD_LIBPNG_LDFLAGS="$HOST_LIBPNG_LDFLAGS"

# wasip2-direct: the linked ELF^Wcomponent IS the deliverable (no adapter).
cp "$NS_SRC/nsfb" netsurf.wasm

# =============================================================================
# 5. stage the runtime resources for the Dockerfile
# =============================================================================
rm -rf res && mkdir -p res
for f in adblock.css credits.html default.css internal.css licence.html \
         netsurf.png quirks.css welcome.html favicon.png; do
    cp "$NS_SRC/frontends/framebuffer/res/$f" res/
done
# The split (framebuffer-filtered, English) messages the build generated.
cp "$NS_SRC/frontends/framebuffer/res/en/Messages" res/Messages

echo "built plugins/netsurf/netsurf.wasm (+ res/)"
echo "package it with: wk images build plugins/netsurf/Dockerfile --tag netsurf"
