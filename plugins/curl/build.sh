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
# TLS is out: no wasm-capable TLS backend is wired up here, so this is a
# plain-http build (`--without-ssl`). Requires wasi-sdk (WASI_SDK, default
# ~/wasi-sdk). Source is fetched (and cached) under curl-<ver>/ on first run.
set -euo pipefail
cd "$(dirname "$0")"

WASI_SDK="${WASI_SDK:-$HOME/wasi-sdk}"
CURL_VER=8.11.1
SRC="curl-$CURL_VER"

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

if [ ! -f Makefile ]; then
    ./configure --host=wasm32-wasi \
        --without-ssl --without-libpsl --without-zlib --without-brotli \
        --without-zstd --without-libidn2 --without-nghttp2 \
        --disable-shared --enable-static \
        --disable-threaded-resolver --disable-ntlm \
        --disable-unix-sockets --disable-socketpair \
        ac_cv_header_sys_un_h=no
fi

env PATH="$BUILD_PATH" make -j"$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)"

cd ..
cp "$SRC/src/curl" curl.wasm
echo "built plugins/curl/curl.wasm (curl $CURL_VER, wasm32-wasip2 component)"
