# bun-run wasm link

Cross-compiles bun+JSC for wasm32-wasip2. The C++/Rust/JSC layer is 100%
done (see cxx-flags.rsp, gen-codegen.ts). This dir finishes the VENDORED C
LIBRARY cross-builds and the final link.

## Built so far (undefined 1287 -> 787):
BoringSSL (build_boringssl.sh + build_bssl_ssl.sh), uSockets+uWebSockets
(build_usockets.sh + libuwsockets.cpp with -include root.h), zstd/brotli
(build-vendored.sh), c-ares (configs/ares_config.h), zlib-ng (compat mode:
generate zlib.h from zlib.h.in, mangling headers from .empty), libarchive
(configs/libarchive_config.h, tar+gzip subset), hdrhistogram, root_certs.cpp.

## Remaining (~787):
QUIC/HTTP3 (~198 us_/uws_h3/lsquic — lsquic NOT fetched), libarchive gzip
(29 — rebuild its gzip filter now that zlib.h exists), simdutf 15 + WebP 15
(fetch + build), Bun::Secrets 3 (stub), misc v8-shim singles (stub).

## Configs: link/configs/ (ares_config.h, libarchive_config.h — hand-written
for wasi). zlib-ng headers generated in native/zlib/ at build time.
