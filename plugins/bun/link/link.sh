#!/bin/bash
S="$WORK"; N="$BUN_NATIVE"; WASI_SDK="$WASI_SDK"
"$WASI_SDK/bin/clang++" --target=wasm32-wasip2 -fno-exceptions -O2 \
  -mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false \
  $S/bunobj/*.o $OBJ/gen_*.o $OBJ/mod_*.o $OBJ/wasi_stubs.o $OBJ/imrc.o $OBJ/hdr_*.o $VLIB/libzstd.a $VLIB/libbrotli.a $VLIB/libbssl_crypto.a $VLIB/libusockets.a $OBJ/libuwsockets.o \
  "/Users/knarf/projects/theknarf-experiments/wk/plugins/bun/bun/target/wasm32-wasip2/release-dev/libbun_rust.a" \
  -L "$N/jsc-build/lib" -lJavaScriptCore -lWTF -lbmalloc \
  -L "$N/icu-wasi/install/lib" -licui18n -licuuc -licudata \
  "$N/libmimalloc.a" \
  -lsetjmp -lwasi-emulated-signal -lwasi-emulated-getpid -lwasi-emulated-mman -lwasi-emulated-process-clocks \
  -Wl,-z,stack-size=8388608 -Wl,--error-limit=0 \
  -o $OUT/bun-run.wasm 2> "$S/link.log"
echo "link exit $?"
