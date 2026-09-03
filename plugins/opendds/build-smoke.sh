#!/usr/bin/env bash
# Build and (with `run`) execute the ACE smoke test.
#
#   ./build-smoke.sh        build smoke/ace-smoke.wasm
#   ./build-smoke.sh run    build it and run it under wasmtime
#
# See smoke/ace-smoke.cpp for what it checks and why each check earns its
# place. The short version: compiling ACE proves the headers agree with
# wasi-libc; only linking and RUNNING proves the exception encoding, the libc
# symbols and the shims do.
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

require_wasi_sdk
ACE_ROOT="$SRC/ACE_wrappers"
[ -f "$ACE_ROOT/lib/libACE.a" ] || {
  echo "opendds: no libACE.a — run ./build-target.sh ace first" >&2; exit 1; }

"$HERE/build-shim.sh"

log "linking the smoke test"
# The flag set is ace/platform_wasi.GNU's, because every object in libACE.a was
# built with it and mixing exception encodings is rejected at instantiate time
# rather than at link.
"$WASI_SDK/bin/clang++" \
  --target=wasm32-wasip2 \
  -std=c++17 -O2 \
  -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_MMAN \
  -D_WASI_EMULATED_PROCESS_CLOCKS -D_WASI_EMULATED_GETPID \
  -fwasm-exceptions -mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false \
  -DACE_AS_STATIC_LIBS \
  -I"$ACE_ROOT" -I"$HERE/shim/include" \
  "$HERE/smoke/ace-smoke.cpp" \
  -Wl,--whole-archive "$HERE/shim/libwkopendds.a" -Wl,--no-whole-archive \
  "$ACE_ROOT/lib/libACE.a" \
  -lunwind -lsetjmp \
  -lwasi-emulated-signal -lwasi-emulated-mman \
  -lwasi-emulated-process-clocks -lwasi-emulated-getpid \
  -Wl,-z,stack-size=8388608 \
  -o "$HERE/smoke/ace-smoke.wasm"

echo "built plugins/opendds/smoke/ace-smoke.wasm"

if [ "${1:-}" = "run" ]; then
  log "running under wasmtime"
  # -S inherit-network for the loopback UDP round trip. Under wk the same
  # sockets land on the node's fabric instead; wasmtime is just the quickest
  # place to find out whether the binary loads at all.
  wasmtime run -W exceptions -S inherit-network "$HERE/smoke/ace-smoke.wasm"
fi
