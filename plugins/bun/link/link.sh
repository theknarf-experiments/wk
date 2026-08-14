#!/bin/bash
S="$OBJDIR"; N="$BUN_NATIVE"; WASI_SDK="/Users/knarf/.local/share/mise/installs/github-web-assembly-wasi-sdk/wasi-sdk-34-rc.2"
"$WASI_SDK/bin/clang++" --target=wasm32-wasip2 -fno-exceptions -O2 \
  -mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false \
  $S/bunobj/*.o /tmp/gen_*.o /tmp/wasi_stubs.o \
  "$BUN/target/wasm32-wasip2/release-dev/libbun_rust.a" \
  -L "$N/jsc-build/lib" -lJavaScriptCore -lWTF -lbmalloc \
  -L "$N/icu-wasi/install/lib" -licui18n -licuuc -licudata \
  "$N/libmimalloc.a" \
  -lsetjmp -lwasi-emulated-signal -lwasi-emulated-getpid -lwasi-emulated-mman -lwasi-emulated-process-clocks \
  -Wl,-z,stack-size=8388608 \
  -o /tmp/bun-run.wasm 2> "$S/link.log"
echo "link exit $?"
