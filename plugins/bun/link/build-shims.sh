#!/bin/bash
# Compile every hand-written C link-shim (link/*.c) plus the vendored C deps
# into the $OBJ/*.o objects that link_all.sh consumes. This is the single
# source of truth for how the shims are built — previously each was compiled by
# hand, which drifted (a shim edit needs its .o rebuilt, and flags like
# main_shim's mimalloc include or the object-name remaps are easy to forget).
#
# PATHS RE-ROOTED (2026-08): objects went to /tmp; now $OBJ
# (native/runtime-build/obj). Flags byte-identical.
#
# Run this, then link/link_all.sh. Needs WASI_SDK (defaults to ~/wasi-sdk).
set -euo pipefail
cd "$(dirname "$0")"

WASI_SDK="${WASI_SDK:-$HOME/wasi-sdk}"
CC="$WASI_SDK/bin/clang"
MI_INC="$(cd ../native/mimalloc/include && pwd)"
WORK="${WORK:-$(cd ../native && pwd)/runtime-build}"
OBJ="${OBJ:-$WORK/obj}"
mkdir -p "$OBJ"

# Most shims: source stem == object stem. Two are remapped in link_all.sh
# (quic_syscall_stubs -> quic_stubs, wasi-stubs -> wasi_stubs).
plain=(alloc_override environ_defer connect_wrap syscall_impls
       trap_stubs trap_stubs_cxx trap_stubs_v8)
for s in "${plain[@]}"; do
    "$CC" --target=wasm32-wasip2 -O2 -c "$s.c" -o "$OBJ/$s.o"
done
"$CC" --target=wasm32-wasip2 -O2 -c quic_syscall_stubs.c -o "$OBJ/quic_stubs.o"
"$CC" --target=wasm32-wasip2 -O2 -c wasi-stubs.c        -o "$OBJ/wasi_stubs.o"
# epoll_impl pulls in the wasi-compat <sys/epoll.h> shim (wasi-libc has none).
"$CC" --target=wasm32-wasip2 -O2 -I ../wasi-compat -c epoll_impl.c -o "$OBJ/epoll_impl.o"
# main_shim pulls in <mimalloc.h> to disable purging (mi_option_set*).
"$CC" --target=wasm32-wasip2 -O2 -I "$MI_INC" -c main_shim.c -o "$OBJ/main_shim.o"

# Vendored C deps compiled for the same link (own scripts, own fetch/flags).
WASI_SDK="$WASI_SDK" OBJ="$OBJ" bash ./build_exec_picohttp.sh   # picohttpparser, exec_host, wkexec, pipe
WASI_SDK="$WASI_SDK" OBJ="$OBJ" bash ./build_lshpack.sh          # lshpack, xxhash

echo "built all link shims + vendored C deps into $OBJ"
