#!/bin/bash
S="/private/tmp/claude-501/-Users-knarf-projects-theknarf-experiments-wk/30610b6d-8ee0-4b7b-901e-7d0641cd3850/scratchpad"; N="/Users/knarf/projects/theknarf-experiments/wk/plugins/bun/native"; WASI_SDK="/Users/knarf/.local/share/mise/installs/github-web-assembly-wasi-sdk/wasi-sdk-34-rc.2"
"$WASI_SDK/bin/clang++" --target=wasm32-wasip2 -fno-exceptions -O2 \
  -mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false \
  /tmp/alloc_override.o /tmp/environ_defer.o $S/bunobj/*.o /tmp/gen_*.o /tmp/mod_*.o /tmp/wasi_stubs.o /tmp/quic_stubs.o /tmp/epoll_impl.o /tmp/syscall_impls.o /tmp/trap_stubs.o /tmp/trap_stubs_cxx.o /tmp/trap_stubs_v8.o /tmp/imrc.o /tmp/hdr_*.o /tmp/vlib/libzstd.a /tmp/vlib/libbrotli.a /tmp/vlib/libbssl_crypto.a /tmp/vlib/libusockets.a /tmp/libuwsockets.o /tmp/us_root_certs.o /tmp/vlib/libcares.a /tmp/vlib/libarchive.a /tmp/vlib/libz.a /tmp/vlib/libdeflate.a /tmp/vlib/libsqlite3.a /tmp/vlib/libllhttp.a /tmp/vlib/libspng.a /tmp/vlib/libturbojpeg.a /tmp/bun_simdutf.o /tmp/mainobj/bun_main.o /tmp/main_shim.o \
  "/Users/knarf/projects/theknarf-experiments/wk/plugins/bun/bun/target/wasm32-wasip2/release-dev/libbun_rust.a" \
  -L "$N/jsc-build/lib" -lJavaScriptCore -lWTF -lbmalloc \
  -L "$N/icu-wasi/install/lib" -licui18n -licuuc -licudata \
  "$N/libmimalloc.a" \
  -lsetjmp -lwasi-emulated-signal -lwasi-emulated-getpid -lwasi-emulated-mman -lwasi-emulated-process-clocks \
  -Wl,-z,stack-size=8388608 -Wl,--error-limit=0 -Wl,--allow-multiple-definition -Wl,--wrap=__wasilibc_initialize_environ -Wl,--export=cabi_realloc -Wl,--export=main -Wl,--export=__main_argc_argv \
  -o /tmp/bun-run.wasm 2> "$S/link.log"
echo "link exit $?"
