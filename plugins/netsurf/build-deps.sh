#!/usr/bin/env bash
# Cross-compile the NetSurf 3.11-era dependency-library chain to WebAssembly
# (wasm32-wasip1, wasi-sdk) and install static libs + headers into ./sysroot/.
#
# This is prep for porting the NetSurf framebuffer browser as a wk node. It
# builds ONLY the upstream libraries (unmodified where possible) — not netsurf
# itself and not libnsfb.
#
# Conventions copied from plugins/bash/build.sh and plugins/curl/build.sh:
#   * mise-pinned wasi-sdk-34-rc.2, guarded below (~/wasi-sdk is a stale side
#     install);
#   * the setjmp/EH lowering flags (-mllvm -wasm-enable-sjlj
#     -mllvm -wasm-use-legacy-eh=false) on EVERY object so the whole sysroot is
#     link-compatible with plugins/curl's libcurl.a later (libpng/libjpeg
#     actually use setjmp; for the rest the flags are inert but keep the EH
#     representation uniform);
#   * a build PATH that contains no wasm-opt — the one on a homebrew PATH can't
#     parse the exnref EH we emit (matters for configure's link probes).
#
# NetSurf's own libs use the netsurf `buildsystem` (make, not autotools):
#   make install PREFIX=<sysroot> COMPONENT_TYPE=lib-static HOST=wasm32-wasip1
# with CC/AR/CFLAGS from the environment. HOST is passed explicitly so
# Makefile.tools doesn't sniff it from `$CC -dumpmachine` (which would answer
# arm64-apple-darwin). Tests never run: buildsystem only enables them for the
# `test`/`coverage` make goals, which we never invoke.
#
# Idempotent: each step is skipped when its installed artifact already exists
# in sysroot/. Full logs land in $LOGDIR (default ./logs), one file per lib —
# on failure the log path is printed.
set -euo pipefail
cd "$(dirname "$0")"

# --- toolchain guard (same as plugins/bash/build.sh) -------------------------
MISE_SDK="$HOME/.local/share/mise/installs/github-web-assembly-wasi-sdk/wasi-sdk-34-rc.2"
WASI_SDK="${WASI_SDK:-$([ -d "$MISE_SDK" ] && echo "$MISE_SDK" || echo "$HOME/wasi-sdk")}"
EXPECT="wasi-sdk-34-rc.2"
case "$WASI_SDK" in
    *"$EXPECT"*) ;;
    *)
        echo "netsurf-deps: expected $EXPECT (set WASI_SDK), got: $WASI_SDK" >&2
        exit 1
        ;;
esac

# --- layout ------------------------------------------------------------------
SYSROOT="$PWD/sysroot"
TARBALLS="$PWD/tarballs"
SRCDIR="$PWD/src"
LOGDIR="${LOGDIR:-$PWD/logs}"
TOOLBIN="$PWD/.toolbin"
mkdir -p "$SYSROOT" "$TARBALLS" "$SRCDIR" "$LOGDIR" "$TOOLBIN"

# Host tools the builds need that live outside /usr/bin (cmake, pkg-config are
# homebrew here). Symlinked into .toolbin so BUILD_PATH can stay free of the
# rest of homebrew — notably any wasm-opt.
for t in cmake pkg-config; do
    p="$(command -v "$t" || true)"
    [ -n "$p" ] && ln -sf "$p" "$TOOLBIN/$t"
done
BUILD_PATH="$WASI_SDK/bin:$TOOLBIN:/usr/bin:/bin"

# --- target flags ------------------------------------------------------------
TARGET=wasm32-wasip1
# The sjlj/EH and WASI-emulation flags match plugins/curl exactly (see header).
WASI_EXTRA="-mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false \
    -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_PROCESS_CLOCKS -D_WASI_EMULATED_GETPID"
WASI_CFLAGS="--target=$TARGET -O2 $WASI_EXTRA"
CC_WASI="$WASI_SDK/bin/clang"
AR_WASI="$WASI_SDK/bin/llvm-ar"
RANLIB_WASI="$WASI_SDK/bin/llvm-ranlib"

NS_MIRROR="https://download.netsurf-browser.org/libs/releases"

# --- versions (NetSurf 3.11-era: the latest release of each) -----------------
BUILDSYSTEM_VER=1.10
WAPCAPLET_VER=0.4.3
PARSERUTILS_VER=0.2.5
HUBBUB_VER=0.3.8
CSS_VER=0.9.2
DOM_VER=0.4.2
NSBMP_VER=0.1.7
NSGIF_VER=1.0.0
NSUTILS_VER=0.1.1
NSLOG_VER=0.1.3
UTF8PROC_VER=2.4.0-1
NSGENBIND_VER=0.9
ZLIB_VER=1.3.1
LIBPNG_VER=1.6.50
JPEG_TURBO_VER=3.1.1
EXPAT_VER=2.7.1

# --- helpers -----------------------------------------------------------------
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

run_logged() { # run_logged <logname> <cmd...>
    local log="$LOGDIR/$1.log"; shift
    if ! "$@" >>"$log" 2>&1; then
        echo "FAILED — full log: $log" >&2
        return 1
    fi
}

JOBS="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)"

# Build+install one netsurf-buildsystem library. Extra make vars may follow
# the two fixed args. CFLAGS additions go in NS_EXTRA_CFLAGS (env), because
# CFLAGS must arrive via the environment: the lib Makefiles do
# `CFLAGS := <their flags> $(CFLAGS)`, which appends an environment CFLAGS but
# would be clobbered BY a command-line CFLAGS.
ns_lib() { # ns_lib <srcdir> <component> [extra make vars...]
    local dir="$1" comp="$2"; shift 2
    if [ -f "$SYSROOT/lib/lib$comp.a" ]; then
        echo "lib$comp: already in sysroot, skipping"
        return 0
    fi
    echo "building lib$comp ($dir)..."
    run_logged "$comp" env PATH="$BUILD_PATH" \
        CC="$CC_WASI" AR="$AR_WASI" RANLIB="$RANLIB_WASI" \
        CFLAGS="$WASI_CFLAGS ${NS_EXTRA_CFLAGS:-}" \
        make -C "$SRCDIR/$dir" -j"$JOBS" install \
            PREFIX="$SYSROOT" HOST="$TARGET" \
            COMPONENT_TYPE=lib-static VARIANT=release "$@"
}

# =============================================================================
# 1. buildsystem — netsurf's shared make infrastructure
# =============================================================================
fetch "$NS_MIRROR/buildsystem-$BUILDSYSTEM_VER.tar.gz" \
      "buildsystem-$BUILDSYSTEM_VER.tar.gz" "buildsystem-$BUILDSYSTEM_VER"
if [ ! -f "$SYSROOT/share/netsurf-buildsystem/makefiles/Makefile.tools" ]; then
    echo "installing netsurf buildsystem..."
    run_logged buildsystem \
        make -C "$SRCDIR/buildsystem-$BUILDSYSTEM_VER" install PREFIX="$SYSROOT"
fi

# =============================================================================
# 2-5. the core parse chain: wapcaplet -> parserutils -> hubbub -> css
# =============================================================================
fetch "$NS_MIRROR/libwapcaplet-$WAPCAPLET_VER-src.tar.gz" \
      "libwapcaplet-$WAPCAPLET_VER-src.tar.gz" "libwapcaplet-$WAPCAPLET_VER"
ns_lib "libwapcaplet-$WAPCAPLET_VER" wapcaplet

# parserutils: WITHOUT_ICONV_FILTER selects its builtin utf8/utf16/8859/ext8
# codecs (WASI has no iconv). This is an upstream-supported knob in
# src/input/filter.c, not a patch.
fetch "$NS_MIRROR/libparserutils-$PARSERUTILS_VER-src.tar.gz" \
      "libparserutils-$PARSERUTILS_VER-src.tar.gz" "libparserutils-$PARSERUTILS_VER"
NS_EXTRA_CFLAGS="-DWITHOUT_ICONV_FILTER" \
    ns_lib "libparserutils-$PARSERUTILS_VER" parserutils

fetch "$NS_MIRROR/libhubbub-$HUBBUB_VER-src.tar.gz" \
      "libhubbub-$HUBBUB_VER-src.tar.gz" "libhubbub-$HUBBUB_VER"
ns_lib "libhubbub-$HUBBUB_VER" hubbub

fetch "$NS_MIRROR/libcss-$CSS_VER-src.tar.gz" \
      "libcss-$CSS_VER-src.tar.gz" "libcss-$CSS_VER"
ns_lib "libcss-$CSS_VER" css

# =============================================================================
# 6. libdom (+ expat for the XML-document binding)
# =============================================================================
# expat first: it cross-compiles cleanly with configure --host, and netsurf's
# own toolchains ship libdom with the expat binding for XML documents. libdom's
# core + hubbub binding do build without it (verified), so if expat ever
# breaks, dropping WITH_EXPAT_BINDING below keeps the HTML path alive.
fetch "https://github.com/libexpat/libexpat/releases/download/R_$(echo "$EXPAT_VER" | tr . _)/expat-$EXPAT_VER.tar.gz" \
      "expat-$EXPAT_VER.tar.gz" "expat-$EXPAT_VER"
if [ ! -f "$SYSROOT/lib/libexpat.a" ]; then
    echo "building expat..."
    (
        cd "$SRCDIR/expat-$EXPAT_VER"
        export PATH="$BUILD_PATH"
        export CC="$CC_WASI" AR="$AR_WASI" RANLIB="$RANLIB_WASI" CFLAGS="$WASI_CFLAGS"
        [ -f Makefile ] || run_logged expat ./configure --host="$TARGET" \
            --prefix="$SYSROOT" --disable-shared --enable-static \
            --without-docbook --without-examples --without-tests
        run_logged expat make -j"$JOBS" install
    )
fi

fetch "$NS_MIRROR/libdom-$DOM_VER-src.tar.gz" \
      "libdom-$DOM_VER-src.tar.gz" "libdom-$DOM_VER"
# The expat binding's REQUIRED_LIBS wants -lexpat at pkg-config time; our
# sysroot include/lib paths ride in via CFLAGS.
NS_EXTRA_CFLAGS="-I$SYSROOT/include" \
    ns_lib "libdom-$DOM_VER" dom \
        WITH_HUBBUB_BINDING=yes WITH_EXPAT_BINDING=yes

# =============================================================================
# 7-11. images + utility libs: nsbmp, nsgif, nsutils, nslog, utf8proc
# =============================================================================
fetch "$NS_MIRROR/libnsbmp-$NSBMP_VER-src.tar.gz" \
      "libnsbmp-$NSBMP_VER-src.tar.gz" "libnsbmp-$NSBMP_VER"
ns_lib "libnsbmp-$NSBMP_VER" nsbmp

fetch "$NS_MIRROR/libnsgif-$NSGIF_VER-src.tar.gz" \
      "libnsgif-$NSGIF_VER-src.tar.gz" "libnsgif-$NSGIF_VER"
ns_lib "libnsgif-$NSGIF_VER" nsgif

fetch "$NS_MIRROR/libnsutils-$NSUTILS_VER-src.tar.gz" \
      "libnsutils-$NSUTILS_VER-src.tar.gz" "libnsutils-$NSUTILS_VER"
ns_lib "libnsutils-$NSUTILS_VER" nsutils

# nslog's filter grammar is generated at build time with host flex/bison
# (outputs are plain C, then cross-compiled).
fetch "$NS_MIRROR/libnslog-$NSLOG_VER-src.tar.gz" \
      "libnslog-$NSLOG_VER-src.tar.gz" "libnslog-$NSLOG_VER"
# *** SOURCE PATCH (loud, per repo patch-minimalism rule) ***
# Stock macOS only has bison 2.3, and libnslog's grammar uses one bison-2.4+
# construct: a per-type `%destructor { nslog_filter_unref($$); } <filter>`.
# Strip it — it only frees semantic values during parse-ERROR recovery of
# filter strings, so its absence leaks a few bytes on malformed filters and
# changes nothing else. (libnslog's Makefile already picks the right 2.3
# prefixing switch; the missing-prototype half of the 2.3 story is handled by
# compat/nslog-bison23.h below, force-included via -include.)
perl -0pi -e 's/%destructor \{\n\tnslog_filter_unref\(\$\$\);\n\} <filter>\n\n//' \
    "$SRCDIR/libnslog-$NSLOG_VER/src/filter-parser.y"
NS_EXTRA_CFLAGS="-include $PWD/compat/nslog-bison23.h" \
    ns_lib "libnslog-$NSLOG_VER" nslog

fetch "$NS_MIRROR/libutf8proc-$UTF8PROC_VER-src.tar.gz" \
      "libutf8proc-$UTF8PROC_VER-src.tar.gz" "libutf8proc-$UTF8PROC_VER"
ns_lib "libutf8proc-$UTF8PROC_VER" utf8proc

# =============================================================================
# 12. zlib + libpng
# =============================================================================
fetch "https://zlib.net/fossils/zlib-$ZLIB_VER.tar.gz" \
      "zlib-$ZLIB_VER.tar.gz" "zlib-$ZLIB_VER"
if [ ! -f "$SYSROOT/lib/libz.a" ]; then
    echo "building zlib..."
    (
        cd "$SRCDIR/zlib-$ZLIB_VER"
        export PATH="$BUILD_PATH"
        export CC="$CC_WASI" AR="$AR_WASI" RANLIB="$RANLIB_WASI" CFLAGS="$WASI_CFLAGS"
        [ -f configure.log ] || run_logged zlib ./configure --prefix="$SYSROOT" --static
        # configure hardcodes Apple `libtool -o` for archiving when uname says
        # Darwin; that can't archive wasm objects. llvm-ar can.
        run_logged zlib make -j"$JOBS" install AR="$AR_WASI" ARFLAGS=rc RANLIB="$RANLIB_WASI"
    )
fi

fetch "https://github.com/pnggroup/libpng/archive/refs/tags/v$LIBPNG_VER.tar.gz" \
      "libpng-$LIBPNG_VER.tar.gz" "libpng-$LIBPNG_VER"
if [ ! -f "$SYSROOT/lib/libpng16.a" ] || [ ! -f "$SYSROOT/lib/libpng.a" ]; then
    echo "building libpng..."
    (
        cd "$SRCDIR/libpng-$LIBPNG_VER"
        export PATH="$BUILD_PATH"
        run_logged libpng cmake -S . -B build-wasi \
            -DCMAKE_TOOLCHAIN_FILE="$WASI_SDK/share/cmake/wasi-sdk-p1.cmake" \
            -DCMAKE_BUILD_TYPE=Release \
            -DCMAKE_C_FLAGS="$WASI_EXTRA" \
            -DCMAKE_C_FLAGS_RELEASE="-O2 -DNDEBUG" \
            -DCMAKE_INSTALL_PREFIX="$SYSROOT" \
            -DPNG_SHARED=OFF -DPNG_STATIC=ON -DPNG_TESTS=OFF -DPNG_TOOLS=OFF \
            -DPNG_FRAMEWORK=OFF -DPNG_HARDWARE_OPTIMIZATIONS=OFF \
            -DZLIB_LIBRARY="$SYSROOT/lib/libz.a" \
            -DZLIB_INCLUDE_DIR="$SYSROOT/include"
        run_logged libpng cmake --build build-wasi -j "$JOBS" --target install
        # The GitHub-archive cmake build installs the archive as
        # liblibpng16_static.a plus a libpng.a symlink; libpng16.pc says
        # -lpng16, so give the linker that name too.
        ln -sf liblibpng16_static.a "$SYSROOT/lib/libpng16.a"
    )
fi

# =============================================================================
# 13. libjpeg-turbo — SIMD off (no wasm SIMD asm here; generic C)
# =============================================================================
fetch "https://github.com/libjpeg-turbo/libjpeg-turbo/releases/download/$JPEG_TURBO_VER/libjpeg-turbo-$JPEG_TURBO_VER.tar.gz" \
      "libjpeg-turbo-$JPEG_TURBO_VER.tar.gz" "libjpeg-turbo-$JPEG_TURBO_VER"
if [ ! -f "$SYSROOT/lib/libjpeg.a" ]; then
    echo "building libjpeg-turbo..."
    (
        cd "$SRCDIR/libjpeg-turbo-$JPEG_TURBO_VER"
        export PATH="$BUILD_PATH"
        run_logged libjpeg-turbo cmake -S . -B build-wasi \
            -DCMAKE_TOOLCHAIN_FILE="$WASI_SDK/share/cmake/wasi-sdk-p1.cmake" \
            -DCMAKE_BUILD_TYPE=Release \
            -DCMAKE_C_FLAGS="$WASI_EXTRA" \
            -DCMAKE_C_FLAGS_RELEASE="-O2 -DNDEBUG" \
            -DCMAKE_INSTALL_PREFIX="$SYSROOT" \
            -DWITH_SIMD=0 -DENABLE_SHARED=0 -DENABLE_STATIC=1 \
            -DWITH_TURBOJPEG=0 \
            -DCMAKE_EXE_LINKER_FLAGS="-lsetjmp"
        run_logged libjpeg-turbo cmake --build build-wasi -j "$JOBS" --target install
    )
fi

# =============================================================================
# 14. nsgenbind — HOST-side code generator (native build, NO wasi-sdk).
#     Runs on this machine at netsurf build time; needs host flex/bison.
# =============================================================================
fetch "$NS_MIRROR/nsgenbind-$NSGENBIND_VER-src.tar.gz" \
      "nsgenbind-$NSGENBIND_VER-src.tar.gz" "nsgenbind-$NSGENBIND_VER"
if [ ! -f "$SYSROOT/host-tools/bin/nsgenbind" ]; then
    # nsgenbind's grammars use %code/%define — bison >= 2.4 territory. Stock
    # macOS ships bison 2.3 (so does CommandLineTools), which stops at the
    # first %code. Homebrew keg `bison` would do; put it on PATH or set BISON.
    BISON_BIN="${BISON:-bison}"
    bison_ok=no
    if command -v flex >/dev/null && command -v "$BISON_BIN" >/dev/null; then
        bison_vsn="$("$BISON_BIN" --version | sed -n '1s/.* //p')"
        case "$bison_vsn" in
            1.*|2.0*|2.1*|2.2*|2.3*) ;;
            *) bison_ok=yes ;;
        esac
    fi
    if [ "$bison_ok" = yes ]; then
        echo "building nsgenbind (native host tool)..."
        run_logged nsgenbind env PATH="/usr/bin:/bin" \
            make -C "$SRCDIR/nsgenbind-$NSGENBIND_VER" -j"$JOBS" install \
                PREFIX="$SYSROOT/host-tools" \
                NSSHARED="$SYSROOT/share/netsurf-buildsystem" \
                BISON="$BISON_BIN" VARIANT=release
    else
        echo "nsgenbind: SKIPPED — needs host bison >= 2.4 (found: ${bison_vsn:-none});"
        echo "  only required when building netsurf WITH duktape/JS bindings."
        echo "  Remedy: brew install bison, then BISON=/opt/homebrew/opt/bison/bin/bison $0"
    fi
fi

# =============================================================================
# summary
# =============================================================================
echo
echo "sysroot: $SYSROOT"
ls "$SYSROOT/lib" 2>/dev/null | grep '\.a$' | sed 's/^/  lib: /'
[ -f "$SYSROOT/host-tools/bin/nsgenbind" ] && echo "  host-tool: host-tools/bin/nsgenbind"
echo "done."
