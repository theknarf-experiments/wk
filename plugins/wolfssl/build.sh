#!/usr/bin/env bash
# Cross-compile UNMODIFIED upstream wolfSSL into a static wasm32-wasip2
# library — the TLS backend for plugins/curl (and through libcurl, netsurf and
# anything else that links it). wolfSSL only transforms buffers here: curl
# does all the socket I/O through its own BIO callbacks over wk's fabric, so
# nothing in this library ever opens a connection itself.
#
# The build knowledge, each item a failure or a WASI fact first:
#   * --enable-curl — wolfSSL's own preset for what curl's wolfSSL backend
#     needs (opensslextra, SNI, session tickets, CRL/OCSP, alt cert chains,
#     DES3/MD4 for the odd legacy path). One flag instead of chasing curl's
#     docs option by option — with two riders the preset gets wrong:
#     - --enable-alpn explicitly: the preset sets ENABLED_ALPN=yes *after*
#       configure.ac's "if ALPN then -DHAVE_ALPN" has already run (upstream
#       ordering bug), so without the explicit flag the summary says ALPN yes
#       but libwolfssl.a has no wolfSSL_UseALPN and curl's probe fails.
#     - --enable-opensslall: wolfSSL_BIO_set_shutdown lives behind
#       OPENSSL_ALL, and curl only takes its BIO-chain path (curl does the
#       socket I/O, wolfSSL transforms buffers) when that symbol links —
#       otherwise it falls back to wolfSSL_set_fd and wolfSSL owns the
#       socket. We want the buffers-only shape.
#   * entropy — WASI has no /dev/urandom and wasi-libc has no getrandom()
#     symbol (its sys/random.h says so in a comment), but it DOES have
#     getentropy() over wasi:random. random.c is pointed at a tiny chunking
#     wrapper (wkrand.c) via -DCUSTOM_RAND_GENERATE_SEED, force-included
#     header for the prototype, and the object is folded into libwolfssl.a
#     after install (static-only, so ar-injection is safe and keeps the
#     upstream tree unedited).
#   * --enable-singlethreaded — no pthreads on WASI.
#   * --disable-asm — no x86/ARM asm on wasm32; pure C wolfCrypt.
#   * --disable-sys-ca-certs — there is no system trust store in a wk node;
#     curl loads /etc/ssl/cacert.pem from the node's vfs instead (filesystem
#     support stays ON for exactly that).
#   * examples/benchmarks/crypttests off — they want sockets+argv harnesses.
#   * CFLAGS mirror plugins/curl exactly (same sjlj/EH lowering, same
#     emulated-signal defines) so every object is link-compatible with
#     libcurl.a and the final consumer link.
#   * --host=wasm32-wasi (not wasip2): same as curl — the bundled config.sub
#     predates wasip2 triples; the real target rides in CFLAGS.
#   * ac_cv_header_sys_un_h=no — curl's build.sh discovered this one: wasip2's
#     libc ships <sys/un.h> but its struct sockaddr_un has no sun_path, which
#     breaks opensslextra's RAND_egd() (an AF_UNIX path we could never use
#     anyway). Deny the header and the whole path compiles out.
#
# Requires wasi-sdk (WASI_SDK, mise-pinned like plugins/netsurf) and GNU
# autotools (the GitHub archive ships no generated configure; autogen.sh
# runs once after extraction). Source is fetched (and cached) on first run;
# static lib + headers install into ./sysroot (gitignored).
set -euo pipefail
cd "$(dirname "$0")"

# --- toolchain guard (same as plugins/netsurf / plugins/bash) ----------------
MISE_SDK="$HOME/.local/share/mise/installs/github-web-assembly-wasi-sdk/wasi-sdk-34-rc.2"
WASI_SDK="${WASI_SDK:-$([ -d "$MISE_SDK" ] && echo "$MISE_SDK" || echo "$HOME/wasi-sdk")}"
EXPECT="wasi-sdk-34-rc.2"
case "$WASI_SDK" in
    *"$EXPECT"*) ;;
    *)
        echo "wolfssl: expected $EXPECT (set WASI_SDK), got: $WASI_SDK" >&2
        exit 1
        ;;
esac

WOLFSSL_VER=5.7.6
SRC="wolfssl-$WOLFSSL_VER-stable"
SYSROOT="$PWD/sysroot"

# autogen.sh runs autoreconf, which is not in mise's registry — check up front
# rather than let ./autogen.sh die on "autoreconf: not found".
missing=""
for tool in autoconf automake libtool make curl; do
    command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool"
done
if [ -n "$missing" ]; then
    echo "wolfssl/build.sh: missing host tools:$missing" >&2
    echo "  (the GitHub archive ships no generated configure)" >&2
    echo "  brew:          brew bundle --file=plugins/wolfssl/Brewfile" >&2
    echo "  Debian/Ubuntu: apt install autoconf automake libtool build-essential" >&2
    exit 1
fi

# No wasm-opt on the build PATH (curl's trick: the one on PATH can't parse the
# exnref EH we emit and wasi-sdk's clang would run it as a post-link step).
# Everything else on PATH has to survive — autoreconf shells out to autom4te,
# autoheader, m4 and libtoolize, so a hardcoded list can't stand in for
# wherever the autotools were installed.
BUILD_PATH="$WASI_SDK/bin:$PATH"
if WASM_OPT="$(command -v wasm-opt 2>/dev/null)"; then
    WASM_OPT_DIR="$(cd "$(dirname "$WASM_OPT")" && pwd)"
    BUILD_PATH="$(printf '%s' "$BUILD_PATH" | tr ':' '\n' \
        | while IFS= read -r p; do
              [ "$(cd "$p" 2>/dev/null && pwd)" = "$WASM_OPT_DIR" ] || printf '%s:' "$p"
          done)"
    BUILD_PATH="${BUILD_PATH%:}"
fi

if [ ! -d "$SRC" ]; then
    echo "fetching wolfSSL $WOLFSSL_VER..."
    curl -fsSL "https://github.com/wolfSSL/wolfssl/archive/refs/tags/v$WOLFSSL_VER-stable.tar.gz" \
        -o "$SRC.tar.gz"
    tar xzf "$SRC.tar.gz"
    rm -f "$SRC.tar.gz"
fi

cd "$SRC"

# The git archive has no ./configure; generate it once. macOS installs GNU
# libtool's scripts under a g prefix (glibtoolize) — Linux has the unprefixed
# name, and forcing LIBTOOLIZE=glibtoolize there just makes autogen.sh fail.
if [ ! -f configure ]; then
    echo "autoreconfing wolfSSL (git archive ships no configure)..."
    LIBTOOLIZE="$(command -v glibtoolize || command -v libtoolize)"
    env PATH="$BUILD_PATH" LIBTOOLIZE="$LIBTOOLIZE" ./autogen.sh
fi

export CC="$WASI_SDK/bin/clang"
export AR="$WASI_SDK/bin/llvm-ar"
export RANLIB="$WASI_SDK/bin/llvm-ranlib"
export CFLAGS="--target=wasm32-wasip2 -O2 \
    -mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false \
    -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_PROCESS_CLOCKS -D_WASI_EMULATED_GETPID \
    -DCUSTOM_RAND_GENERATE_SEED=wk_getentropy_seed -include $PWD/../wkrand.h"
export LDFLAGS="-lwasi-emulated-signal -lwasi-emulated-process-clocks -lwasi-emulated-getpid"

# configure's answers are only good for the toolchain+options that produced
# them; reconfigure from scratch when either changes (bash's lesson).
CONFIGURE=(./configure --host=wasm32-wasi
    --prefix="$SYSROOT"
    --disable-shared --enable-static
    --enable-singlethreaded
    --disable-asm
    --enable-curl --enable-alpn --enable-opensslall
    --disable-sys-ca-certs
    --disable-examples --disable-crypttests --disable-benchmark
    ac_cv_header_sys_un_h=no)
STAMP="$("$CC" --version | head -1) ${CONFIGURE[*]} $CFLAGS"
if [ -f Makefile ] && [ "$(cat .wk-configured 2>/dev/null)" != "$STAMP" ]; then
    echo "toolchain or configure options changed; reconfiguring wolfSSL"
    env PATH="$BUILD_PATH" make distclean >/dev/null 2>&1 || true
    rm -f .wk-configured
fi
if [ ! -f Makefile ]; then
    env PATH="$BUILD_PATH" "${CONFIGURE[@]}"
    printf '%s' "$STAMP" > .wk-configured
fi

env PATH="$BUILD_PATH" make -j"$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)"
env PATH="$BUILD_PATH" make install

# The entropy shim: compiled with the same flags, folded into the installed
# archive so `-lwolfssl` alone resolves wk_getentropy_seed everywhere.
cd ..
"$CC" --target=wasm32-wasip2 -O2 -c wkrand.c -o wkrand.o
"$AR" r "$SYSROOT/lib/libwolfssl.a" wkrand.o
"$RANLIB" "$SYSROOT/lib/libwolfssl.a"

echo "built plugins/wolfssl/sysroot (wolfSSL $WOLFSSL_VER, static, wasm32-wasip2)"
