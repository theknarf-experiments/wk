# bun-run wasm link

Cross-compiles the FULL bun+JSC runtime to a wasm32-wasip2 component
(`bun-run.wasm`, ~181 MB) that runs as a wk node. (The Phase-1
`bun-transpile.wasm` from `../build.sh` is a separate, smaller JSC-free slice.)

## Build flow

1. `../build-jsc.sh` — JavaScriptCore (cloop/no-JIT) + ICU for wasi. Slow.
2. `../gen-codegen.ts` — the configure-time codegen (`generate-classes`,
   bindgenv2 option-structs, `runtime.out.js`) bun's own build would produce.
3. `cargo +nightly-<pin> build -p bun_bin --target wasm32-wasip2 --profile
   release-dev` (with `BUN_CODEGEN_DIR` set) → `libbun_rust.a`.
4. Vendored C libraries, cross-built for wasip2:
   - BoringSSL — `build_boringssl.sh` + `build_bssl_ssl.sh`
   - uSockets + uWebSockets — `build_usockets.sh` (recompile after touching
     `packages/bun-usockets/**` headers, e.g. the `Bun__addrinfo_set`/
     `zig_mutex_t` wasi arms)
   - zstd / brotli / c-ares / zlib-ng / libarchive / hdrhistogram /
     libdeflate / sqlite3 / llhttp / libspng / libjpeg-turbo — `build-vendored.sh`
     (+ hand-written `configs/*.h` for the ones whose configure can't run)
   - picohttpparser + the `wk:exec` guest bindings — `build_exec_picohttp.sh`
5. The C shims in this dir (see below), each compiled to `/tmp/<name>.o`.
6. `link_all.sh` — the final `clang++` link into `/tmp/bun-run.wasm`
   (`wasm-component-ld` emits the component). Package it into an image with a
   `FROM scratch` Dockerfile (`COPY bun-run.wasm /bin/bun.wasm`).

Paths in `link_all.sh` are absolute snapshots (scratchpad `/tmp` intermediates +
this checkout); it documents the exact recipe rather than being hermetic.

## C shims (this dir)

- `main_shim.c` — `__main_argc_argv` startup (bun's compiled Rust `main`
  deadlocks; this replicates its init order). Kept as a GC root via
  `-Wl,--export=__main_argc_argv`.
- `alloc_override.c` — route C malloc/free/realloc → mimalloc (one heap;
  else cabi_realloc-mimalloc vs wasi-libc-dlmalloc corrupts on cross-free).
- `environ_defer.c` — `-Wl,--wrap=__wasilibc_initialize_environ`: skip the
  eager environ ctor (it allocates during ctors, before the clock import is
  callable); `main_shim.c` drives it post-ctors.
- `connect_wrap.c` — `-Wl,--wrap=connect`: blocking connect so wasip2's
  two-phase TCP finish-connect is driven (uSockets assumes BSD semantics).
- `epoll_impl.c` — epoll-over-poll with a level-triggered busy-poll fallback
  (wasi-libc `poll()` is ENOTSUP for wasip2 socket fds). Drives serve/fetch/WS
  readiness.
- `syscall_impls.c`, `wasi-stubs.c` — small libc gaps (eventfd via dup, etc.).
- `trap_stubs.c` / `trap_stubs_cxx.c` / `trap_stubs_v8.c` /
  `quic_syscall_stubs.c` — blind `__builtin_trap()` stubs for symbols that
  cannot run on wasip2 (QUIC/HTTP3 over UDP, the v8 addon shim, inspector).
  LLD inserts trapping thunks; they only fire if actually called.
  NOTE: do NOT stub real functions here (e.g. picohttpparser's `phr_*`) — a
  blind `void f(void){}` shadows the real definition and traps at runtime
  with `signature_mismatch`. Build the real object instead.

## Status: WORKS

`bun-run.wasm` runs real JS/TS on JSC as a wk node: modules, timers, async I/O,
fs/path/Buffer/crypto, sqlite, WebCrypto, `Bun.serve` (HTTP + WebSocket), raw
TCP, `fetch` (GET/POST, by-name, and real-internet HTTP+HTTPS via a gateway),
and `Bun.spawn`/`spawnSync` → `wk:exec` (run programs, exit codes, stdout/stderr
capture, async, fixed stdin). Remaining gaps are documented in the
`wk-bun-port` memory (streamed stdin, IPv6 string rendering, the `WebAssembly`
global — JSC cloop has no wasm tier).
