#!/usr/bin/env bash
# Package an UNMODIFIED upstream CPython 3.12 (the real C interpreter) as a wk
# container. Proves wk ships a full production scripting runtime — not a toy: the
# complete standard library rides along in python312.zip, so `import http.server`,
# json, sqlite3, etc. all work.
#
# We don't compile CPython here (a from-source WASI build is a ~20-minute affair
# needing a matching host python); instead we fetch VMware Wasm Labs' prebuilt
# wasm32-wasi CPython — an unpatched upstream build — and do the one wk-specific
# step: turn the wasip1 *module* into a wasi:cli command *component* by grafting
# the preview1->preview2 adapter (exactly as the lua/sqlite plugins do). The
# Dockerfile then lays it out as an OCI image (interpreter + stdlib + PYTHONHOME).
#
# NB: the adapter stubs the wasip1 socket extension (sock_send/recv), so this
# build runs scripts and the full stdlib but cannot open sockets — a networked
# CPython (webserver on the fabric) needs a from-source wasip2 build. See the
# netserve plugin for the sockets-on-the-fabric proof.
#
# Requires wasm-tools. The runtime is fetched (and cached) under bin/ + usr/ on
# first run. Then build the image with `docker://plugins/python/Dockerfile`.
set -euo pipefail
cd "$(dirname "$0")"

PY_VER=3.12.0
REL="python/3.12.0%2B20231211-040d5a6"
TARBALL="python-$PY_VER-wasi-sdk-20.0.tar.gz"
URL="https://github.com/vmware-labs/webassembly-language-runtimes/releases/download/$REL/$TARBALL"
ADAPTER="${WASI_ADAPTER:-$(find "$HOME/.cargo/registry/src" -name 'wasi_snapshot_preview1.command.wasm' 2>/dev/null | head -1)}"

if [ ! -f "bin/python-$PY_VER.wasm" ]; then
    echo "fetching CPython $PY_VER (wasm32-wasi)..."
    curl -fsSL "$URL" -o "$TARBALL"
    tar xzf "$TARBALL"        # -> bin/python-$PY_VER.wasm, usr/local/lib/{python312.zip,python3.12/}
    rm -f "$TARBALL"
fi

# The one wk step: wasip1 module -> wasi:cli command component.
wasm-tools component new "bin/python-$PY_VER.wasm" --adapt "$ADAPTER" -o python.wasm
echo "built plugins/python/python.wasm ($(du -h python.wasm | cut -f1))"
