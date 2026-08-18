#!/usr/bin/env bash
# Build UNMODIFIED upstream curl into a wasm32-wasip2 component that wk runs in
# a terminal node — the real curl(1), resolving and connecting over wk's
# network fabric (wasi:sockets → smoltcp), not a reimplementation.
#
# wasip2 links a component directly (no wasm-tools adapter), and wasi-libc's
# wasip2 sysroot provides sockets + getaddrinfo, which map to wk's own
# wasi:sockets host impl — so fabric DNS ("netserve") and node-to-node TCP both
# just work.
#
# No curl source is edited. The cross-build needs four things configure can't
# work out on its own, all of them WASI facts:
#   * setjmp/longjmp lowers to the wasm exception proposal — the same
#     -mllvm flags the lua/php/sqlite plugins use (host enables
#     Config::wasm_exceptions);
#   * WASI has no signals/getpid/process clocks — use wasi-libc's emulation;
#   * WASI has no AF_UNIX: wasip2 ships <sys/un.h> but its struct has no
#     sun_path, so tell configure the header isn't there (and skip curl's
#     socketpair(), which it would otherwise use for multi-handle wakeups and
#     which traps at runtime);
#   * getaddrinfo is detected by *running* a probe, which cross-compiling
#     can't do, so configure assumes "no" and falls back to the gethostbyname
#     path that WASI lacks entirely (symptom: "Curl_ipv4_resolve_r failed").
#     Defining HAVE_GETADDRINFO/HAVE_FREEADDRINFO picks curl's getaddrinfo
#     resolver, which is the one wasi-libc implements.
#
# TLS: wolfSSL, cross-compiled by plugins/wolfssl/build.sh into its sysroot
# with the exact same sjlj/emulated flags — so `--with-wolfssl` just links.
# curl does all socket I/O itself (BIO callbacks over the fabric); wolfSSL
# only transforms buffers and reads the CA bundle from the vfs. Trust comes
# from a pinned Mozilla bundle (cacert.pem, fetched below) that images COPY
# to /etc/ssl/cacert.pem — the path baked in via --with-ca-bundle; --cacert
# still overrides per-invocation. Requires wasi-sdk (WASI_SDK, default
# ~/wasi-sdk). Source is fetched (and cached) under curl-<ver>/ on first run.
set -euo pipefail
cd "$(dirname "$0")"

WASI_SDK="${WASI_SDK:-$HOME/wasi-sdk}"
CURL_VER=8.11.1
SRC="curl-$CURL_VER"
WOLFSSL="$PWD/../wolfssl/sysroot"

if [ ! -f "$WOLFSSL/lib/libwolfssl.a" ]; then
    echo "curl: plugins/wolfssl/sysroot missing — build plugins/wolfssl first (./build.sh)" >&2
    exit 1
fi

# The CA trust anchor set: Mozilla's bundle as curl.se extracts it, pinned by
# date (https://curl.se/docs/caextract.html keeps every dated snapshot).
# Ships in images as /etc/ssl/cacert.pem, which matches --with-ca-bundle.
CACERT_PIN=2026-08-13
if [ ! -f cacert.pem ]; then
    echo "fetching Mozilla CA bundle ($CACERT_PIN)..."
    curl -fsSL "https://curl.se/ca/cacert-$CACERT_PIN.pem" -o cacert.pem.part
    mv cacert.pem.part cacert.pem
fi

# wasi-sdk's clang runs wasm-opt as an optional post-link step, but the wasm-opt
# on PATH can't parse the new exnref EH we emit; run the build with a PATH that
# omits it (kept consistent with the lua/php plugins).
BUILD_PATH="$WASI_SDK/bin:/usr/bin:/bin"

if [ ! -d "$SRC" ]; then
    echo "fetching curl $CURL_VER..."
    curl -fsSL "https://curl.se/download/$SRC.tar.gz" -o "$SRC.tar.gz"
    tar xzf "$SRC.tar.gz"
    rm -f "$SRC.tar.gz"
fi

cd "$SRC"

export CC="$WASI_SDK/bin/clang"
export AR="$WASI_SDK/bin/llvm-ar"
export RANLIB="$WASI_SDK/bin/llvm-ranlib"
export CFLAGS="--target=wasm32-wasip2 -O2 \
    -mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false \
    -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_PROCESS_CLOCKS -D_WASI_EMULATED_GETPID \
    -DHAVE_GETADDRINFO=1 -DHAVE_FREEADDRINFO=1"
export LDFLAGS="-lwasi-emulated-signal -lwasi-emulated-process-clocks -lwasi-emulated-getpid"

# configure's answers are only good for the toolchain+options that produced
# them (bash's lesson); reconfigure from scratch when either changes — e.g.
# the --without-ssl → --with-wolfssl move.
CONFIGURE=(./configure --host=wasm32-wasi
    --with-wolfssl="$WOLFSSL" --with-ca-bundle=/etc/ssl/cacert.pem
    --without-libpsl --without-zlib --without-brotli
    --without-zstd --without-libidn2 --without-nghttp2
    --disable-shared --enable-static
    --disable-threaded-resolver --disable-ntlm
    --disable-unix-sockets --disable-socketpair
    ac_cv_header_sys_un_h=no)
# The stamp also covers wolfSSL's generated options.h: a wolfSSL reconfigure
# changes what curl's feature probes find (ALPN, the BIO chain), and only a
# fresh configure re-asks.
STAMP="$("$CC" --version | head -1) ${CONFIGURE[*]} $(cksum "$WOLFSSL/include/wolfssl/options.h" 2>/dev/null | awk '{print $1}')"
if [ -f Makefile ] && [ "$(cat .wk-configured 2>/dev/null)" != "$STAMP" ]; then
    echo "toolchain or configure options changed; reconfiguring curl"
    env PATH="$BUILD_PATH" make distclean >/dev/null 2>&1 || true
    rm -f .wk-configured
fi
if [ ! -f Makefile ]; then
    env PATH="$BUILD_PATH" "${CONFIGURE[@]}"
    printf '%s' "$STAMP" > .wk-configured
fi

env PATH="$BUILD_PATH" make -j"$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)"

cd ..
cp "$SRC/src/curl" curl.wasm
echo "built plugins/curl/curl.wasm (curl $CURL_VER, wasm32-wasip2 component)"
