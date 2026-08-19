#!/bin/bash
# The final bun-run link: every object and archive the stages before it
# produced, in an order that matters, through wasi-sdk clang++ (which drives
# wasm-component-ld and emits the component directly) → $WORK/bun-run.wasm.
#
# PATHS RE-ROOTED (2026-08): this used to be an absolute-path snapshot of
# the original porting session (/tmp objects + a session scratchpad); inputs
# now come from native/runtime-build ($WORK/$OBJ/$VLIB — see build-runtime.sh
# for the env contract). The link line's CONTENT (object order, libraries,
# flags) is byte-identical to the snapshot.
#
# Order/flag invariants, each learned the hard way:
#   * alloc_override.o comes FIRST + -Wl,--allow-multiple-definition: its
#     malloc/free/realloc must preempt wasi-libc's dlmalloc so mimalloc is
#     the ONE heap (else cabi_realloc-mimalloc vs libc-dlmalloc corrupts on
#     cross-free).
#   * bunobj/*.o precede the trap stubs, so real definitions win over blind
#     __builtin_trap() stubs under --allow-multiple-definition.
#   * `main` is force-exported so the archive's own main object is pulled in.
#     Do not re-add a standalone bun_main.o: a stale hand-compiled copy once
#     referenced bun_core internals (OS_ARGV/OS_ARGC) by codegen-unit hash,
#     so any Rust crate change rehashed those symbols and broke the link
#     with "undefined symbol".
#   * run with wasm-opt OFF the PATH (build-runtime.sh does) — the sjlj/EH
#     instructions in the JSC/bindings objects predate wasm-opt's parser.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
P="${BUN_PLUGIN:-$(cd "$HERE/.." && pwd)}"
N="${BUN_NATIVE:-$P/native}"
B="${BUN:-$P/bun}"
WORK="${WORK:-$N/runtime-build}"
OBJ="${OBJ:-$WORK/obj}"
VLIB="${VLIB:-$WORK/vlib}"
WASI_SDK="${WASI_SDK:?set WASI_SDK (wasi-sdk-34-rc.2)}"
mkdir -p "$WORK/logs"

"$WASI_SDK/bin/clang++" --target=wasm32-wasip2 -fno-exceptions -O2 \
  -mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false \
  "$OBJ/alloc_override.o" "$OBJ/environ_defer.o" "$WORK/bunobj"/*.o "$OBJ"/gen_*.o "$OBJ"/mod_*.o "$OBJ/wasi_stubs.o" "$OBJ/quic_stubs.o" "$OBJ/picohttpparser.o" "$OBJ/exec_host.o" "$OBJ/wkexec.o" "$OBJ/pipe.o" "$OBJ/lshpack.o" "$OBJ/xxhash.o" "$P/../exec-compat/gen/exec_host_component_type.o" "$OBJ/connect_wrap.o" "$OBJ/epoll_impl.o" "$OBJ/syscall_impls.o" "$OBJ/trap_stubs.o" "$OBJ/trap_stubs_cxx.o" "$OBJ/trap_stubs_v8.o" "$OBJ/imrc.o" "$OBJ"/hdr_*.o "$VLIB/libzstd.a" "$VLIB/libbrotli.a" "$VLIB/libbssl_crypto.a" "$VLIB/libusockets.a" "$OBJ/libuwsockets.o" "$OBJ/us_root_certs.o" "$VLIB/libcares.a" "$VLIB/libarchive.a" "$VLIB/libz.a" "$VLIB/libdeflate.a" "$VLIB/libsqlite3.a" "$VLIB/libllhttp.a" "$VLIB/libspng.a" "$VLIB/libturbojpeg.a" "$OBJ/bun_simdutf.o" "$OBJ/main_shim.o" \
  "$B/target/wasm32-wasip2/release-dev/libbun_rust.a" \
  -L "$N/jsc-build/lib" -lJavaScriptCore -lWTF -lbmalloc \
  -L "$N/icu-wasi/install/lib" -licui18n -licuuc -licudata \
  "$N/libmimalloc.a" \
  -lsetjmp -lwasi-emulated-signal -lwasi-emulated-getpid -lwasi-emulated-mman -lwasi-emulated-process-clocks \
  -Wl,-z,stack-size=8388608 -Wl,--error-limit=0 -Wl,--allow-multiple-definition -Wl,--wrap=__wasilibc_initialize_environ -Wl,--wrap=connect -Wl,--export=cabi_realloc -Wl,--export=main -Wl,--export=__main_argc_argv \
  -o "$WORK/bun-run.wasm" 2> "$WORK/logs/link.log"
rc=$?
echo "link exit $rc (log: $WORK/logs/link.log)"
exit $rc
