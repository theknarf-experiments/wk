# NetSurf dependency chain on wasm32-wasip1 — status

`./build-deps.sh` fetches + builds everything below into `sysroot/{lib,include,share}`
(idempotent; artifacts gitignored). Toolchain: mise-pinned wasi-sdk-34-rc.2,
same guard as plugins/bash. This is ONLY the library chain — netsurf itself and
libnsfb come later.

## The incantation that works

NetSurf's own libs (netsurf `buildsystem`) build with:

    CC=$WASI_SDK/bin/clang AR=$WASI_SDK/bin/llvm-ar RANLIB=$WASI_SDK/bin/llvm-ranlib \
    CFLAGS="--target=wasm32-wasip1 -O2 \
        -mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false \
        -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_PROCESS_CLOCKS -D_WASI_EMULATED_GETPID" \
    make install PREFIX=<abs sysroot> HOST=wasm32-wasip1 COMPONENT_TYPE=lib-static VARIANT=release

Details that matter:
* CFLAGS must arrive via the **environment**, never the make command line — the
  lib Makefiles do `CFLAGS := <their flags> $(CFLAGS)`, which appends an env
  CFLAGS but is clobbered by a command-line one.
* `HOST=wasm32-wasip1` explicitly, or Makefile.tools sniffs `$CC -dumpmachine`
  (= arm64-apple-darwin) and configures a native build.
* The sjlj/EH flags match plugins/curl exactly, so every object in the sysroot
  is link-compatible with its libcurl.a. Anything that *links an executable*
  from these objects needs `-lsetjmp` (see the smoke test / libjpeg tools).
* Tests never run: buildsystem only enables them for the `test` make goal.
* Build PATH contains no wasm-opt (curl's trick; matters for configure probes).

Verified end-to-end: a smoke program touching every archive (hubbub parse ctx,
css stylesheet, dom-via-hubbub parser, gif/bmp decoders, png/jpeg/z/expat/
utf8proc/nsutils/nslog calls) compiles, links, and **runs under wasmtime**.

## Per-lib result (all latest 3.11-era releases)

| lib | version | status | notes |
|---|---|---|---|
| buildsystem | 1.10 | installed | to `sysroot/share/netsurf-buildsystem` |
| libwapcaplet | 0.4.3 | built | clean |
| libparserutils | 0.2.5 | built | `-DWITHOUT_ICONV_FILTER` (upstream knob, builtin utf8/utf16/8859/ext8 codecs; WASI has no iconv) |
| libhubbub | 0.3.8 | built | clean |
| libcss | 0.9.2 | built | clean |
| libdom | 0.4.2 | built | `WITH_HUBBUB_BINDING=yes WITH_EXPAT_BINDING=yes`; also verified it builds expat-free (hubbub binding only) if expat ever becomes a problem |
| expat | 2.7.1 | built | classic `configure --host=wasm32-wasip1`, zero friction |
| libnsbmp | 0.1.7 | built | clean |
| libnsgif | 1.0.0 | built | clean |
| libnsutils | 0.1.1 | built | clean |
| libnslog | 0.1.3 | built | needed the one source patch of the whole exercise (below) |
| libutf8proc | 2.4.0-1 (netsurf fork) | built | clean |
| zlib | 1.3.1 | built | configure hardcodes Apple `libtool -o` on Darwin → override `AR=llvm-ar ARFLAGS=rc` at make time |
| libpng | 1.6.50 | built | cmake + `wasi-sdk-p1.cmake` toolchain, `PNG_HARDWARE_OPTIMIZATIONS=OFF`; installs `liblibpng16_static.a`, script symlinks `libpng16.a` to match libpng16.pc's `-lpng16` |
| libjpeg-turbo | 3.1.1 | built | cmake, `WITH_SIMD=0` (generic C), `WITH_TURBOJPEG=0`; its cjpeg/djpeg tools need `CMAKE_EXE_LINKER_FLAGS=-lsetjmp` under the sjlj lowering |
| nsgenbind | 0.9 | **SKIPPED** | host tool; grammars use `%code` = bison >= 2.4, and everything on this Mac (system + CommandLineTools) is bison 2.3. Only needed when building netsurf WITH duktape/JS; a no-JS framebuffer build doesn't invoke it. Remedy: `brew install bison`, then `BISON=/opt/homebrew/opt/bison/bin/bison ./build-deps.sh` — the script gates on version and picks it up. |

## Patches / shims (patch-minimalism ledger)

1. **libnslog** (only source edit, done as a loud `perl -0pi` in build-deps.sh):
   strip the grammar's per-type `%destructor { nslog_filter_unref($$); } <filter>`
   — bison-2.4+ syntax that host bison 2.3 can't parse. Cost: a small leak on
   parse errors of *malformed filter strings* only. Paired with
   `compat/nslog-bison23.h` (force-included via `-include`): bison 2.3's
   generated header omits the `filter_parse` prototype, and clang's C99
   implicit-declaration error stops the build without it. Both are inert if a
   modern bison is ever used.
2. Everything else is make-variable overrides / upstream knobs only. No other
   upstream file is touched.

## libcurl.a (do NOT rebuild)

Present and reusable: `plugins/curl/curl-8.11.1/lib/.libs/libcurl.a`
(852 KB, wasm objects verified by magic). Built by plugins/curl/build.sh with:
`--target=wasm32-wasip2 -O2` + the same sjlj/EH + `-D_WASI_EMULATED_*` flags +
`-DHAVE_GETADDRINFO=1 -DHAVE_FREEADDRINFO=1`; configure
`--host=wasm32-wasi --without-ssl --without-libpsl --without-zlib
--without-brotli --without-zstd --without-libidn2 --without-nghttp2
--disable-shared --enable-static --disable-threaded-resolver --disable-ntlm
--disable-unix-sockets --disable-socketpair ac_cv_header_sys_un_h=no`.
Plain HTTP only (no TLS backend), sockets/DNS via the wasip2 fabric.

**Target-triple note for the eventual netsurf link:** libcurl.a is wasip2, this
sysroot is wasip1. wasm object files mix fine (same wasm32 ABI; the final link
picks the libc/sysroot), but curl's resolver+sockets only exist in wasip2's
libc — so expect the netsurf binary itself to be linked `--target=wasm32-wasip2`
against this sysroot, exactly like plugins/bash links its wasip1-era shims into
a wasip2 component. If a pure-wasip1 netsurf is ever wanted, curl would need a
rebuild instead.

## Not yet built (deliberately out of scope)

netsurf itself, libnsfb, libsvgtiny (pulls in gperf-generated colours, decide
with the netsurf build), libnspsl, librosprite/librufl/libpencil (RISC OS-only).
