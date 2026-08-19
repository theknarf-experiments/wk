#!/bin/bash
# Build the two object sets the networking + spawn work added to the final
# link (see link_all.sh):
#
#   * picohttpparser.o — the HTTP response parser bun_http calls
#     (phr_parse_response / phr_parse_headers / phr_decode_chunked). A vendored
#     single .c file (h2o/picohttpparser) that bun's own configure fetches; not
#     in the tree, so fetch the pinned revision and compile it. (Without this
#     the link dead-strips it away UNTIL a build change references it, then
#     fails `undefined symbol: phr_decode_chunked`. The old workaround —
#     blind `void phr_*(void){}` stubs in quic_syscall_stubs.c — is WRONG: it
#     shadows the real definition and traps at runtime, `signature_mismatch`.)
#
#   * exec_host.o + wkexec.o (+ gen/exec_host_component_type.o) — the wk:exec
#     guest bindings, so Bun.spawn can `wk_run` a program. Same generator as
#     plugins/exec-compat, but compiled for wasm32-wasip2 (the demos are
#     wasip1+adapter) and WITHOUT its own component `new` — the type object is
#     what makes bun-run.wasm `import wk:exec/process`.
#
#   * pipe.o — pipe-compat's real `pipe()` (a wk:exec bounded-buffer pipe behind
#     a wasi-libc descriptor-table fd) plus `wk_pipe_of_fd`. Bun's shell (`Bun.$`)
#     wires pipeline stages through these fds, and the wasi spawn arm uses
#     `wk_pipe_of_fd` to tell a real pipeline pipe from the node's inherited stdin.
#
# Emits $OBJ/{picohttpparser,exec_host,wkexec,pipe}.o for link_all.sh.
# PATHS RE-ROOTED (2026-08): objects went to /tmp; now $OBJ. Flags identical.
set -euo pipefail
cd "$(dirname "$0")"

WASI_SDK="${WASI_SDK:-$HOME/wasi-sdk}"
CC="$WASI_SDK/bin/clang"
WORK="${WORK:-$(cd ../native && pwd)/runtime-build}"
OBJ="${OBJ:-$WORK/obj}"
mkdir -p "$OBJ"
EXEC="$(cd ../../exec-compat && pwd)"
BUN="$(cd ../bun && pwd)"            # the (gitignored) bun checkout
PICO_COMMIT=066d2b1e9ab820703db0837a7255d92d30f0c9f5  # h2o/picohttpparser (bun's pin)

# --- picohttpparser -------------------------------------------------------
PICO_DIR="$BUN/vendor/picohttpparser"  # under the gitignored bun tree
if [ ! -f "$PICO_DIR/picohttpparser.c" ]; then
    mkdir -p "$PICO_DIR"
    curl -fsSL "https://raw.githubusercontent.com/h2o/picohttpparser/$PICO_COMMIT/picohttpparser.c" -o "$PICO_DIR/picohttpparser.c"
    curl -fsSL "https://raw.githubusercontent.com/h2o/picohttpparser/$PICO_COMMIT/picohttpparser.h" -o "$PICO_DIR/picohttpparser.h"
fi
"$CC" --target=wasm32-wasip2 -O2 -I "$PICO_DIR" -c "$PICO_DIR/picohttpparser.c" -o "$OBJ/picohttpparser.o"

# --- wk:exec bindings (wasip2) --------------------------------------------
# gen/ is produced by plugins/exec-compat/build.sh (wit-bindgen c --world
# exec-host wit --out-dir gen); the .c is target-independent, only the object
# target differs, and exec_host_component_type.o is target-neutral.
if [ ! -f "$EXEC/gen/exec_host.c" ]; then
    ( cd "$EXEC" && wit-bindgen c --world exec-host wit --out-dir gen )
fi
"$CC" --target=wasm32-wasip2 -O2 -I "$EXEC" -I "$EXEC/gen" -c "$EXEC/gen/exec_host.c" -o "$OBJ/exec_host.o"
"$CC" --target=wasm32-wasip2 -O2 -I "$EXEC" -I "$EXEC/gen" -c "$EXEC/wkexec.c" -o "$OBJ/wkexec.o"

# --- pipe-compat's pipe() (wasip2) ----------------------------------------
# Reuses the same wk:exec bindings (exec_host.h) as wkexec.o; adds the
# descriptor-table-backed pipe and wk_pipe_of_fd.
PIPE="$(cd ../../pipe-compat && pwd)"
"$CC" --target=wasm32-wasip2 -O2 -I "$PIPE" -I "$EXEC" -I "$EXEC/gen" -c "$PIPE/pipe.c" -o "$OBJ/pipe.o"

echo "built $OBJ/{picohttpparser,exec_host,wkexec,pipe}.o (+ $EXEC/gen/exec_host_component_type.o)"
