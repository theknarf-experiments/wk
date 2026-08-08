#!/usr/bin/env bash
# Build the UNMODIFIED upstream CPython 3.14 interpreter from source, targeting
# wasm32-wasip2 so its socket module lights up over wk's network fabric — a real
# production runtime that can spin up a webserver, not just run scripts.
#
# The one non-obvious bit: CPython's own Tools/wasm/wasi helper defaults to
# wasm32-wasip1 (no sockets) and never passes an explicit --target to clang, so
# it would emit wasip1 even with --host-triple wasm32-wasip2. We fix that by
# injecting `--target=wasm32-wasip2` through configure's CFLAGS/LDFLAGS — only on
# the cross (host) build, never the native build-python. With wasi-sdk 22+ the
# wasm32-wasip2 sysroot provides <sys/socket.h> et al, so configure enables the
# _socket module, and the wasip2 link step (wasm-component-ld) emits a component
# directly — no wasm-tools adapter step (unlike the wasip1 lua/sqlite plugins).
#
# Requires wasi-sdk (WASI_SDK, default ~/wasi-sdk) with a wasm32-wasip2 sysroot,
# a host python3 of the SAME 3.14.x line (used to drive the cross build), and
# wasmtime on PATH (CPython's wasi helper runs the freshly-built python during
# `make install` for ensurepip, and checks for it up front). The CPython source
# is cloned (and cached) under cpython/ on first run; the build is incremental.
set -euo pipefail
cd "$(dirname "$0")"

PY_TAG="${PY_TAG:-v3.14.6}"
PY_XY=3.14
WASI_SDK="${WASI_SDK:-$HOME/wasi-sdk}"
export WASI_SDK_PATH="$WASI_SDK"

# wasmtime is required by CPython's wasi helper; use a local install if present.
command -v wasmtime >/dev/null 2>&1 || export PATH="$HOME/.wasmtime/bin:$PATH"
command -v wasmtime >/dev/null 2>&1 || {
    echo "wasmtime not found; install from https://wasmtime.dev/install.sh" >&2
    exit 1
}

if [ ! -d cpython ]; then
    echo "fetching CPython $PY_TAG..."
    git clone --depth 1 --branch "$PY_TAG" https://github.com/python/cpython.git cpython
fi
cd cpython

# 1. Native build-python (drives the cross compile). No wasip2 flags here.
python3 Tools/wasm/wasi configure-build-python --quiet
python3 Tools/wasm/wasi make-build-python --quiet

# 2. Cross-compile for wasm32-wasip2, forcing the target into the compiler so
#    the wasip2 sysroot (with sockets) is used and a component is produced.
python3 Tools/wasm/wasi configure-host --host-triple wasm32-wasip2 \
    'CFLAGS=--target=wasm32-wasip2' 'LDFLAGS=--target=wasm32-wasip2'
python3 Tools/wasm/wasi make-host --host-triple wasm32-wasip2

# 3. Install into a staging prefix to get the canonical bin/ + lib/ layout.
STAGE="$PWD/../stage"
rm -rf "$STAGE"
make -C "cross-build/wasm32-wasip2" install DESTDIR="$STAGE" >/dev/null

# 4. Assemble the trimmed artifacts the Dockerfile COPYs: the component + the
#    runtime stdlib (drop the test suite, build-config static libs, GUI toolkits
#    that are n/a on wasi, and .pyc caches).
cd ..
rm -rf python.wasm lib
cp "stage/usr/local/bin/python$PY_XY.wasm" python.wasm
mkdir -p lib
cp -R "stage/usr/local/lib/python$PY_XY" "lib/python$PY_XY"
rm -rf "lib/python$PY_XY"/{test,config-$PY_XY-wasm32-wasi,idlelib,tkinter,turtledemo,pydoc_data}
find lib -type d -name __pycache__ -prune -exec rm -rf {} + 2>/dev/null || true

echo "built plugins/python/python.wasm ($(du -h python.wasm | cut -f1)) + lib ($(du -sh lib | cut -f1))"
