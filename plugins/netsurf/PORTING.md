# NetSurf on wasm32-wasip2 — a real browser as a wk node

Two layers, two scripts:

* `./build-deps.sh` — the dependency-library chain (below, unchanged).
* `./build.sh` — libnsfb (with a new `wk` surface backend) + NetSurf 3.11
  itself, `TARGET=framebuffer`, linked wasip2-direct into
  `netsurf.wasm` + staged runtime resources in `res/`, packaged by the
  `Dockerfile` (`docker://plugins/netsurf/Dockerfile` in workspace.wk).

## The browser build (what stuck)

### libnsfb 0.2.2 + the wk surface (`surface/wk.c`)

A new backend file copied in beside sdl.c/ram.c (registered via the stock
`NSFB_SURFACE_DEF` constructor macro — the fb frontend Makefile links libnsfb
`--whole-archive`, so the constructor survives static linking):

* **Format**: the framebuffer lives in `NSFB_FMT_XBGR8888` — the 32bpp word
  `0xAABBGGRR`, whose little-endian bytes are exactly the r,g,b,a layout
  wasi:frame-buffer wants. No swizzle; presents only force the alpha byte to
  0xff (libnsfb's plotters write netsurf colours verbatim, alpha 0).
  `geometry()` *ignores the requested format*: NetSurf asks for XRGB8888
  purely as a "bpp 32" proxy (framebuffer_format_from_bpp), and honouring it
  would mean a per-pixel swizzle on every present.
* **Presents**: `update()` presents the whole frame (wkgfx scales/letterboxes
  against the live surface size); partial-rect bookkeeping isn't worth it.
* **Input**: wkgfx events → nsfb events via a small ring (one wkgfx event can
  fan out): pointer move → MOVE_ABSOLUTE; buttons 0/1/2 → MOUSE_1/2/3
  press/release; a scroll tick → a MOVE_ABSOLUTE warp to the pointer then
  MOUSE_4 (up) / MOUSE_5 (down) press+release pairs, one per line, capped at
  10 — sdl.c's wheel convention, which gui.c turns into ±100px scrolls;
  WKGFX_RESIZE → NSFB_EVENT_RESIZE (gui_resize → set_geometry → realloc).
  Keys map by unshifted position (fbtk applies its own shift keymap), with a
  printable-ASCII fallback from the event's text scalar for unmapped keys.
* **Pacing**: `input(timeout != 0)` blocks on `wkgfx_wait_frame()` and
  reports `NSFB_CONTROL_TIMEOUT` if the frame carried no input — the same
  contract sdl.c implements with a wake timer, with the compositor's frame
  clock (~16ms) as the timer. NetSurf's scheduler only needs *some* periodic
  wakeup: fetches progress via `fetcher_poll` rescheduled every 10ms, and
  `fetch_curl_poll` is pure `curl_multi_perform` — **no select() anywhere on
  the framebuffer frontend's path** (fetch_fdset exists but only monkey/gtk
  call it; the fdset dance inside fetch_curl_poll is debug logging). So
  neither select nor curl_multi_wait was needed.
* **Cursor**: the default no-op — the compositor draws the host pointer.

Surface selection: netsurf enumerates registered surfaces and defaults to the
lowest-numbered type. `NSFB_SURFACE_WK` is patched in right after
`NSFB_SURFACE_NONE` so wk (not the always-compiled ram surface) is the
default; `-f wk` also works.

### netsurf 3.11 configuration

    make TARGET=framebuffer PREFIX=/usr SHELL=/bin/bash
      NETSURF_USE_DUKTAPE=NO           # no JS → skipped nsgenbind never runs
      NETSURF_USE_OPENSSL=NO           # curl has no TLS backend anyway
      NETSURF_USE_CURL=YES  NETSURF_USE_JPEG=YES
      NETSURF_USE_JPEGXL=NO NETSURF_USE_WEBP=NO
      NETSURF_USE_NSSVG=NO  NETSURF_USE_ROSPRITE=NO  NETSURF_USE_NSPSL=NO
      NETSURF_FB_FONTLIB=internal      # no freetype; glyphs compiled in

with env `CC=<wasi clang>`, env `CFLAGS="--target=wasm32-wasip2 -O2 <sjlj/EH>
-D_WASI_EMULATED_{SIGNAL,PROCESS_CLOCKS,GETPID} -D_WASI_EMULATED_MMAN
-I sysroot/include"` (env, not command line — netsurf appends to an inherited
CFLAGS but a command-line one clobbers its internals), env
`LDFLAGS="--target=wasm32-wasip2 <wkgfx.o, bindings .o, component-type .o>"`
(the link step runs `$(CC) $(LDFLAGS)` without CFLAGS — bash's lesson), and
`PKG_CONFIG_LIBDIR=sysroot/lib/pkgconfig` so every probe answers from the
sysroot. Enabled fetchers: curl + data + file + about + resource (the last
three are unconditional). png/jpeg/gif/bmp handlers on. `PREFIX=/usr` bakes
`/usr/share/netsurf` into the resource search path.

Host tools (convert_image, convert_font, split-messages, xxd) build with
`BUILD_CC=cc`; convert_image links the *host's* libpng, whose flags are
computed with the host pkg-config **before** PKG_CONFIG_LIBDIR is pinned to
the sysroot, and passed as `BUILD_LIBPNG_{CFLAGS,LDFLAGS}`.

Things that never became problems: iconv (musl's real iconv is in wasi-libc —
utils/utf8.c just works), select (see above), mkdir/stat (wasi-libc has
them; Choices/Cookies live under $HOME=/root and their absence is handled),
`-Wl,--trace`+`--whole-archive` (wasm-ld speaks both), getopt_long/realpath
(in wasi-libc).

### Patch ledger additions (all applied as loud, guarded ops in build.sh)

3. **libnsfb include/libnsfb.h**: insert `NSFB_SURFACE_WK` into the surface
   type enum (a fixed upstream list; no dynamic ids), right after
   NSFB_SURFACE_NONE so wk is netsurf's default surface.
4. **libnsfb src/surface/Makefile**: add wk.c to the hardcoded
   always-compiled surface list. (surface/wk.c itself is a new file, copied
   in — not an edit.)
5. **netsurf content/fetchers/curl.c**: `SETOPT(CURLOPT_ENCODING, "gzip")` →
   `NULL`. Our libcurl.a is --without-zlib; advertising gzip would get
   responses netsurf renders as compressed garbage. NULL sends no
   Accept-Encoding at all.
6. **sysroot/lib/pkgconfig/libnsfb.pc** (generated file post-processing, not
   an upstream edit): append `-lsetjmp -lwasi-emulated-{signal,
   process-clocks,getpid,mman}` to its Libs. libnsfb is the *last* library on
   netsurf's link line, and env LDFLAGS lands *before* every pkg-config lib —
   this is the only spot late enough for archives that must resolve
   libpng/libjpeg/curl setjmp+emulation references.
7. **sysroot/lib/pkgconfig/libcurl.pc** (generated whole): points netsurf's
   pkg-config probe at plugins/curl's existing wasip2 libcurl.a.
8. **SHELL=/bin/bash on the make command line**: the LINKDEPS recipe uses
   `echo -n`; macOS /bin/sh writes a literal "-n" into link.d, which kills
   the *next* make run with "missing separator".

### Runtime resources / packaging

build.sh stages into `res/`: Messages (the build's fb-filtered English
messages, **gzipped** — netsurf reads Messages via zlib's gzopen, so it ships
compressed as-is), adblock/default/internal/quirks.css, welcome/credits/
licence.html, netsurf.png, favicon.png. The Dockerfile COPYs netsurf.wasm to
/netsurf.wasm and res/ to /usr/share/netsurf (the compiled-in search path;
`${HOME}/.netsurf` and `${NETSURFRES}` are also consulted at runtime), sets
HOME=/root, entrypoint `/netsurf.wasm` — no URL argument, so the browser
opens `about:welcome` → `resource:welcome.html` from the image: first paint
needs zero network. Toolbar icons, pointers, throbber frames and the internal
font are compiled into the binary by the build tools, not shipped as files.

Known limitations: HTTP only (no TLS backend in libcurl.a — https:// fails
cleanly), no JavaScript (duktape off), no gzip transfer encoding, no SVG.

### Tests / example

* `wk-server plugin.rs::netsurf_paints_its_welcome_page` — boots the browser
  headless (resources seeded into the node fs, standing in for the image
  COPY), pumps compositor frames, asserts a non-uniform paint.
* `wk-server plugin.rs::netsurf_fetches_over_the_fabric` — an HTTP server as
  the named fabric peer "websrv" on the node's net (the httpfs test's
  pattern) serves a solid-red page; netsurf launched at http://websrv:8080/
  must render a run of pure-red pixels.
* `wk-server workspace.rs::browser_example_wires_netsurf_to_a_web_server` —
  guards `example/browser.wk` (netsurf + CPython http.server on a shared
  network, browsing `example/browser-www/` by fabric name).

---

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
