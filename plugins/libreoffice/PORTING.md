# LibreOffice Impress on wasm32-wasip2 — a real office application as a wk node

The goal: run **real LibreOffice Impress** in wk — menu bar, toolbars, slide
panel, editing, dialogs, and a running slideshow — so a user can create and show
PowerPoint files inside a wk workspace. Not a viewer, not a tile renderer, not a
UI we wrote ourselves.

LibreOfficeKit was considered and **rejected**: it paints document tiles only,
with no application chrome, which would mean writing an Impress UI ourselves.

The shape is a **wk VCL backend**. VCL is LibreOffice's own toolkit. Its
`svp`/headless backend already draws the *entire* application — menus,
toolbars, dialogs, the slide panel — into an offscreen Cairo bitmap, because it
declines every native-integration hook and VCL then falls back to drawing
widgets itself. The port presents that bitmap through `plugins/gfx-compat` and
feeds wk input back as VCL events, exactly as `plugins/qt/qpa` does for Qt and
`plugins/netsurf`'s `surface/wk.c` does for libnsfb.

This document is modelled on `plugins/qt/PORTING.md` and assumes you have read
it. Where the two ports face the same problem — EH flags, the wasm-opt trap,
the component-type object, the no-threads reality — this file says "same as Qt"
and does not repeat the reasoning.

## What is in this directory today

* `src/` — a `git clone --depth 1` of LibreOffice core at tag
  **`libreoffice-26.2.6.2`**, commit `ad5cf9fd4989cacf0bca866ebefc0ec8926cb0b2`
  (2026-08-31). 1.8 GB, gitignored, **never edited in place**.
* `.gitignore` — nested rather than a block in the repo root, deliberately;
  the header comment says why. Fold it into the root file later if you prefer
  the convention.
* `clone.log` — the fetch.
* `PORTING.md` — this file. **The plan; `## Current state` near the end is what
  actually happened.** Impress runs, draws and takes input; the design sections
  below were written before any of it was built and are kept because the
  reasoning still holds, not because every sentence is still current.
* `run-lo.sh` — run the built `soffice.bin` under wasmtime with the mounts it
  cannot do without. Not how it runs in wk (that is `example/impress.wk`), but
  how it is debugged.
* `shim/` — two shims on every link line: the wasip2 thread shim, and the
  gfx-compat archive that `vcl/wk` draws through.
* `common.sh` — the shared preamble every stage sources: the `WASI_SDK` guard,
  the layout, the exception/sjlj flag set, the two PATHs, and the rule about
  which flags may travel through the environment (`CFLAGS`/`LDFLAGS` may not —
  gbuild treats them as *replacements*, not additions).
* `preflight.sh` — probes only. Compiles nothing, configures nothing, downloads
  nothing, never touches `src/`. Run it first.
* `build-configure.sh` (M0) → `build-host.sh` (M1) → `build-lo.sh` (M2/M3) —
  the three stages, each idempotent, each `tee`-ing into `./logs`.
* `shim/wk-wasi-threads.c` + `build-shim.sh` — the wasip2 thread shim: a
  one-file static archive that overrides `pthread_cond_timedwait` so a wait
  which can only ever time out returns `ETIMEDOUT` instead of letting libc++
  `abort()`. Deliberately outside `src/` and outside `patches/`, because as a
  link-line override it also covers libc++, libc++abi and the externals. All
  three build stages run it; `WASI_INTEL_GCC.mk` refuses to parse without it.
  See decision 12.
* `patches/` — `core-0001` … `core-0008` plus its `README.md`. The two the
  header calls structural (`core-0001` the `configure.ac` host arm,
  `core-0002` the gbuild platform file) are the two `build-configure.sh`
  refuses to run without. `core-0006` is every small in-file `#if defined(WASI)`
  gap in `osl`; `core-0007` is the three `osl` subsystems that diverge whole
  (see decision 14); `core-0008` is a one-word host-portability fix to
  `unxgcc.mk` that has nothing to do with wasm (decision 15).
* `mise.toml` — the staged tasks. Its `build` task was written to **self-skip**
  while `patches/` is empty, so the repo-wide `mise run build-plugins` sweep
  would not sit in a LibreOffice build.
  **⚠ That guard no longer fires, and nobody noticed.** `mise.toml:105` tests
  `ls patches/core-*.patch`, and `patches/` has held eight of them since M2. So
  `mise run build` now runs `build-deps` → `build-configure` → `build-host` →
  `./build-lo.sh` **with no argument**, i.e. the full M3 link — which is known
  to fail, because `bridges` does not compile (see **Current state**). Any
  repo-wide sweep that reaches this plugin will now spend minutes and then
  fail. Left as-is deliberately rather than quietly rewritten: the guard's
  *intent* is "skip until the port can complete a build", and choosing the new
  condition — M3 linking, or an explicit opt-in variable — is a decision about
  what the sweep is for, not a fact about the port. Whoever picks one should
  also update this bullet and `mise.toml:101`'s description string.

**There is no `vcl/wk/` yet**, so no rung above M3 has been attempted. What
*has* been built is in **Current state**.

Why 26.2.6.2 and not the newest tag: `git ls-remote` shows 26.8 only just
branched (`libreoffice-26-8-branch-point` → `26.8.0.0.alpha1` → `26.8.0.3`), so
26.8.0.3 is a days-old `x.y.0`. 26.2 is the maintained "Still" line with six
bugfix rounds behind it. For a multi-week port you do not want to be debugging
somebody else's fresh-release regressions alongside your own.

---

## The honest answer: is this reachable?

**Yes, but only with a patch set roughly five times the size of the Qt port's,
and one entire axis of it has no upstream precedent to copy.**

Characterising N, from the survey rather than from optimism:

| bucket | scale | precedent |
|---|---|---|
| `configure.ac` WASI host arm + 6 satellite edits | ~60 lines, 1 patch | the `emscripten)` arm, 20 lines away |
| `solenv/gbuild/platform/WASI_INTEL_GCC.mk` (new) + `unxgcc.mk` link flags | ~150 lines, 2 patches | `EMSCRIPTEN_INTEL_GCC.mk`, 122 lines |
| the four `BUILD_TYPE_FOR_HOST=EMSCRIPTEN` gates | 4 one-liners | exact |
| UNO C++ bridge: replace the `EM_JS` symbol lookup | ~100 lines, 2 patches | **none** |
| `sal`/`desktop`/`vcl` libc gaps (pwd.h, dlsym, atfork, ini path, VclBuilder) | ~8 small patches | the `ANDROID`/`EMSCRIPTEN` arms beside each |
| **threadless LibreOffice** | ~10 patches, and an unbounded tail | **none** |
| `external/*` WASI arms (15 externals mention EMSCRIPTEN across 24 files) | ~15 patches | the EMSCRIPTEN arm beside each |
| `vcl/wk/` — our backend, new files not patches | ~1,500–2,500 lines | `plugins/qt/qpa` (2,235 lines), `vcl/qt5/QtFrame.cxx` |

So: **40–60 patch files and low thousands of lines of diff, of which only
~500 lines are genuinely novel logic** (the guest-side compositor and the yield
loop). Most of the rest is writing a `wasi*)` arm beside an `emscripten)` arm
that already exists. That is the good news, and it is why this is a port and
not a research project.

The bad news is the row with no precedent. **Upstream's Emscripten port is a
fully threaded build** — `EMSCRIPTEN_INTEL_GCC.mk:14` is `-pthread -s
USE_PTHREADS=1` unconditionally, `:21` is `-sPTHREAD_POOL_SIZE=7`,
`desktop/Executable_soffice_bin.mk` adds `-sPROXY_TO_PTHREAD=1`, and
`static/README.wasm.md` requires COOP/COEP headers. `configure.ac` is 16,474
lines and contains **no `--disable-threads` of any kind**. Single-threaded
LibreOffice is not a configuration that exists; it is a patch set we write and
maintain. The brief's framing — "our delta is Emscripten → WASI" — understates
the work by one whole axis, and that correction is the single most important
thing in this document.

Two mitigations make it tractable rather than open-ended:

1. **Impress itself does not use threads.** Counted across non-test sources:
   `slideshow` 0, `drawinglayer` 0, `editeng` 0, `oox` 0, `xmloff` 0, `svl` 0,
   `basegfx` 0, `cppcanvas` 0, `emfio` 0, `tools` 0, `i18npool` 0. `sd` has 8,
   of which 7 are the `ENABLE_SDREMOTE` remote control and 1 is the Presenter
   Console's timer. Editing a slide, laying out text, importing a `.pptx` shape
   tree and running the animation engine never touch a thread. The threading
   work is at the infrastructure edges — file I/O, settings persistence, image
   decode — not in the code that draws.
2. **On wasip2 a missing thread aborts loudly instead of hanging.** Verified by
   running under wasmtime: `pthread_cond_wait` traps on `unreachable`;
   `std::condition_variable::wait_for` goes `__do_timed_wait` →
   `std::terminate()` → `abort()`, exit 134; `pthread_cond_timedwait` returns
   `ENOTSUP` (58) rather than `ETIMEDOUT` — that last one is what the thread
   shim now overrides (decision 12), so the *timed* waits no longer abort; the
   untimed ones still do, on purpose. That inverts the usual porting
   intuition — normally a missing thread means "deadlock, then bisect" — into
   "crash with a stack trace, patch the named site, repeat". Every reachable
   wait is findable in one run.

The honest residual: **nobody can bound the number of reachable waits by
reading.** The way to close that gap is M3 (headless `--convert-to`), which is
cheap and comes long before any GUI work.

---

## The strategy: a genuine WASI host triple, not Emscripten-with-a-different-libc

Same shape as the Qt decision, for the same reason, and here it is even more
clear-cut because LibreOffice's Emscripten support is *not* a fork of the
codebase — it is a `host_os` arm in `configure.ac`, a gbuild platform file, and
a scattering of `#ifdef EMSCRIPTEN`. Taking it wholesale would mean inheriting:

* an `EM_JS` JavaScript callback in the UNO C++ bridge
  (`bridges/source/cpp_uno/gcc3_wasm/cpp2uno.cxx:32`, `jsGetExportedSymbol`,
  which looks up a wasm export by name through the JS `Module` object) — there
  is no JS in a wasip2 component;
* an emcc flag wall (`-s FETCH=1`, `-s TOTAL_MEMORY=1GB`, `--bind`,
  `EXPORTED_RUNTIME_METHODS`, `-sPTHREAD_POOL_SIZE=7`) injected into
  `gb_LinkTarget_LDFLAGS` for every target;
* `soffice.data` + `qtloader.js` + `.worker.js` + embind post-js packaging,
  which the wk VFS and OCI layers replace outright;
* the whole threaded model above;
* and `emscripten_set_main_loop_arg` with `maAppData.m_bUseSystemLoop = true`,
  which **cancels every synchronous modal dialog** (see the traps).

So we add a `wasi*)` arm beside `emscripten)` and keep every Emscripten path
switched off. Where upstream's arm is right for us we copy it verbatim; where
it exists only because emcc is a Python wrapper around clang, we drop it — and
several of those probes pass *better* under wasi-sdk than under emcc.

**This has been probed end to end.** A throwaway ~40-line patch adding
(a) a `wasi*)` host_os arm mirroring `emscripten)`, (b) a `wasi*)` OS/CPUNAME
arm, (c) `WASI` on the `ENABLE_WASM_STRIP_*` gate and (d) the `BUILD_TYPE`
token, followed by `aclocal -I m4 -I m4/mac && autoconf -I .`, ran
`configure --host=wasm32-unknown-wasip2 --disable-gui --with-wasm-module=impress`
through 160+ checks and into the BUILD-platform sub-configure before dying on
the host's GNU Make 3.81. `configure.ac` was reverted afterwards; `src/` is
clean. **The `configure.ac` side of a new host triple is a one-day job and is
not where the risk lives.**

Things that probe *better* than Emscripten, from that run: `-ggdb2` supported;
`--gc-sections` **found** (so unlike `EMSCRIPTEN_INTEL_GCC.mk` we keep it);
`-Bsymbolic-functions` absent and handled gracefully; no `emconfigure` wrapper
needed (`autogen.sh:321` only prepends it for `--host=wasm*-emscripten`); no
`EMMAKEN_JUST_CONFIGURE` hack.

---

## Decisions already made (do not relitigate)

1. **The wk VCL backend, not LibreOffice's qt6 backend on our Qt.** Reasoning
   and switch conditions in the next section. This is the decision with the
   most at stake, so it gets its own heading.
2. **Not a `Library_vclplug_wk.mk` — compile the backend INTO `libvcl`,
   iOS/Android style.** `vcl/android/androidinst.cxx` and `vcl/ios/iosinst.cxx`
   are literally `class AndroidSalInstance : public SvpSalInstance` +
   `extern "C" SalInstance* create_SalInstance()`, added to
   `vcl/Library_vcl.mk` under an `ifeq ($(OS),ANDROID)` block, with
   `configure.ac` setting a bare `R="android"` and no `libo_ENABLE_VCLPLUG`
   call at all. Under `DISABLE_DYNLOADING`, `salplug.cxx:52` defines
   `STATIC_SAL_INSTANCE 1` and `:296` is just `return create_SalInstance();`
   (verified). **One `extern "C"` symbol is the entire integration surface.**
   Set `using_headless_plugin=no` in the WASI arm, or `salplug.cxx:286` hijacks
   the instance with `svp_create_SalInstance()` before ever reaching ours.
3. **`--enable-cairo-rgba`.** This kills the per-frame swizzle before it is
   ever written, and it is upstream's own supported option — not a patch.
   `include/vcl/CairoFormats.hxx:36-42` under `ENABLE_CAIRO_RGBA` gives
   `SVP_CAIRO_RED 0, GREEN 1, BLUE 2, ALPHA 3`, i.e. byte order `[r,g,b,a]` —
   exactly what `wkgfx_present` documents ("Pixels are RGBA8: bytes
   `[r, g, b, a]` in memory order") and blits without a copy when the
   dimensions match. It is real, not cosmetic: `external/cairo/
   UnpackedTarball_cairo.mk:37` applies `cairo.GL_RGBA.patch`, which remaps
   `CAIRO_FORMAT_ARGB32` onto `PIXMAN_a8b8g8r8` inside the bundled cairo
   (verified by reading both). Upstream's own rationale is Online's canvas
   `ImageData`, which is the same problem we have. This is the exact analogue
   of `plugins/netsurf` choosing `NSFB_FMT_XBGR8888` so that `update()` is a
   memcpy. **Correction to an earlier survey**, which concluded a swizzle was
   unavoidable because no cairo format matches: true of stock cairo, false of
   LibreOffice's.
4. **`--with-wasm-module=impress`.** Upstream supports it
   (`configure.ac:2262`, implemented at `:4324`): it clears
   `ENABLE_WASM_STRIP_ACCESSIBILITY` and
   `ENABLE_WASM_STRIP_BASIC_DRAW_MATH_IMPRESS`, which pulls in `animations`,
   `sd`, `sdext`, `slideshow` and `starmath` via `RepositoryModule_host.mk`,
   and drops `sw`/`swext` and `sc`/`scaddins`/`sccomp`/`basctl` entirely. It
   also packages the whole `simpress` UI-config tree. Dropping Writer and Calc
   is the single biggest lever on link time and module size.
5. **`--disable-mergelibs`.** The brief said upstream's wasm path uses
   `--enable-mergelibs`; **it does not** — `distro-configs/LibreOfficeWASM32.conf`
   is five lines and does not mention it, and the `emscripten)` arm never sets
   it. It buys nothing here (`DISABLE_DYNLOADING` already makes every Library a
   `.a`) and it actively breaks things: `native-code.py:21-24` hardcodes
   `libi18npoollo.a` and `libsvtlo.a` in its factory map, and mergelibs renames
   both to `libmergedlo.a` — 76 services (74 i18npool transliterations, the
   svt file/folder pickers) would fail at runtime with
   `CannotActivateFactoryException`, *partially and confusingly*, because
   constructor-based implementations ignore the uri and would keep working.
6. **`--enable-customtarget-components`, and never touch `native-code.py`'s
   hand-written group lists.** This is the Emscripten model, not the iOS/Android
   one. iOS and Android hand-maintain `-g core -g writer -g calc -g draw -g edit`
   lists; the Emscripten path derives the constructor map from the build
   (`postprocess/CustomTarget_components.mk` → `constructors.py` →
   `services_constructors.list` → `native-code.py -c` → `component_maps.cxx`).
   Cost, in upstream's own words (`static.mk:52-57`): everything is built and
   cleaned up at link time, "especially expensive for WASM". Constraint it
   imposes: `--with-locales` must be unset, `all`, or `en` (`configure.ac:3511`
   errors otherwise). Trim locales at the VFS layer, not at configure.
7. **Mount the image at `/instdir`, binary at `/instdir/program/soffice.wasm`.**
   This one choice makes `$ORIGIN`, `BRAND_BASE_DIR`, the stock `fundamentalrc`
   layout **and** fontconfig's compiled-in paths (`--with-baseconfigdir=
   /instdir/share/fontconfig`, `--with-cache-dir=…/cache`,
   `--with-add-fonts=/instdir/share/fonts`, from
   `external/fontconfig/ExternalProject_fontconfig.mk`'s EMSCRIPTEN arm)
   resolve with zero further changes. `/instdir` is not arbitrary: it is
   literally what `configure.ac:5987` sets `INSTDIR` to, and the Emscripten fs
   image stores paths relative to `$(BUILDDIR)`.
8. **wasip2 direct**, `wasm-component-ld` emitting the component at link time.
   Same as `plugins/netsurf` and `plugins/bun`. No preview1 adapter, no
   `wasm-tools component new` step.
9. **Keep Emscripten's `CPUNAME=INTEL` lie** (`RTL_ARCH=x86`,
   `PLATFORMID=linux_x86`), so the platform file is
   `solenv/gbuild/platform/WASI_INTEL_GCC.mk` and every already-exercised
   `-DINTEL` code path stays on the exercised branch. The naming is already
   loose upstream; deviating buys nothing and risks an unaudited `#ifdef`.
10. **`--disable-gui` is the staging point, not the destination**, and it is
    only reachable because the WASI arm copies Emscripten's `using_x11=yes`
    fib — `configure.ac:5949` makes `--disable-gui` itself require
    `using_x11 = yes`. Switch to the real single-VCL-plugin shape (decision 2)
    as early as M4; a `--disable-gui` build exercises a materially different
    module graph and you will re-debug configure when you switch.
11. **Java: off, and stop thinking about it.** `configure.ac:3494` force-sets
    `with_java=no` whenever `DISABLE_DYNLOADING=TRUE` on a non-Apple,
    non-Android, non-Windows target; `--disable-scripting` does it again
    independently. Every `loader="com.sun.star.loader.Java2"` component in the
    tree is a wizard, reportbuilder or scripting provider.
12. **The threading policy lives in one 200-line C file on the link line, not
    in patches.** `shim/wk-wasi-threads.c` → `shim/libwkwasithreads.a`, built by
    `build-shim.sh`, injected by `gb_WASI_SHIM` in `WASI_INTEL_GCC.mk`'s
    `gb_LinkTarget_LDFLAGS` with `--whole-archive`. It overrides exactly two
    libc functions, and the asymmetry between them is the decision:
    * `pthread_cond_timedwait` **sleeps for the requested time on
      `CLOCK_MONOTONIC` and returns `ETIMEDOUT`.** A wait that can only ever
      time out should say so rather than let libc++ abort. The sleep is not
      optional: returning instantly would turn `SvpSalInstance::ImplYield` into
      a 100%-CPU spin, so this is a correctness *and* a power decision.
    * `pthread_cond_wait` (untimed) **prints one sentence and aborts.** An
      unsignallable wait with no deadline is a deadlock, not a timeout. The
      three options were a spurious `return 0` (a silent livelock, because
      every caller uses the predicate form and loops), sleeping forever (an
      undebuggable hang), or crashing where the fault is. Failing loudly is the
      only one that names the bug. If a *reachable* untimed wait appears, give
      that call site a serial path; do not soften the shim.
    Not overridden, each deliberately: `pthread_create` (already a clean
    ENOTSUP), `pthread_join` (wasi-libc returns 0 without touching `*retval` —
    a lie, but unobservable, since `pthread_create` can never succeed so there
    is never a thread to join), and `pthread_barrier_wait` /
    `__wasilibc_futex_wait` (both trap exactly when they would block, which is
    right for the same reason the untimed wait is). A survey of all 847 members
    of `libc.a` found exactly three functions whose body is a bare
    `unreachable`: `abort`, `__stack_chk_fail_local` and `pthread_cond_wait`.
    There is no fourth landmine of this shape.
    **Why a link-line archive and not a patch:** it also covers libc++,
    libc++abi and the ~149 externals, which we never patch.
13. **Python: runtime off, host required.** `enable_python=no` comes free from
    the same block, but a *host* Python 3 is a hard build dependency
    (`native-code.py`, `constructors.py`, `com_GCC_class.mk`). **Pass
    `PYTHON=` explicitly** or `configure.ac:10689` may quietly build an
    internal CPython 3.12.14 for the build machine. The host's Python 3.14.7
    runs `native-code.py -g core -g draw` fine (verified: exit 0, 1447 lines,
    508 map entries).
14. **`osl`'s WASI gaps split two ways, and the size of the divergence decides
    which.** LibreOffice already splits `osl` by platform directory
    (`sal/osl/unx` vs `sal/osl/w32`), so both shapes are native to the tree.
    * **A small gap gets an in-file `#if defined(WASI)`** — `core-0006`. Ten
      files, none of them restructured: the `chown`/`lchown`/`getuid` calls in
      `file_misc.cxx` (policy: wk's vfs has no owners, so the call is dropped
      and the mode/timestamp half of the same function still runs), the
      synthetic `passwd` in `secimpl.hxx` + `security.cxx`, `tzset`/`timezone`
      in `time.cxx`, the `dlsym`/`/dev/urandom` pair in `random.cxx` replaced by
      `getentropy` (added at the `cppu` rung — decision 20), and roughly a dozen
      socket constants and errnos that wasi-libc keeps behind
      `__wasilibc_unmodified_upstream`.
    * **A whole-subsystem divergence gets its own file** — `core-0007`, three
      of them, selected by an `ifeq ($(OS),WASI)` in `Library_sal.mk`:
      `process_wasi.cxx` (fork/exec/waitpid/socketpair/kill: 1200 lines of
      primitives wasip2 does not have), `pipe_wasi.cxx` (an osl pipe is a named
      AF_UNIX socket, and wasi-libc's `struct sockaddr_un` has **no `sun_path`
      member at all** — the file cannot compile, never mind connect), and
      `signal_wasi.cxx` (450 lines of `sigaction`/`siginfo_t`/`sigset_t`
      replaced by two functions).
    **They live in `sal/osl/unx/` with a `_wasi` suffix, NOT in a new
    `sal/osl/wasi/`.** WASI reuses the other twenty-odd `unx` sources unchanged;
    a directory of its own would advertise a platform port that does not exist
    and would need copies of all of them to work.
    The test of a stub is what a caller does with the answer, not whether it
    compiles: `osl_getLastPipeError` returns `osl_Pipe_E_invalidError`
    specifically because `officeipcthread.cxx:739-772` **retries forever** on
    any other value, and `osl_joinProcessWithTimeout` deliberately does not
    return `osl_Process_E_TimedOut` for the same reason. Same principle as the
    thread shim's untimed wait: fail loudly at the fault, never livelock.
15. **`unxgcc.mk`'s `$(shell echo -n …)` breaks on a macOS build host** —
    `core-0008`, and it is not a wasm bug. The `DISABLE_DYNLOADING` branch of
    `gb_LinkTarget__command_dynamiclink` builds its `-l` list with
    `$(shell echo -n … | tee $@.linkdeps)`. The pipe forces make through
    `$(SHELL)` = `/bin/sh`, and macOS's `/bin/sh` has a POSIX `echo` builtin
    that prints `-n` **as a word**; the literal `-n` reaches the compiler as
    `clang++: error: unknown argument: '-n'`. It has never bitten anyone
    because iOS — the only other `DISABLE_DYNLOADING` target built on macOS —
    takes `macosx.mk` instead of this file. Fixed with `printf '%s '`, which is
    POSIX, identical on Linux, and emits nothing for an empty list. Verified by
    running: `/bin/sh -c 'echo -n foo'` prints `-n foo` on this machine, while
    a bare `$(shell echo -n a b)` in a test makefile does not — because without
    a pipe make execs `/bin/echo` directly, and *that* one honours `-n`.
16. **No `-pthread` on a WASI link line — NOR ON A COMPILE LINE, and the second
    half of that sentence was missing until the `unoidl` milestone.**
    `unxgcc.mk:48-53` sets `gb_CXX_LINKFLAGS := -pthread` whenever libc++ or
    libstdc++ is detected. On a normal Unix that is nearly free advice; on wasm32
    it is a target-feature switch — clang expands it to `-matomics
    -mbulk-memory` and asks lld for `--shared-memory`, and wasi-sdk's plain
    `wasm32-wasip2` sysroot has neither, so the link dies with `wasm-ld: error:
    --shared-memory is disallowed by Unwind-wasm.c.o`. Nothing is lost:
    `pthread_create` is an ENOTSUP stub on this target either way (decision 12).
    * **`gb_CXX_LINKFLAGS := ` does not remove it from the compile command**, and
      the original version of this decision assumed it did. `unxgcc.mk:55-60`
      folds `$(gb_CXX_LINKFLAGS)` into `gb_CXXFLAGS` with `:=`, and
      `unxgcc.mk:107` then does `gb_LinkTarget_CXXFLAGS := $(gb_CXXFLAGS)` —
      both immediate assignments, both evaluated *while unxgcc.mk is being
      included*, i.e. before a single line of `WASI_INTEL_GCC.mk` runs. Emptying
      the variable afterwards changes only the link. So **every C++ object this
      port has built so far carried `-pthread`**, and `make verbose=t` is the
      only thing that says so.
    * **The compile half does not fail loudly. It miscompiles exception
      handling.** `-pthread` turns on the atomics/bulk-memory target features,
      which switches thread-local storage to the real TLS model, so the wasm
      EH/SjLj landing-pad context `__wasm_lpad_context` is addressed TLS-relative
      (`R_WASM_MEMORY_ADDR_TLS_SLEB`) while wasi-sdk's libunwind defines it as an
      ordinary `.bss` global. With `-pthread` *also* on the link line lld refuses
      and names the symbol; with it on the compile line **only** — the state this
      port was in — lld quietly relaxes TLS to static addressing, links, and the
      landing pad reads far outside linear memory. **Every `catch` in
      LibreOffice's own code traps**, as `memory fault at wasm address 0x12f4678
      in linear memory of size 0x980000`, with no message and no useful frame.
    * `WASI_INTEL_GCC.mk` now filters `-pthread` out of **both** `gb_CXXFLAGS`
      and `gb_LinkTarget_CXXFLAGS`. Filtering only the first does nothing at all.
      `gb_CFLAGS` never had it (`PTHREAD_CFLAGS` is empty in `config_host.mk`),
      so C objects — `zlib` — were never affected.
    * **Why four clean module milestones did not catch this**: nothing had ever
      *executed* a `catch`. Archives compile and link with the bad relocation;
      only running a throw that unwinds into LibreOffice code shows it. That is
      the general lesson, not a detail about `-pthread`: a milestone that only
      builds an archive proves nothing about the archive's exception handling.
17. **`salhelper` needs no patch, and the reason it needs none is the shape to
    look for in every module above it.** It is the first module in this port to
    cross-compile completely untouched, and it is the first C++ *consumer* of
    `osl` conditions to reach the compiler, so it is where the thread shim's two
    halves become visible side by side in one archive. `llvm-nm
    --undefined-only` on `timer.o` shows **both**
    `std::condition_variable::__do_timed_wait` (the half the shim rescues) and
    `std::condition_variable::wait` (the half the shim deliberately aborts on).
    Neither is a landmine, and the distinction is the point:
    * **Both live inside `TimerManager::run()`** (`timer.cxx:426` and `:434`),
      which is the *body of an `osl::Thread`*. On wasip2 `create()` can never
      succeed, so `run()` is never entered and neither wait is reachable. **A
      wait on a thread that cannot be spawned is not a wait.** Do not patch
      these; changing a thread body to be shim-safe is work spent on code that
      cannot execute.
    * `~TimerManager` opens with `if (!isRunning()) return;` *before* its
      `notify_all()` and `join()`. Upstream wrote that for static-destruction
      ordering in unit tests; it happens to be exactly the arm a threadless
      build takes, which is why nothing hangs at exit. Luck, but load-bearing
      luck — check for it rather than assuming it, in every `~Manager` of this
      shape.
    * The runtime consequence, which is a **behaviour gap, not a build gap, and
      is deliberately left open**: with no `TimerManager` thread, a
      `salhelper::Timer` is registered and then **never fires**. Its only three
      callers are `connectivity`'s connection pool, `desktop/source/lib/init.cxx`
      (LibreOfficeKit — rejected, see the header) and `fpicker`'s `fileview`.
      **None is on the M4 headless-convert path**, so this bill comes due at
      M6–M8, not before.
    * The genuinely reachable untimed wait in this module is the *other* class:
      `salhelper::ConditionWaiter`'s one-argument constructor →
      `osl::Condition::wait()` → `osl_waitCondition(c, nullptr)` →
      `conditn.cxx:125`, which is the untimed `wait(g, predicate)`. Its only
      non-test caller is `unotools/source/ucbhelper/ucblockbytes.cxx`, **which
      is on the document-load path** — four untimed sites (`:512`, `:545`,
      `:562`, `:616`) beside one timed one (`:478`) that already catches
      `timedout`. That is the concrete list to check when M4 aborts in the shim,
      and per decision 12 the fix is a serial path at those call sites, never a
      softer shim.
    * Mitigating that: `wait(lock, pred)` is `while (!pred()) wait(lock);`, so
      an `osl` condition that is **already set** returns without ever entering
      `pthread_cond_wait`. The shim only fires on a wait that would genuinely
      block. Verified by reading `conditn.cxx:101-130`; the standard guarantees
      the predicate is tested first.

18. **`store` and `registry` need no patch either, but for a completely
    different reason from `salhelper`'s — and the reason is the one that
    matters most for this port.** These two libraries are what reads
    `services.rdb` and `types.rdb`, and **those files are written at build time
    by the native arm64 build-side tools and read at run time by the wasm32
    binary**. A word-size or endianness dependence in the format would not
    produce a compile error; it would produce a `CannotActivateFactoryException`
    at first service lookup, months later. So the thing to check here was never
    whether the module compiles — it was whether the *format* survives the
    crossing. It does, and it was measured rather than assumed:
    * **Every on-disk `store` page struct has identical size and alignment on
      both sides.** Dumped with `clang++ -Xclang -fdump-record-layouts` twice —
      once `--target=wasm32-wasip2 -I build/config_host`, once with the native
      `/usr/bin/clang++ -I build/config_build` — over `OStorePageGuard`,
      `OStorePageDescriptor`, `OStorePageLink`, `PageData`, `OStoreDataPageData`,
      `OStoreIndirectionPageData`, `OStorePageKey`, `OStorePageNameBlock`,
      `OStoreDirectoryDataBlock`, `OStoreDirectoryDataBlock::LinkTable`,
      `OStoreDirectoryPageData`, `OStoreBTreeEntry` and `OStoreBTreeNodeData`.
      `diff` of the two dumps: **identical**, down to `dsize` and the padding
      (`OStoreDirectoryPageData` is `sizeof=420, dsize=417` on both).
      **Use the two `config_*` directories, not one.** A first attempt compiled
      the native side against `build/config_host` and got `sizeof=16, align=8`
      for a struct of two `sal_uInt32` — a self-inflicted artefact, because
      `SAL_TYPES_SIZEOFLONG` is 4 in `config_host` and 8 in `config_build` and
      `sal/types.h:52` picks `long` for `sal_Int32` when it is 4.
    * That mismatch is worth understanding rather than working around, because
      it **confirms decision 9 from an angle decision 9 never claimed**. On
      wasm32 `SIZEOFLONG` is 4, so `sal_Int32` is `long`; on the arm64 build
      host it is `int`. Different underlying type, identical width, identical
      layout — and "`sal_Int32` is `long`" is exactly the branch **32-bit Intel
      Linux** takes. Keeping `CPUNAME=INTEL`/`RTL_ARCH=x86` does not merely
      avoid unaudited `#ifdef`s; the type system genuinely lands on the `i386`
      configuration, which upstream still builds. The only visible consequence
      is that `SAL_PRIuUINT32` expands to `"lu"` here and `"u"` there, which is
      why both compiles are warning-clean.
    * Endianness is a non-question and the code says so: `store/source/
      storbase.hxx:59` and `registry/source/reflread.cxx:471` are the tree's
      only two `OSL_BIGENDIAN` sites in these modules, and `include/osl/
      endian.h`'s WASI arm (`core-0004`) defines `OSL_LITENDIAN` because wasm is
      little-endian *by specification*, on every engine. `registry`'s
      `REGTYPE_IEEE_NATIVE` double punning then takes the same little-endian
      arm x86 takes.
    * `registry` has **no struct-layout dependence at all**: `reflcnst.hxx`
      addresses the blob through `readUINT16`/`readUINT32` at
      `sizeof(sal_uIntNN)`-derived byte offsets, never by overlaying a struct.
    * **`mmap` works on wasip2 — verified by running, not by linking.**
      `sys/mman.h` `#error`s unless `_WASI_EMULATED_MMAN` is set, and the
      implementation lives in `libwasi-emulated-mman.a`, which `core-0002`
      already puts on the link line. Its `mman.c.obj` undefines exactly
      `malloc`, `free`, `pread` and `errno` — i.e. **a file mapping is a
      `malloc` plus a `pread`, a copy and not a mapping**. Under wasmtime, both
      `MAP_SHARED` and `MAP_PRIVATE` with `PROT_READ` over a real file succeed
      and return the right bytes, a non-zero unaligned `offset` is honoured,
      and a length past EOF zero-fills. So `sal/osl/unx/file.cxx:1445`'s
      `MAP_SHARED` call — the one `osl_mapFile` is built on — is fine as-is.
    * The copy semantics are harmless **here** for a reason worth naming,
      because it will not hold everywhere: `MappedLockBytes` is constructed
      only inside `if (eAccessMode == storeAccessMode::ReadOnly)`
      (`lockbyte.cxx:851`), and its `writePageAt_Impl`/`writeAt_Impl`/
      `setSize_Impl` all return an error unconditionally. Nothing ever writes
      through a mapping, so "writes do not propagate" is unobservable. Any
      *other* consumer of `osl_mapFile` that expects shared-write semantics is
      a real bug on this target; there is no such consumer in these modules.
    * And even that is belt-and-braces: `FileLockBytes_createInstance` treats a
      failed mapping as an ordinary condition — `if (!rxLockBytes.is())` falls
      through to a plain `FileLockBytes` on `osl_readFileAt`. **Two independent
      reasons the `.rdb` read path works**, which is why no `osl_mapFile` stub
      was written.
    * **A trap that is currently disarmed, and must stay that way:
      `fcntl(fd, F_SETLK, &flock)` returns `-1`/`ENOTSUP` (58) on wasip2**
      (verified under wasmtime). That is the non-macOS branch of
      `osl_openFile`'s locking block (`file.cxx:1262-1279`), and it does not
      degrade — it `close()`s the fd and returns the error, so **every
      write-mode `osl_openFile` in the whole product would fail**. It never
      fires only because `osl_file_queryLocking` (`file.cxx:814-829`) returns
      false unless `getenv("SAL_ENABLE_FILE_LOCKING")` is set, and
      `HAVE_O_EXLOCK` is defined nowhere in the tree so the `getenv` branch is
      the live one. **Never set `SAL_ENABLE_FILE_LOCKING` in the wk node's
      environment.** This belongs in the runtime image's notes, not in a patch:
      the correct WASI behaviour for a single-process node is exactly "no file
      locking", and upstream already spells that as an unset variable.
    * Neither module contains a single `osl::Condition`, `osl::Thread`,
      `salhelper::Thread` or `std::condition_variable` — `grep` over both
      `source/` trees returns nothing. All they use is `osl::Mutex`, which is a
      working `pthread_mutex` here. **The thread shim is not involved at all**,
      which is what makes decision 17's reachability rule unnecessary rather
      than merely satisfied.
    * Undefined-symbol audit of the two archives, as the cheap check that no
      unsupported subsystem is reached: `libstorelo.a` wants sixteen `osl_*`
      symbols, all file, mutex and URL; `libreglo.a` wants six plus `unlink`.
      **Nothing from `process_wasi.cxx`, `pipe_wasi.cxx` or `signal_wasi.cxx`.**
      The one that looked like an exception is not: `osl_getProcessWorkingDir`
      lives in the *unmodified* `process_impl.cxx`, not in the replaced
      `process.cxx`, and it is `getcwd` + a URL conversion. Under wasmtime
      `getcwd` returns `"/"`, so `store`'s relative-path branch
      (`lockbyte.cxx:228`) resolves to `file:///…` and returns
      `osl_Process_E_None` — a truthful answer for a node whose vfs root is `/`.
    * `regview` and `registry_helper` are **not built**, and that is
      `Module_registry.mk` doing it, not us: `gb_CondExeRegistryTools`
      (`Conditions.mk:19`) takes the empty arm whenever `DISABLE_DYNLOADING` is
      set. Confirmed by `grep regview` over the build log returning nothing.
      Which means **this port still has not linked a single executable** — E1
      and the `--start-group` question remain entirely untouched.

19. **`unoidl` needs no source patch either, and the milestone's real content is
    that it is the first LibreOffice code this port has RUN.** The module builds
    one library and nothing else: `Module_unoidl.mk` gates
    `Executable_unoidl-check` on `$(call gb_not,$(CROSS_COMPILING))` and both
    `unoidl-read`/`unoidl-write` on `ODK` in `BUILD_TYPE`, and this configuration
    has `CROSS_COMPILING=TRUE` and no `ODK` — so the three executables are
    build-side only, exactly like `registry`'s `regview`.
    * **The new-format `.rdb` crosses arm64 → wasm32 by construction, not by
      luck, and this was checked by running it rather than by reading.** Unlike
      `store`'s page structs (decision 18, which had to be *measured*),
      `unoidlprovider.cxx`'s `Memory16`/`Memory32`/`Memory64` are `unsigned char
      byte[N]` with explicit little-endian shift-and-or accessors, and
      `unoidl-write.cxx`'s `write16`/`write32`/`write64` are the mirror image —
      byte-at-a-time into a `char` buffer. There is no struct overlay anywhere in
      the format. The only endianness sites are the `float`/`double` union
      punnings (`unoidlprovider.cxx:113,148` under `OSL_LITENDIAN`,
      `unoidl-write.cxx:181,194` under `OSL_BIGENDIAN`), and both sides here are
      little-endian.
      **Verified end to end**: three `.idl` files → the native arm64
      `unoidl-write` → a 347-byte `.rdb` → a `wasm32-wasip2` binary linked
      against `libunoidllo.a` and run under wasmtime, which read back the struct
      member list, `MaxSlides = 305419896` (`long`), `HugeValue =
      1311768467463790320` (`hyper`, i.e. a 64-bit value on a 32-bit target),
      `Ratio = 0.15625` (`double`) and the three enumerators. **This is the first
      LibreOffice code in this port to execute at all.**
    * **And executing it is what found the `-pthread` miscompile** (decision 16).
      The trigger is `Manager::loadProvider` (`unoidl.cxx`), which catches
      `FileFormatException` from `UnoidlProvider` and retries the file as legacy
      `registry` format. Passing a *relative* URI makes `osl_openFile` fail with
      21, which takes that catch — and the catch trapped. Four modules of clean
      archives had proved nothing about it. Bisected by recompiling a verbatim
      copy of `loadProvider` in a separate TU with and without `-pthread` against
      the same archives: without it the fallback runs and reports `cannot open
      legacy file: 6`; with it, the same out-of-bounds fault.
    * **`osl_mapFile` is load-bearing here in a way it was not in `store`.**
      `MappedFile`'s constructor (`unoidlprovider.cxx:262-283`) throws
      `FileFormatException("cannot mmap: …")` when the mapping fails —
      there is no `osl_readFileAt` fallback, unlike
      `FileLockBytes_createInstance`. So decision 18's "mmap works on wasip2, as
      a `malloc`+`pread` copy" is the only thing keeping the `.rdb` path alive,
      and the copy semantics mean `types.rdb` costs its full size in RSS at open
      time. Worth remembering for E2.
    * `osl_File_MapFlag_RandomAccess` — which is what `MappedFile` passes — runs
      `file.cxx:1451-1474`'s page-touch loop, `while (nSize > nPageSize)` over
      `FileHandle_Impl::getpagesize()`. A zero page size would spin forever.
      **`sysconf(_SC_PAGESIZE)` returns 65536 under wasmtime** (verified), the
      wasm page size, so the loop terminates. The same call sizes
      `FileHandle_Impl`'s read buffer (`file.cxx:214-219`), so every buffered
      `osl` file handle on this target costs a 64 KiB `calloc` rather than the
      4 KiB it costs on Linux — 16× per open file, and LibreOffice opens a lot.
    * The `dirent.h` block in `sourcetreeprovider.cxx:28-88` is `#if defined
      MACOSX || defined LINUX`, and `gb_OSDEFS` gives us `-DWASI`, so WASI takes
      the `#else` — `return status.getFileName()`, the same arm Windows takes.
      That is upstream's own fallback for "cannot recover the original spelling
      of a filename", not a stub of ours, and `readdir_r` never has to exist.
    * Undefined-symbol audit: `libunoidllo.a` wants fifteen `osl_*` symbols (file,
      directory, mutex, thread-text-encoding) plus `initRegistry_Api`. Nothing
      from `process_wasi.cxx`, `pipe_wasi.cxx` or `signal_wasi.cxx`; no
      `osl::Condition`, `osl::Thread` or `std::condition_variable` anywhere in
      the module, so the thread shim is again uninvolved. The `exit`, `stdin` and
      `fprintf` references come from the generated flex scanner
      (`yy_fatal_error`), which is on the `.idl`-source path, not the `.rdb` one.
    * `unoidl` is also the first module whose **wasm** compiles emit warnings —
      five of them, all in generated `bison`/`flex` output
      (`workdir/YaccTarget/…/sourceprovider-parser.cxx`,
      `workdir/LexTarget/…/sourceprovider-scanner.cxx`). Each has an identical
      twin from the `workdir_for_build` native compile of the same generated
      file, so they are `bison`/`flex` codegen artefacts and say nothing about
      the target.
20. **`cppu` compiles untouched, and the milestone is entirely about what it
    *reaches*: `dlsym`, `getentropy` and a hard `abort()` in the thread pool.**
    `./build-lo.sh cppu.allbuild` produced five wasm archives with zero `error:`
    lines on the first attempt and no source patch. That is the fourth module in
    a row to do so and it means nothing on its own — what this rung actually
    established is below, and none of it was visible from the archive.
    * **`cppu` is where `dlsym` first becomes reachable, and where a "polite"
      stub would have aborted LibreOffice at UNO bootstrap.** The chain is
      `lbenv.cxx`'s `rtl_getGlobalProcessId()` → `rtl_createUuid` →
      `rtl_random_getBytes` → `osl_get_system_random_data`, and
      `sal/osl/unx/random.cxx:25` opens with `dlsym(RTLD_DEFAULT,
      "lok_open_urandom")`. wasi-libc **declares** `dlsym` in `<dlfcn.h>` and
      defines it nowhere, so this is a *link* error — `wasm-ld: error:
      libuno_sal.a(random.o): undefined symbol: dlsym` — the first time anything
      pulls `random.o` in. `sal`, `salhelper`, `store`, `registry` and `unoidl`
      never did; `cppu` does, on the very first `uno_getEnvironment`.
    * **The fix is not allowed to be a stub, and this is the clearest example so
      far of decision 14's "the test of a stub is what a caller does with the
      answer".** `rtl/random.cxx:73-77` turns a `false` return into
      `fprintf(stderr, "cannot read random device, aborting")` + `abort()`.
      There is no degraded path. And the two things the Unix implementation
      needs are both genuinely absent: `dlsym`, and `/dev/urandom` (verified —
      `fopen("/dev/urandom")` returns `NULL` under wasmtime; a wasm component's
      filesystem is its preopens and has no device nodes). The symbol being
      looked up, `lok_open_urandom`, is a LibreOfficeKit hook, and LOK is
      rejected by this port outright, so nothing is lost by never looking.
    * **`getentropy()` is the real primitive here, not a stand-in** — wasi-libc
      implements it directly on the wasi `random_get` import, which wasmtime
      backs with the host CSPRNG. Its one wrinkle is a hard **256-byte cap per
      call** (`getentropy(300)` fails with `EIO`/29 — verified by running),
      hence the chunking loop. `core-0006` gains a tenth file for this; it is
      the same in-file `#if defined(WASI)` shape as the other nine.
    * **`cppu`'s thread pool is dead code in this port, and its landmine is an
      unconditional `std::abort()`, not a wait.** `ORequestThread::launch()`
      (`thread.cxx:126-128`) is literally `if (!create()) { std::abort(); }`, and
      `osl::Thread::create()` can never succeed on wasip2. Verified by running:
      `uno_threadpool_create()` + `uno_threadpool_destroy()` is **clean** (the
      constructor spawns nothing, and `joinWorkers()` over an empty deque
      returns at once — the same load-bearing luck as `~TimerManager` in
      decision 17), while one `uno_threadpool_putJob` dies at
      `abort` ← `ORequestThread::launch()` ← `ThreadPool::createThread` ←
      `ThreadPool::addJob` ← `uno_threadpool_putJob`, exit 134, every frame
      named. Reachability: the only callers of `uno_threadpool_*` outside `cppu`
      are `binaryurp` (remote UNO over a pipe — and `osl` pipes are unsupported,
      decision 14) and `bridges/source/jni_uno` (Java, off by decision 11).
      **Do not patch it.** The two waits in `jobqueue.cxx:72` (untimed, aborts)
      and `threadpool.cxx:121` (timed, the shim returns `ETIMEDOUT`) sit behind
      the same wall.
    * What was actually run, and passed, linked against `libuno_cppu.a` +
      `libuno_salhelpergcc3.a` + `libuno_sal.a` + `libzlib.a` + the shim: the
      static types (`long`, `string`, `com.sun.star.uno.XInterface`);
      `XInterface`'s type description completing with **3 members from
      `static_types.cxx` and no `.rdb` at all**, which is the bootstrap that
      makes a registry-less process possible; `Any` round-tripping long, string,
      hyper (a 64-bit value on a 32-bit target) and double, plus copy-compare;
      `Sequence` copy-on-write, `realloc` and `Any` round-trip; `uno_getEnvironment("uno")`
      creating the built-in binary UNO environment (which is what proves the
      `getentropy` arm, since that call is what reaches `rtl_createUuid`);
      `uno_getMapping(env, env)` giving the identity mapping from
      `IdentityMapping.cxx`; and a `css::uno::RuntimeException` thrown and
      **caught by its base class `css::uno::Exception`** with its `Message`
      intact — a cppumaker-generated udkapi type unwinding through real RTTI,
      which is the post-decision-16 regression check.
    * **The C++/UNO bridge half is NOT covered by any of that**, and cannot be:
      see the blocked row in Current state. The probe supplies its own stubs for
      the two symbols `cppu` imports from it under `DISABLE_DYNLOADING` —
      `CPPU_ENV_uno_ext_getMapping` (`lbmap.cxx:332`, via `selectMapFunc`) and
      `CPPU_ENV_uno_initEnvironment` (`lbenv.cxx:1001`, via `loadEnv`). Those
      two undefined symbols are the *whole* of `cppu`'s dependency on
      `bridges`; there is no makefile edge between the modules, only a link-time
      one, which is why `cppu.allbuild` succeeds while nothing can yet use it.

## wk VCL backend vs. LO's qt6 backend on our Qt — recommendation

**Recommendation: the wk VCL backend. The qt6 route is a strict superset of its
work, not a shortcut.** Four findings, each verified by reading the source (and
one by running a symbol dump), kill it:

1. **It does not avoid the externals.** `QtInstance`'s member initialiser is
   `m_bUseCairo(nullptr == getenv("SAL_VCL_QT_USE_QFONT"))` — *cairo is the
   default*. `QtFrame::AcquireGraphics` creates a `QtSvpGraphics`, which is
   `final : public SvpSalGraphics`, over its own
   `cairo_image_surface_create(CAIRO_FORMAT_ARGB32, …)` with the same
   `DamageHandler`. `Library_vclplug_qt6.mk` lists `cairo graphite harfbuzz
   icu_headers icuuc epoxy qt6`. So the qt6 route needs cairo + pixman +
   freetype + fontconfig + harfbuzz + graphite + icu cross-built **anyway**,
   and stacks Qt on top.
2. **It does not compile against this repo's Qt.** `vcl/inc/qt5/
   QtAccessibleWidget.hxx:25-30` unconditionally includes six `<QtGui/
   QAccessible*>` headers, and `Library_vclplug_qt6.mk` lists four accessibility
   objects unconditionally. **Verified by running:** `llvm-nm --defined-only
   plugins/qt/sysroot/lib/libQt6Gui.a | grep -c QAccessible` → **0**, because
   `plugins/qt/build-qtbase.sh:432` sets `-DFEATURE_accessibility=OFF`
   ("accessibility's bridge is AT-SPI over D-Bus, which we do not have"). Fix
   is a fresh multi-hour qtbase build or patching four LO source files plus a
   makefile.
3. **Upstream's Qt6 wasm arm is bound to Qt's Emscripten QPA.**
   `configure.ac:14113-14120` hard-errors unless it finds `libqwasm.a` **and**
   `wasm_shell.html`; `:14125` appends `-lqwasm -sGL_ENABLE_GET_PROC_ADDRESS`
   (an emcc-only flag); `QtInstance.cxx:982` does
   `Q_INIT_RESOURCE(wasmfonts)`/`(wasmwindow)` to keep libqwasm's resources
   alive in a static link. Ours is `libqwk.a`. *(Correction to an earlier
   survey, which reported upstream's wasm path as Qt5-only: a Qt6 arm does
   exist — it is Emscripten-QPA-bound, not Qt5-bound.)*
4. **It needs the same new `configure.ac` host arm anyway**, plus a rewritten
   Qt6 probe, plus the accessibility fix, plus driving `vcl/qt6` through wasm
   for the first time in history — inheriting two ports' unknowns instead of
   one. And `vcl/qt6` is 85 one-line `#include "../qt5/QtX.cxx"` shims over a
   codebase full of `RunInMainThread`, `QThread::currentThread()` comparisons
   and `emscripten_proxy_promise` calls.

**What would make me switch.** Any one of these, discovered at M4 or M5:

* the guest-side compositor turns out to be substantially harder than Qt's
  `QFbScreen` was — VCL ships no frame compositor at all and its only frame
  collection, `SalUserEventList::m_aFrames`, is an `o3tl::sorted_vector<
  SalFrame*>` sorted by **pointer value**, carrying no z-order. If z-ordering
  and hit-testing menus/tooltips/dialogs correctly eats more than ~2 weeks,
  Qt's already-working `QFbScreen` becomes worth its price;
* VCL's own widget drawing turns out to be unacceptable to look at. Note
  `SalGraphics::initWidgetDrawBackends` only installs a widget-draw backend if
  `VCL_DRAW_WIDGETS_FROM_FILE` is set, so by default you get VCL's `decoview`
  drawing themed from `vcl/uiconfig/theme_definitions/*/definition.xml` — a
  data file, not code, so retheming is cheap and this is unlikely to bind;
* `SvpSalFrame`'s private `m_pSurface` turns out not to be re-decoratable from
  a subclass after a resize without an upstream patch (see Known gaps). That
  would be a two-line upstream patch, not a switch — listed here only because
  it is the one place the "subclass svp" plan touches private state.

Cost asymmetry to keep in mind: `plugins/qt/build-qpa.sh` rebuilds the Qt
backend in **20 seconds** because Qt installs a findable CMake package. gbuild
has no equivalent, so `vcl/wk/` lives in-tree and every backend edit is an
incremental gbuild run through `vcl` plus a relink of a very large executable.
That is a real, daily tax on the recommended route — and it is still cheaper
than the four items above.

---

## Traps this port already accounts for

* **Applying `patches/` re-triggers configure, and a bare `build-lo.sh` cannot
  survive it.** `build/Makefile:47-61` makes `config_host.mk` depend on
  `$(SRCDIR)/configure.ac`, and `lo_apply_patches` rewrites `configure.ac` —
  same bytes, new mtime — every time a session starts from a pristine `src/`.
  Make then re-runs `autogen.sh` **from inside `build-lo.sh`**, whose `PATH`
  deliberately excludes `.hosttools`, so the BUILD-side sub-configure dies for
  want of GNU Make and gperf. That failure is not harmless: it **deletes
  `build/config_build.mk`**, and the next `make` stops with `No rule to make
  target '…/config_build.mk'`. Recovering costs a full `./build-configure.sh`
  (~4 min). Two ways out, in order of preference:
  1. run `./build-configure.sh` first in any session that applies patches — it
     has the right `PATH` and is idempotent; or
  2. if the patch content is genuinely unchanged from what the tree was
     configured with, `touch build/config_host.mk` before `./build-lo.sh`.
     This is a timestamp fix for a timestamp problem, and nothing else. Do not
     reach for it after editing `configure.ac` for real.
* **Same as Qt: the EH trap.** wasmtime runs with `wasm_exceptions` (exnref)
  and *rejects* wasi-sdk's default legacy encoding at instantiate time, so one
  bad translation unit poisons the component with an error pointing nowhere.
  Every object gets `-fwasm-exceptions -mllvm -wasm-enable-sjlj -mllvm
  -wasm-use-legacy-eh=false`, links with `-lunwind -lsetjmp`, no LTO.
  **`-fwasm-exceptions` also selects wasi-sdk 34's `eh/` variant of
  libc++/libc++abi** — without it you silently get `noeh/`.
  LibreOffice funnels this through one variable
  (`gb_LinkTarget_EXCEPTIONFLAGS`, `EMSCRIPTEN_INTEL_GCC.mk:48`), so LO's own
  code is easy. **The externals are not**: `grep -rn gb_EMSCRIPTEN_EXCEPT`
  matches only the platform makefile — the EH flag reaches LO's gbuild targets
  and the link line and **nothing under `external/`**. Define the WASI
  equivalent of `gb_EMSCRIPTEN_CPPFLAGS` to *include* the EH flags so every
  external arm that already injects it inherits them. Otherwise icu, boost,
  harfbuzz and liborcus compile against `noeh` and the first `try` produces a
  component wasmtime refuses.
* **Same as Qt: the wasm-opt trap.** clang runs `wasm-opt` as an optional
  post-link pass and the one on PATH cannot parse exnref. Scrub PATH for the
  **build** step, not only configure.
* **Same as Qt: the stack trap.** Link with `-Wl,-z,stack-size=8388608`. LO's
  layout and import code recurse at least as deeply as Qt's raster engine, and
  the default 64 KB shadow stack presents as a mystery trap, not a message.
* **Same as Qt: the pthread stub trap**, with a wasip2 twist. wasi-libc
  *defines* `pthread_create` as an `ENOTSUP` stub, and libc++'s
  `__config_site` sets `_LIBCPP_HAS_THREADS 1` for wasip2 — so everything
  compiles, links, and **aborts at runtime**. Verified by running:
  `pthread_create` → 58; `std::thread` ctor throws
  `"thread constructor failed: Not supported"`; `hardware_concurrency()` → 1.
  Locking is *fine* — plain and recursive mutexes, `pthread_key` TLS,
  `pthread_self`, `sched_yield`, `sleep_for` all returned 0/worked — so
  `SolarMutex` (an `osl::Mutex`, `PTHREAD_MUTEX_RECURSIVE` at
  `sal/osl/unx/mutex.cxx:55`) is **not** a blocker. The problem is never
  locking; it is waiting and spawning.
* **The `--start-group` trap. Verified by running the linker.**
  `solenv/gbuild/platform/unxgcc.mk:156,163,165,169` emits
  `-Wl,--start-group … -Wl,--end-group` on every executable link in the
  `DISABLE_DYNLOADING` branch — the branch a wasm build takes. Both linkers
  reject it: `wasm-component-ld` → `error: unexpected argument '--start-group'
  found … a similar argument exists: '--start-lib'`; `wasm-ld` → `unknown
  argument: --start-group`. emcc silently swallows them, which is why upstream
  never hit this. Everything else gbuild emits is fine: `--gc-sections` ✓,
  `--whole-archive`/`--no-whole-archive` ✓, and `--no-as-needed` (rejected)
  only appears in the dynamic branch we never take.
* **`std::condition_variable` is a kill switch, and `SvpSalInstance::ImplYield`
  sits on it.** `svpinst.cxx:495` is `m_WakeUpMainCond.wait(g, …)` and `:503`
  is `wait_for(g, milliseconds(nTimeoutMS), …)`. Verified by running both
  shapes under wasmtime: the timed one aborts with
  `condition_variable timed_wait failed` (error 58), the untimed one traps on
  `unreachable` inside `pthread_cond_wait`. **The process aborts within
  milliseconds of `InitVCL` unless `DoYield` is overridden.** This is a hard
  prerequisite, not a performance item. Good news beside it: svp's
  *yield mutex* is safe — `doAcquire` takes the main-thread branch and `break`s
  on `tryToAcquire()` success, only reaching the condvar when another thread
  holds the mutex, which cannot happen.
  **The timed half of this is now closed by `shim/wk-wasi-threads.c`** — see
  decision 12 — so `wait_for` sleeps and returns `cv_status::timeout`
  everywhere, including in `osl_waitCondition`
  (`sal/osl/unx/conditn.cxx:119`, which is the same `wait_for(…, predicate)`
  shape and is reached long before VCL). The untimed half is deliberately still
  a crash. **`DoYield` must still be overridden**: the shim makes the timed wait
  survivable, it does not make svp's wake-up condition wake up.
* **The root cause under that abort is a clock, not a thread.** wasi-libc's
  `pthread_cond_timedwait` is musl's, and it waits by calling
  `clock_nanosleep(CLOCK_REALTIME, TIMER_ABSTIME, …)`. Verified by running:
  on wasip2 every `CLOCK_REALTIME` sleep — absolute or relative — returns
  ENOTSUP (58) in 0.0 ms, while every `CLOCK_MONOTONIC` sleep works and takes
  the time asked for. That is where the 58 comes from, and it is why the shim
  can honour the caller's timeout instead of returning instantly.
* **Do NOT copy `svpinst.cxx`'s `#if defined EMSCRIPTEN` block.** It sets
  `maAppData.m_bUseSystemLoop = true` and overrides `DoExecute` to call
  `emscripten_set_main_loop_arg`, because Emscripten must unwind the stack
  through a JS exception. With that flag set, `Application::Yield()` and
  `Reschedule()` **`std::abort()`** (`svapp.cxx:394-403, 494-502`), and
  `dialog.cxx:951-958` reads: *"As long as Application::Yield deliberately
  calls std::abort … better cancel them here for now as a hack"* — i.e.
  **upstream's wasm build silently cancels every synchronous modal dialog**.
  Impress is full of them. wasip2 can block (`wasi:io/poll`,
  `wkgfx_wait_frame_timeout(int64_t)`), so we leave `m_bUseSystemLoop` false,
  override only `DoYield`, and get working modal dialogs — a capability
  upstream's wasm build does not have. `plugins/qt/PORTING.md` records the same
  win for nested `exec()`.
* **`pthread_atfork` does not exist** in wasi-sdk 34's wasm32-wasip2 libc, and
  `svpinst.cxx:101` calls it. It is already `#ifdef`-ed out for
  EMSCRIPTEN/ANDROID/IOS; add WASI.
* **`<pwd.h>` does not exist and `getuid()` is undeclared. Verified by
  compiling. CLOSED in `core-0006`** — recorded here because the *shape* of the
  fix is not what this document originally proposed. `sal/osl/unx/secimpl.hxx`
  includes `<pwd.h>` and embeds a `struct passwd`; `security.cxx:334` calls
  `getuid()`. This is a **build break in `sal`**, the bottom-most module — not
  a runtime inconvenience.
  The plan was to copy the ANDROID arm (`security.cxx:304-317`, `HOME` out of
  `rtl::Bootstrap`). **The EMSCRIPTEN arm at `:131-160` was copied instead**,
  because the ANDROID arm does not actually solve the build break: it only
  changes where `osl_psz_getHomeDir` looks, and `oslSecurityImpl` still embeds
  a `struct passwd` that no header declares. So `secimpl.hxx` declares the
  record itself under `#if defined(WASI)` (seven members, only the ones osl
  reads), and `osl_getCurrentSecurity` fills it with one synthetic user —
  `wk`, uid/gid 1000, home `/root`, no loop and no `getpwuid_r`. `getuid()` is
  gone rather than stubbed: the one place that compared it against `pw_uid` is
  asking "is this the current user?", which has exactly one answer here.
  The consequence the original bullet named still holds and is now load-bearing
  in a second way: **never write `UserInstallation=$SYSUSERHOME`**, pass a
  literal path. `$HOME` still wins over `pw_dir`; `pw_dir` is `/root` to agree
  with the runtime image's `ENV HOME=/root` for the case where it does not.
* **Three unguarded `dlsym`/`dlopen` calls. Verified by compiling and linking:**
  the header exists at `wasi-sysroot/include/wasm32-wasip2/dlfcn.h` but
  `wasm-ld: error: undefined symbol: dlsym` / `dlopen`. They are
  `sal/osl/unx/random.cxx:24` (the `lok_open_urandom` probe),
  `drawinglayer/source/processor2d/cairopixelprocessor2d.cxx:76`
  (a `cairo_set_hairline` probe — with a static cairo you know the answer at
  compile time), and `sal/osl/unx/process_impl.cxx:132` (`dlsym(RTLD_DEFAULT,
  "main")`, reachable unless you extend the `#ifdef EMSCRIPTEN` at `:98`).
  The last is load-bearing beyond linking: it *is* `bootstrap_getExecutableFile`,
  which drives `$ORIGIN`/`BRAND_BASE_DIR` and therefore where `services.rdb` is
  found. Everything *else* is already guarded — `usable_dlapi=no` compiles
  `sal/osl/unx/module.cxx`'s dlopen out entirely and every significant caller
  in `cppu`, `i18npool` and `vcl` has a `DISABLE_DYNLOADING` arm.
* **The `VclBuilder` dead branch.** `vcl/source/window/builder.cxx:1385`
  selects the generated `lo_get_custom_widget_func()` only for
  `!HAVE_FEATURE_DESKTOP || (EMSCRIPTEN && !ENABLE_QT5)`; a WASI GUI build has
  `HAVE_FEATURE_DESKTOP` set (`configure.ac:3388` defines it for everything but
  iOS/Android/fuzzers) and falls into `osl_getFunctionSymbol(RTLD_DEFAULT)`,
  which returns nullptr with dlapi off. `component_maps.cxx` *does* generate
  the function. One-line fix (key on `DISABLE_DYNLOADING`), nasty to find
  later: the symptom is Impress dialogs and the notebookbar silently missing
  custom widgets, with only a `SAL_WARN`.
* **`cppu::getUnoIniUri()`** (`cppuhelper/source/paths.cxx:54`) has arms for
  ANDROID and EMSCRIPTEN and otherwise calls `get_this_libpath()` → `dladdr` →
  `DeploymentException`. Add a WASI arm returning
  `file:///instdir/program`.
* **The `-Wl,--start-group` removal is mechanical; its *semantics* are not.**
  See "Where this port is most likely to die".
* **`gb_CAN_EXECUTE_HOST_CODE` is true for Emscripten** (node runs the `.js`)
  and must be false for us. `gbuild.mk:167-173`'s `else` branch gives that for
  free, and `post_SpeedUpTargets.mk` then sets
  `gb_Module_SKIPTARGETS := check coverage slowcheck screenshot subsequentcheck
  uicheck`. That is one line to flip the day you want cppunit under wasmtime.
* **The component-type object trap** (same as every wk GUI port): each WIT
  world's `*_component_type.o` must be on the link line as an **object**, never
  as an archive member, or the linker drops it and the component silently
  loses that import. In gbuild the injection point is `gb_LinkTarget_LDFLAGS`
  in the new platform file, the same place Emscripten appends its emcc flags.
* **`nasm` is NOT needed**, contrary to the brief. `configure.ac:10049` guards
  the check on an x86 `host_cpu`; both the arm64 bootstrap and the wasm32
  target fall outside it, and the failure mode is `AC_MSG_WARN` anyway. Do not
  spend time on it.
* **`aclocal` first, always.** Bare `autoconf` emits a 1,151,105-byte
  `configure` that *looks* fine and dies at runtime with
  `syntax error near unexpected token 'android-editing,'` because the LO-local
  `m4/*.m4` macros were never expanded. `aclocal -I m4 -I m4/mac && autoconf
  -I .` (what `autogen.sh` does, with the `m4/mac` include being
  Darwin-conditional) produces a correct 1,712,088-byte one. Verified both
  ways. **Always go through `autogen.sh`.**
* **`configure.ac:6201` does `cp configure CONF-FOR-BUILD`** — from the *build*
  directory, not `$SRC_ROOT`. Invoking `$SRC/configure` from an empty build dir
  fails with `cp: configure: No such file or directory`. Verified. Every
  `build-*.sh` must call `$SRCDIR/autogen.sh` from inside the build directory.
* **wasi-sdk's clang shadows Apple clang on this shell's PATH**
  (`which -a clang` → wasi-sdk first), and `configure.ac:6206` **unsets `CC`**
  before the BUILD sub-configure, which then autodetects from PATH. Set
  `CC_FOR_BUILD=/usr/bin/clang CXX_FOR_BUILD=/usr/bin/clang++`
  (`configure.ac:6209-6216` consumes them; verified working — the sub-configure
  then reported "Apple Clang / Xcode 26.3"). Cheap to fix, expensive to
  discover late.
* **`--with-build-platform-configure-options` is appended LAST** on the
  sub-configure line, so it wins every conflict — but it takes configure
  *arguments*, not environment. Use it for `--enable-ccache`; do **not** try to
  set the native compiler through it.
* **`llvm-readelf` does not exist in wasi-sdk 34** and macOS has no `readelf`,
  so configure's `READELF` probe comes up empty. Harmless — its only gbuild
  consumer is the SONAME step, unreachable under `DISABLE_DYNLOADING` — but
  pass `AR`/`NM`/`RANLIB`/`STRIP` explicitly from wasi-sdk so the
  `AC_CHECK_TOOLS` fallbacks do not pick up Apple's, which cannot read wasm
  archives.

---

## Milestone ladder

Each rung is something you can **observe**: a file that exists, a command that
exits 0, a test that passes, an image you can look at. No rung is "a phase of
effort".

**E — experiments, before any of it.** See the next section; they are hours,
not days, and two of them can kill the port.

**M0 — `configure` completes.** `autogen.sh --host=wasm32-unknown-wasip2
--disable-gui --with-wasm-module=impress --disable-mergelibs
--enable-customtarget-components --enable-cairo-rgba` exits 0 and writes
`config_host.mk` with `OS=WASI`, plus `config_build.mk` naming Apple clang.
*Observable:* `grep '^export OS=' config_host.mk` → `WASI`.
*Contains:* the `configure.ac` patch (already probed), `WASI_INTEL_GCC.mk`,
`gmake`/`gperf` on the host.

**M1 — the native bootstrap builds.** `make cross-toolset` produces
`workdir_for_build/LinkTarget/Executable/wasmbridgegen` and the ~25 other
`gb_BUILD_TOOLS` executables. *Observable:* `./wasmbridgegen --help` runs on
arm64 macOS. *Note:* this is **not** a full native LibreOffice — it is on the
order of 300–400 TUs (sal 114, basegfx 77, i18npool 78, codemaker 32,
cppuhelper 27, cppu 26, …) plus ICU's native build, which dominates. Estimated
20–40 minutes on this host without ccache; **measure it with `time` on the
first real run** rather than trusting that estimate.

**M2 — `libsal.a` cross-builds and a wasip2 program using it runs.**
`make sal` completes, then link a ten-line program against `libsal.a` that
opens a file through `osl::File` and prints, and run it under wasmtime.
*Observable:* the output. *Why this rung exists:* every wasi-libc gap
(`pwd.h`, `dlsym`, `pthread_atfork`, `getuid`) lives in `sal`, and finding them
here costs minutes instead of at the end of a multi-hour full build. This is
the direct analogue of Qt's M0.

**M3 — `soffice.wasm` links, headless.** The full `--disable-gui` build
completes and produces a component. *Observable:* the file exists,
`wasm-tools validate --features all` passes, and `wasm-tools component wit`
prints a world importing `wasi:cli`/`wasi:filesystem` and exporting
`wasi:cli/run`. **This is the rung most likely to fail**, and it is where the
link-memory and archive-graph questions get answered.

**M4 — it converts a `.pptx` to a PDF.** Run the headless component under
wasmtime (or as a plain wk node with a BindMount) with
`--headless --convert-to pdf sample.pptx`. *Observable:* a PDF you can open.
*What it proves, all at once:* the UNO bootstrap, `services.rdb` + the
constructor map, the type registries, `oox` import, the `sd` model, the
drawinglayer, the font stack, the VFS layout, **and — crucially — that a
single-threaded LibreOffice can make progress at all.** Every reachable
condvar wait between `main()` and a finished document shows up here as an
`abort()` with a stack trace. Do not start any VCL work before this rung.

**M5 — svp renders a slide to a PNG.** `--convert-to png` on the same file, or
LO's own `screenshot_test`-shaped path. *Observable:* an image of a slide.
Proves Cairo + FreeType + fontconfig + the whole svp graphics stack with **no
wk graphics involved**. Qt's M1 was exactly this rung and for exactly this
reason: it produces an artifact you can look at.

**M6 — Impress's UI paints into the wk surface.** *(Done. The rungs below were
written before any of this was built; they are left as written because the
reasoning still holds and because what actually happened is in `## Current
state` and in the commit messages, not here.)* Switch off `--disable-gui`,
register `wk` as the single VCL plugin, add `vcl/wk/` (`WkSalInstance`,
`WkSalFrame`, the compositor, the input translator). *Observable:* a wk-server
harness pumps frames and dumps one, and the dump is a LibreOffice Impress
window — menu bar, toolbar, slide panel, the outline of a slide. Pixel-assert
it, not just narration: `plugins/qt-kcalc` spent hours on a bug where the app
narrated correct state onto a blank screen.

**M7 — a menu opens and a click lands.** A real `wasi:surface` pointer event on
"Insert" opens the menu; the frame count goes 1 → 2. *Observable:* the second
`SalFrame` appears in the dump at the right place. This is the rung that
exercises the guest-side compositor's z-order and hit-testing — the largest
piece of genuinely new code in the port.

**M8 — typing and a modal dialog.** Type into a text box on a slide and see the
character; open a dialog (Slide Properties) and have `Dialog::Execute`'s nested
yield loop return a value. *Observable:* the glyph in the dump, the dialog's
return code in the narration. This is where upstream's wasm build gives up,
so there is nothing to compare against.

**M9 — F5.** A slideshow runs full-surface with a transition between two
slides. *Observable:* two dumps, before and after, plus the animation timing not
wedging the yield loop.

**M10 — a real `.pptx` round-trip as a shipped node.** `plugins/libreoffice/
Dockerfile` + a `dependencies` entry in `workspace.wk` + an `example/*.wk` that
wires a BindMount, opens a deck, edits it, saves it, and shows it.

---

## Where this port is most likely to die — and the cheapest experiment for each

Ranked by (probability × cost of discovering late). Every experiment here is
hours, and all six together are less than one failed overnight build.

**1. The final link.** `wasm-ld`/`wasm-component-ld` has to swallow ~200
archives, thousands of objects and a `component_maps.cxx` referencing 1000+
constructors, on a 32 GB host — and upstream's own `README.wasm.md` says the
Emscripten link "possibly needs 64GB RAM". Worse, gbuild's group flags are
rejected (verified), and LO's static build genuinely relies on group re-scanning
for circular dependencies between its archives (`static.mk:26-51` describes the
cycle). `--start-lib/--end-lib` is a *different* mechanism (force all members
in), not a drop-in.
> **E1 (30 minutes, and it can kill the port): two mutually-referencing static
> archives.** Build `liba.a` calling into `libb.a` and back, link them for
> wasm32-wasip2 **without** any group flag, in both orders. If wasm-ld resolves
> it, the mechanical fix in `unxgcc.mk` is enough. If not, you need
> `--start-lib`/`--end-lib` or archive-order surgery, and you want to know that
> before writing the platform file, not after a four-hour build.

**2. Component size and instantiation.** LibreOffice will produce the largest
wasm artifact in this repo by a wide margin. `plugins/bun/bun-run.wasm`
(182,569,475 B) validates as a component in 0.1 s — but **nothing in
`crates/wk-server` references it and it has never been instantiated as a node**.
The largest artifact with a green wk-server test is 22.7 MB; the largest ever
observed painting is 41.8 MB.
> **E2 (1 hour): instantiate a 182 MB component on wk's real runtime.** Point a
> throwaway `PluginHost` at `plugins/bun/bun-run.wasm` and measure compile
> time, the on-disk code-cache size and peak RSS. This answers "does wk's
> wasmtime scale to LibreOffice" for free, using a binary that already exists.

**3. Threads: the unbounded tail.** The known sites are enumerated and each has
a serial path beside it, but nobody can prove by reading how many condvar waits
are *reachable*. The ones already found, in likely-to-fire order: `package`'s
`>100 KB` zip-save branch (`ZipPackageStream.cxx:804`), `configmgr`'s
`WriteThread` (`components.cxx:308`), `framework`'s `SharedWakeUpThread`
(the document-load progress bar, launched from a *constructor*), and
`sfx2`'s `CheckReadOnlyTask` — **the one task that must not be inlined**,
because its `doWork` is `while(true) { mCond.wait_for(60s); … }` and inlining it
hangs the main thread forever.
> **E3 (free, it is M4): run the headless convert.** There is no cheaper probe
> and no reading that substitutes for it. Do the `comphelper::ThreadPool` patch
> **before** M4 though — `pushTask` does `maWorkers.push_back(new
> ThreadWorker(this))` *before* `launch()` throws, so the first failed spawn
> leaves a non-existent worker in the list permanently, which makes every later
> `waitUntilDone` skip the existing inline path (`threadpool.cxx:271-273`) and
> fall into the aborting `wait_for`. ~10 lines, inline-at-push (not
> inline-at-wait — `ZipOutputStream.cxx:154` spins on `sleep_for` waiting for
> pushed entries), and it fixes the zip save, threaded graphic import and all
> three vcl bitmap filters at once.

**4. The UNO C++ bridge. THIS IS NOW THE ONE OPEN BLOCKER, and it has been
narrowed to a single `#include` and a single function.** Everything else in
`bridges` cross-compiles: `./build-lo.sh bridges.allbuild` builds `abi.cxx`,
`uno2cpp.cxx`, all six `cpp_uno/shared/*.cxx` and
`StaticLibrary_emscriptencxxabi` with **zero errors**, and dies on exactly one
line — `bridges/source/cpp_uno/gcc3_wasm/cpp2uno.cxx:15: fatal error:
'emscripten.h' file not found`. The only thing that header is there for is the
`EM_JS jsGetExportedSymbol` at `:32`, whose only caller is `Rtti::getRtti`,
whose only job is to find the address of a UNO exception type's `_ZTI…` RTTI
object by name. There is no JS, and a wasm component has no runtime symbol
table.

**Two facts were measured this session and they settle the shape of the fix:**

1. **The existing fallback is not usable, and "return nullptr" is therefore not
   an option.** When `jsGetExportedSymbol` returns 0, `cpp2uno.cxx:117-135`
   synthesizes a fresh `__class_type_info`/`__si_class_type_info` from a
   `strdup`'d name. On Emscripten that is survivable. On wasi-sdk 34 it is not:
   `<typeinfo>` picks `_LIBCPP_TYPEINFO_COMPARISON_IMPLEMENTATION 1` (the
   "Unique" implementation, which compares `__type_name` **by address**) for
   every platform that is not COFF/XCOFF or arm64 Apple. Verified by running: a
   synthesized `__si_class_type_info` carrying the byte-identical mangled name
   `N3tst7DerivedE` compares unequal to the real `typeid`, and `__cxa_throw`
   with it falls through both `catch (Derived&)` and `catch (Base&)` into
   `catch (...)`, while the same throw with the real RTTI is caught normally.
   **Always synthesizing would mean every `catch (css::uno::Exception&)` in
   LibreOffice silently missing every exception the bridge raises.**
2. **A link-time table works, including the "symbol is not in this image" case,
   which is the half that was not obvious.** `wasmbridgegen` **already**
   enumerates every UNO exception type and computes its mangled RTTI name —
   `scan()`'s `SORT_EXCEPTION_TYPE` arm calls `computeRttiSymbol`, and the names
   go into the `exports` file so that Emscripten keeps and exports them. So the
   generator needs no new knowledge, only a new output. Verified by running,
   under `wasm32-wasip2` with this port's flag set:
   `extern "C" extern char _ZTIN3tst7DerivedE __attribute__((weak));` binds to
   the **real** RTTI address (`__cxa_throw` with it is caught by the real base
   class), and a weak reference to a name that is in no object **links anyway
   and reads as `nullptr`** — which is precisely `jsGetExportedSymbol`'s
   returned-0 contract, so `cpp2uno.cxx`'s existing synthesize-fallback stays
   reachable and unchanged for types nothing in the image ever catches. Weak
   references also do not drag objects out of archives, so the table costs
   nothing but its own entries.
> **E4 is now "write it", not "prototype it".** The remaining decisions are
> where the table lives (a fourth `wasmbridgegen` output compiled into
> `libgcc3_uno.a`, next to `generated-cxx.cxx`, is the obvious place) and how
> `getRtti` is spelled on WASI so the Emscripten path is untouched. Note the
> table is ~one entry per UNO exception type across udkapi+offapi, so check the
> generated file's size before assuming it is free. Also still to do: audit
> `bridges/source/emscriptencxxabi/cxxabi.cxx` against wasi-sdk 34's libcxxabi
> layout — it compiled, but "compiles" has been shown not to mean "unwinds".
> **The wasm-specific risk on the *vtable* side is already retired:** the exact
> `void X(void);` mis-declaration + cast-and-call pattern that `native-code.py`
> uses was reproduced standalone and **linked silently and ran correctly**
> under wasm32-wasip2, including with the definition inside a static archive.

**5. The externals, especially cairo/pixman under meson.**
`external/cairo/ExternalProject_pixman.mk:28-42` generates a meson cross-file
that maps everything non-Windows/macOS/Android to `system = 'linux'` and takes
`cpu_family` from `RTL_ARCH` — i.e. it would tell meson "linux/x86" while the
compiler identifies as wasm32. cairo is load-bearing for svp. Separately, each
of the 149 external tarballs ships its **own** `config.sub`, and
`HOST_PLATFORM` (`wasm32-unknown-wasip2`) is passed verbatim as `--host=`.
> **E5 (an afternoon, fully independent of LibreOffice): build pixman + cairo +
> freetype + fontconfig standalone against wasi-sdk 34**, out of tree, with
> `-DCAIRO_NO_MUTEX -Dxlib=disabled -Dxcb=disabled -Ddefault_library=static`
> and libxml2 in place of expat. This is the `plugins/netsurf/build-deps.sh`
> shape and it can be done in parallel with everything else, by someone who has
> never seen LibreOffice's build system.

**6. Reachability of the wk surface model from VCL.** One wk surface per node
vs. one `SalFrame` per menu popup, tooltip, combo dropdown, floating toolbar and
dialog.
> **E6 (a day, at M6): stub `WkSalFrame` and count.** Log every `SalFrame`
> creation with its style flags and `maGeometry` while driving Impress headless
> through opening a menu. You will know the real z-order and coordinate problem
> before writing the compositor, instead of discovering it through unclickable
> menus.

---

## Known gaps, carried from the survey

* **The svp resize dance touches private state.** `SvpSalFrame::SetPosSize`
  destroys and recreates `m_pSurface` without preserving the Cairo user-data
  damage key or the old pixels. `WkSalFrame::SetPosSize` must reinstall the key
  after calling the base — reachable via the public
  `SvpSalGraphics::getSurface()`/`getDamageKey()`, but `SetPosSize` also calls
  `setSurface()` on already-cached graphics in the private `m_aGraphics`, and
  it is unverified that every path leaves a live `SvpSalGraphics` to reach the
  new surface through. If not, the fix is a two-line upstream patch adding a
  protected accessor or a virtual `surfaceChanged()` hook. Read
  `vcl/qt5/QtFrame.cxx:65-216` and `:1340-1405` before writing a line of
  `vcl/wk/` — it is the working in-tree blueprint for exactly this operation,
  including a `copySource` trick that avoids a white flash on resize.
* **wkgfx has no pointer-leave, focus-change or window-close event.**
  `wkgfx_event_type` is `KEY_DOWN/UP`, `POINTER_MOVE/DOWN/UP`, `SCROLL`,
  `RESIZE`. VCL wants `SalEvent::MouseLeave`, `GetFocus`/`LoseFocus` and
  `Close`. Missing `MouseLeave` in particular makes hover highlight in menus
  stick. Same gap the Qt QPA has; fixing it is a `plugins/gfx-compat` +
  compositor change, not a VCL one.
* **No IME**, same as Qt, same cause.
* **DPI is hardcoded.** `SvpSalGraphics::GetResolution` returns 96×96 and the
  Qt QPA pins `devicePixelRatio` to 1.0 with the host scaling on present.
  Whether wk exposes a scale factor a VCL backend should honour is unchecked.
* **`SvpSalGraphics::SupportsCairo()` returns false**, so `cairocanvas` is
  unavailable under plain svp and canvas falls back to `vclcanvas`
  (`QtSvpGraphics` returns true and supplies an 88-line `QtSvpSurface`).
  Whether `slideshow/` needs `cairocanvas` or is content with `vclcanvas` is
  **not traced**, and neither is what `SvpSalFrame::StartPresentation` (an
  empty body) should do for a wk node. This is an M9 question.
* **What the general `ENABLE_WASM_STRIP` removes is not traced.** It is forced
  on in the emscripten arm (`configure.ac:1259`) and a WASI arm would copy it.
  It sets `with_galleries=no`, `with_templates=no`, `enable_scripting=no`.
  Whether it also strips something the slideshow engine needs is unknown —
  and carrying it over uncritically is exactly the copy-the-emscripten-arm
  mistake that would surface at M9.
* **Missing from the shipped image, by design, and user-visible:** no
  presentation templates (Impress's startup template picker will be empty), no
  colour palettes, no autocorrect data, no dictionaries. All are cheap opt-in
  layers from sources already on disk: `extras/source/templates/presnt` is
  11 MB / 23 designs (or a 3-design subset under 1 MB), `extras/source/palettes`
  ~200 KB installed, `extras/source/autocorr/lang/en-US` 224 KB. Add the
  templates and the palettes; they cost nothing in the binary.
* **Settings will not persist on day one.** The one-line posture is to declare
  the user config layer read-only in our `fundamentalrc` — `user:*${…}` instead
  of `user:!${…}` (`configmgr/source/components.cxx:594-598` parses `*` as
  write=false) — which makes settings load but never save. The real fix is the
  synchronous-write patch to `Components::writeModifications`.

## The runtime image

Upstream ships an exact, machine-readable manifest of everything a wasm
`soffice` needs on disk: **`static/CustomTarget_emscripten_fs_image.mk`** (1,833
lines, 1,656 explicit `$(INSTROOT)`-relative paths plus three package filelists
and one autoinstall set). Its own comment states the contract: *"Currently WASM
simply assumes the image has the same layout then instdir."* Replace the
`file_packager` call with a tar emitter over the same list and the data problem
is solved by construction. Do not guess; do not run Android's
`mobile-config.py`, which deletes `DrawImpressCommands` and `GenericCommands` —
precisely the menus this port exists to show.

Measured and estimated sizes for an Impress build, coldest first (one `COPY` per
Dockerfile layer, `soffice.wasm` **last** so a binary iteration invalidates one
layer instead of fifty megabytes):

| layer | size | note |
|---|---|---|
| `share/config/soffice.cfg` | **17.0 MB / 1092 files** (measured) | 6.66 MB unconditional + 9.62 MB Impress/Draw/Math + 0.77 MB chart. Writer's 8.95 MB and Calc's 5.88 MB are stripped. 100% relevant — do not prune. |
| `program/types*.rdb` + `services*.rdb` | ~12 MB (**estimated**) | offapi/oovbaapi/udkapi. Static registration replaces dlopen, **not** the service catalogue. Unmeasurable without a build. |
| `share/fonts/truetype` (curated) | ~10 MB (estimated) | Android's five: Liberation, Caladea, Carlito, Gentium, OpenSymbol — which is exactly the PowerPoint-interop set (metric-compatible with Arial/Times/Courier, Cambria, Calibri). |
| `share/liblangtag` | ~5 MB (estimated) | 14 files; mandatory, i18nlangtag cannot resolve locales without it. |
| `share/config/images_colibre.zip` | ~3.8 MB (measured by zipping) | one file, the only theme built. |
| `share/registry/*.xcd` | ~3.5 MB (estimated from sources × a measured 0.71–0.88 xsltproc trim ratio) | `main.xcd` is ~2.5 MB of it; `impress.xcd` only ~200 KB. |
| `share/filter` | 1.7 MB (measured) | the OOXML preset-shape catalogue — required for `.pptx` import. |
| `share/gallery/fontwork.*` + themes + presets | ~1.0 MB (measured) | |
| `share/fontconfig` | ~0.1 MB | 43 `conf.d` files; **the cache dir must be writable**. |
| translations | **0 bytes** | `AllLangMoTarget.mk:60` filters `en-US` out. An en-US build ships no catalogues at all. |
| ICU data | **0 bytes on disk** | `--with-data-packaging=static` under `DISABLE_DYNLOADING` links it **into the module**. Budget for a much larger `.wasm`, not a data layer. |

≈ **51 MB core image**, plus the module. wk's layer model makes that cheap:
`crates/wk-vfs/src/layers.rs` indexes a layer lazily from an on-disk tar, so
mounting costs directory entries, not RAM, and only the files a session
actually touches are ever paged in.

Entrypoint: `["/instdir/program/soffice.wasm", "--norestore", "--nologo",
"-env:UserInstallation=file:///root/.libreoffice", "--impress"]` — all four
switches verified present in `desktop/source/app/cmdlineargs.cxx`. Plus
`ENV HOME=/root`, `ENV TMPDIR=/tmp` (`sal/osl/unx/tempfile.cxx:35` is a plain
`TMPDIR`/`TEMP`/`TMP`/`/tmp` chain), and an empty `/tmp` in the image. Wire a
wk Volume at the `UserInstallation` path if Impress's settings should survive a
node restart.

## Build layout — written, and what each stage is for

Modelled on `plugins/qt` and `plugins/netsurf`. Three stages, plus a probe:

| script | milestone | wall-clock |
|---|---|---|
| `preflight.sh` | — | seconds |
| `build-shim.sh` | — | a second |
| `build-configure.sh` | M0 | minutes |
| `build-host.sh` (`make cross-toolset`) | M1 | 20–40 min, **estimated** |
| `build-lo.sh` (`make`, or `make <module>`) | M2 / M3 | hours |

Ordering note that differs from the Qt port: there, `build-host.sh` is a
genuinely independent native build that can run first. Here it cannot —
LibreOffice's *one* `configure` run produces both the host config and, through
a whole nested configure in `build/CONF-FOR-BUILD`, the native build config.
So configure comes first and `make cross-toolset` second.

The reasoning behind every configure flag is in `build-configure.sh` itself,
next to the flag, with the `configure.ac` line that forces it. It also carries
a **deliberately NOT passed** list, which is the half that gets lost otherwise.

Four decisions in those scripts that are easy to undo by accident:

* **`CFLAGS`/`CXXFLAGS`/`LDFLAGS` are never set in the environment.**
  `solenv/gbuild/LinkTarget.mk:66,68,72` treat a non-empty one as a
  *replacement* for LibreOffice's own `-g`/`-O` handling, not an addition. The
  compile flags ride inside `CC`/`CXX` instead — which is also the only route
  that reaches the **externals**, since `config_host.mk.in:64` exports `CC`
  into the whole make environment. Upstream's Emscripten build gets this wrong
  in the other direction: `grep -rn gb_EMSCRIPTEN_EXCEPT` matches only the
  platform `.mk`, so the EH flag never reaches `external/` at all. Survivable
  for emcc; for us it would mean ICU, boost and harfbuzz compiled against the
  `noeh` libc++.
* **Two PATHs, and which stage gets which matters.** `build-configure.sh` and
  `build-host.sh` run with wasi-sdk **off** PATH (every cross tool is passed by
  absolute path), because `configure.ac:6205-6208` unsets `CC`/`CXX`/`AR`/… before
  the BUILD sub-configure and it then autodetects — and `which -a clang` here
  puts wasi-sdk's wasm32-wasip1 clang ahead of Apple's. `build-lo.sh` runs with
  wasi-sdk **first**, because externals' libtool fragments call `ar`/`ranlib`
  by bare name. Neither PATH can reach a `wasm-opt`; that is subtractive rather
  than a wrapper, and wasi-sdk ships none of its own.
* **No `-j` anywhere.** `Makefile.in:87` is
  `PARALLELISM_OPTION := -j $(PARALLELISM)` and every recursive `$(MAKE)`
  already carries it, from `--with-parallelism`.
* **`build-configure.sh` refuses to run** until the two structural patches
  exist, printing what each must contain. Their absence otherwise surfaces as
  *"wasip2 operating system is not suitable to build LibreOffice for!"* and as
  a gbuild include error naming a file nobody has heard of.

Everything else follows the house rules: upstream is **fetched, never vendored,
never edited in place**; every change is `patches/core-NNNN-<slug>.patch`
applied with the reverse-check idiom, with a header answering WHAT / WHY /
UPSTREAM (see `patches/README.md` — that last field splits this patch set
cleanly, since a `wasm32-wasi` host triple is genuinely upstreamable and
`vcl/wk/` never is); `mise.toml`'s `build` task self-skips while `patches/` is
empty so the repo-wide sweep does not sit in a LibreOffice build; and long
builds run detached with `./logs` tailed.

### Host tools missing on this machine

Reported, not installed, per the house rules:

* **GNU Make is 3.81 and there is no `gmake`.** `configure.ac:6907` requires
  ≥ 4.2. This is where the configure probe actually died — **on the BUILD side**,
  so the error arrives indented under *"Running the configure script for BUILD
  side failed"*. Do not misread that as a cross-compilation problem.
  `brew install make`.
* **`gperf` is 3.0.3** (Xcode's); `configure.ac:8201` requires ≥ 3.1.
  `brew install gperf`.
* **`ccache` is absent.** This is a two-stage build of one of the largest C++
  codebases in existence on 10 cores; without it, every configure-flag
  experiment re-pays the full native bootstrap. `--enable-ccache` **and**
  `--with-build-platform-configure-options=--enable-ccache`
  (`static/README.wasm.md:130-137`).
* `nasm` is **not** needed — see the traps.
* **`meson` is absent, and that is not fatal either.** `configure.ac:14751`
  warns and falls back to the internal `meson-1.8.3` from `download.lst`
  (`BUILD_TYPE="$BUILD_TYPE MESON"`, `MESON=$(gb_UnpackedTarball_workdir)/meson/meson.py`),
  which is the copy cairo, pixman and harfbuzz get built with. An earlier survey
  listed meson as an unprobed risk; it is not one. Note the version floor it
  would have applied to a *system* meson: 1.3.0 when internal cairo is in play,
  and 1.8.3 on macOS SDK ≥ 26 — i.e. even an installed Homebrew meson would
  likely have been rejected on this machine.
* **`bison` is Apple's 2.3, and that is fine.** `configure.ac:12352` needs
  ≥ 2.0; the ≥ 2.4 check at `:12348` is scoped to `--enable-compiler-plugins`,
  which the WASI host arm switches off.
* `flex`, `m4`, `perl`, `zip`, `xsltproc`, `xmllint`, `autoconf` 2.73 and
  `automake` 1.18.1 all resolve. `preflight.sh` checks the lot and prints the
  `configure.ac` line for each thing it rejects.

---

## Current state

**LibreOffice Impress runs as a wk node, draws its own window, and takes
input.** Verified by `cargo test -p wk-server libreoffice_impress_paints`,
which spawns `soffice.bin` on `PluginHost`, bind-mounts the install tree,
pumps frames, and asserts on the pixels wk gets back — then waits for the
window to stop changing on its own and presses Tab, which selects a shape.
Set `WK_LO_DUMP=/tmp/f.ppm` to keep the frame and look at it.

The detail of how each rung fell is in the commit messages, which are the
running log this section used to be; what follows is only where things stand.

| milestone | state |
|---|---|
| M0 configure completes | **done** — `config_host.mk` has `OS=WASI` |
| M1 native bootstrap | **done** — `wasmbridgegen` and ~39 build-side tools |
| M2 `sal`, `salhelper`, `store`, `registry`, `unoidl`, `cppu` cross-build | **done** |
| the C++/UNO bridge (`bridges`) | **done** — `gcc3_wasm` with real RTTI rather than a lookup table, plus the fix that mattered: `VtableFactory`'s arena mmaps read-write and then mprotects `PROT_EXEC`, which wasi-libc answers `ENOSYS`, so **every UNO exception that had to cross the bridge killed the process and reported itself as `std::bad_alloc`** |
| M3 `soffice.bin` links | **done** — a 190 MB wasm **component**; `wasm-tools validate` passes and `wasm-tools component wit` parses it |
| M4 `.odp` → PDF, headless | **done** — one A4-landscape page, an embedded LiberationSerif subset, text and bezier operators |
| M5 svp renders a slide to PNG | **done** — 1058x595, the page white, the shapes their own colours |
| M6 the GUI build shape | **done** — `--disable-gui` is gone, `HAVE_FEATURE_UI` is 1, `VCL_PLUGIN_INFO=wk`, and the headless conversions are unchanged to the pixel |
| M7 Impress paints into the wk surface | **done** — `vcl/wk/wkcompositor.cxx` flattens VCL's many SalFrames into the one surface a node has |
| M8 input | **done** — `vcl/wk/wkinput.cxx`; hit-tested against the frame register, keys carrying both the physical code and the character |
| M9 a shipped node | **done enough to use** — `libreoffice` is a node type in `workspace.wk` and `example/impress.wk` opens a deck. It is a BindMount of `build/instdir`, not an image; see that file for why |

### What is not done

* **A slideshow (F5) has never been run**, and neither has a transition. The
  animation timing against a host-driven frame clock is untested.
* **The clipboard is not wired.** `plugins/clipboard-compat` exists and the Qt
  port uses it; vcl/wk does not.
* **No image.** The node needs `build/instdir` bind-mounted from the repo, so
  it is not something you can hand to someone else. 644 MB is the number to
  beat, and most of it is not needed at run time (`.a` files, the SDK).
* **Resize is written but barely exercised.** `WKGFX_RESIZE` resizes top-level
  frames; nothing has yet resized the node on the canvas and watched.
* **The user profile does not persist** in the example — it lives in the node's
  in-memory filesystem. A Volume at `/profile` would fix it.
* **Cold-cache build times are still unmeasured.** Everything here was built
  with a warm `ccache`, and wasmtime's compile cache makes the test 3 s warm
  against minutes cold.

### Two bugs found in wk itself

Worth separating from the port, because they were never LibreOffice's:

* **wk's vfs did not resolve `..`.** `components()` dropped `.` and passed `..`
  through as an ordinary name. LibreOffice is built on such paths —
  `fundamentalrc` sets `BRAND_BASE_DIR` to `${ORIGIN}/..` — so its configuration
  registry, UI configuration and presets are all read through a parent link.
  There were **two** resolvers, and fixing one was worse than fixing neither: a
  file read through `..` worked while a directory stat through `..` did not, so
  the configuration loaded and the UI configuration then failed with "does not
  denote an existing directory", which reads like a missing file.
* **`salhelper::Thread::launch()` double-frees when thread creation fails.** It
  acquires a reference and a ScopeGuard releases it on failure, taking the count
  to zero and `delete this` — while the subclass constructor that called it is
  still on the stack, so the `new` expression frees the same storage again.
  Upstream never sees it because `osl::Thread::create` does not fail where there
  are threads. Here it never succeeds, and the result was a corrupted heap and a
  fault half a second later in unrelated code.

### Diagnostics this port had to build

On wasm there is no core dump, no debugger, and `std::current_exception()` is
NULL inside a `std::terminate` handler — verified with a five-line test case, so
the usual rethrow-and-print cannot name an uncaught exception and it reaches the
host as a bare `wasm trap: unreachable`. `shim/wk-wasi-threads.c` therefore
wraps `__cxa_throw` and `malloc` through `wasm-ld --wrap`:

| variable | what it does |
|---|---|
| `WK_LO_TRACE_THROW=1` | print every exception's mangled type as it is thrown |
| `WK_LO_TRAP_THROW=<substring>` | `abort()` **at** the throw, so the backtrace names the code that threw rather than wherever `terminate` was reached |
| `WK_LO_TRACE_ALLOC=<bytes>` | print allocations at least this large |
| `WK_LO_DUMP_FRAMES=<path>` `WK_LO_DUMP_AFTER=<n>` | write each visible SalFrame's own surface to PNG after n yields |
| `SAL_LOG=+INFO.vcl.wk` | vcl/wk's frame census: every frame with its style, geometry and visibility |

A failed `malloc` always reports its size and the linear memory it failed
against, with no flag to ask for it. `WK_LO_TRAP_THROW=bad_alloc` is what turned
"`std::bad_alloc`, somewhere" into sixteen named frames ending in
`VtableFactory::createVtables`, which is the bridge bug above.

One scar in the shim worth reading before touching it: the flags are resolved in
a constructor rather than lazily, and the malloc trace formats its own decimal
rather than calling `snprintf`, because **wasi-libc's `getenv` allocates**.
Reading the environment from inside `malloc` is unbounded recursion, and it
presents as the trace being silent and the process dying somewhere else.

### A record of the 2026-09-01 mid-port verification

Everything from here to the end of the section describes the port **as it stood
on 2026-09-01**, when `bridges` was the open blocker and nothing above `cppu`
had been built. It is kept because the method is worth repeating -- wipe the
build tree, apply only what is in `patches/`, and check that the artefacts come
back byte-identical -- and because a plan that edits away its own wrong turns
teaches nothing. Read the numbers as history: there were eight patches then and
there are twenty-two now, and the state table above is the current one.

### Claims this document used to make that the re-verification retired

Recorded rather than quietly deleted, because a plan that edits away its own
wrong turns teaches nothing.

* **"six patches in all"** — the patch-set row said six long after `core-0007`
  and `core-0008` were written. It is **eight**, and the row now says how many
  lines they are so the next drift is visible.
* **"the `+ a wasip2 program that runs` half is NOT done — no `.wasm` has been
  executed"** — true when written, false since M2⅞. Four separate things have
  now run under wasmtime: the `unoidl` `.rdb` reader, the `cppu` 8-check probe,
  the thread-shim harnesses, and `osl_process_child`, which is the first wasm
  **component gbuild itself linked**.
* **"E1 … not started"** — understated. Six LibreOffice archives now go through
  `wasm-component-ld` in one left-to-right pass with no group flags, via
  gbuild's own recipe. It is a chain and not the cycle, so E1 is *narrowed*,
  not closed.
* **M1 "done"** was, until this session, a claim about a directory that
  happened to exist. `workdir_for_build`'s gbuild outputs had never been
  deleted, so nothing had ever shown that the committed patches produce
  `wasmbridgegen`. Now they have.
* The M2 row quoted `libuno_sal.a` as 2,395,198 bytes. That was the size before
  the `getentropy` arm of decision 20; the archive is **2,394,402** bytes and
  has been since M2¹⁵⁄₁₆. Two rows of the same table disagreed with each other.

### What was verified by running something

* **The whole chain reproduces from `patches/` alone, byte for byte**
  (2026-09-01). From a pristine `src/` and with `build/workdir`,
  `build/instdir`, `build/instdir_for_build` and every gbuild-produced
  directory under `build/workdir_for_build` deleted:
  `./build-configure.sh` → `=== configured: OS=WASI`;
  `./build-host.sh` → 520 `[build CXX]`, 55 links, 0 errors, 39 executables
  with `wasmbridgegen` freshly relinked;
  `./build-lo.sh {sal,salhelper,store,registry,unoidl,cppu}.allbuild` → six
  `[build MOD]` lines and 0 `error:` lines, leaving 155 object files under
  `build/workdir` where the wipe had left none.
  **All eleven archives came back with the same SHA-256, size and member list
  as before the wipe** — `libuno_sal.a` 2,394,402 / 85,
  `libuno_salhelpergcc3.a` 36,052 / 5, `libstorelo.a` 172,782 / 11,
  `libreglo.a` 128,298 / 6, `libunoidllo.a` 983,308 / 7, `libuno_cppu.a`
  354,092 / 20, `libuno_purpenvhelpergcc3.a` 22,272 / 3, `libaffine_uno_uno.a`
  10,904 / 1, `liblog_uno_uno.a` 6,186 / 1, `libunsafe_uno_uno.a` 4,882 / 1,
  `libzlib.a` 102,012 / 14. This is the first time anyone has shown that the
  committed patches — rather than some session's working tree — are what
  produced the artefacts in `build/`.
* **A wasm component that gbuild itself linked, executed.**
  `sal.allbuild` links `workdir/LinkTarget/Executable/osl_process_child`
  (**3,330,577 bytes**) through `wasm-component-ld` from six archives plus the
  shim, with no `--start-group`. `llvm-objdump` refuses it — *"invalid version
  number: 65549"* is the component preamble, not a corrupt file — and
  `wasmtime` runs it to exit 0. Every earlier "it links" claim in this document
  was about an `ar` archive.
* **`-pthread` really is off the compile line**, read rather than inferred:
  `touch src/sal/rtl/crc.cxx` then `make verbose=t sal.allbuild`, and the
  emitted `wasi-clang++` command contains `-fwasm-exceptions`, `-std=c++20`
  and no `-pthread` anywhere.
* `git clone --depth 1 --branch libreoffice-26.2.6.2` → `git describe --tags`
  confirms the tag; `configure.ac:2` confirms the version.
* `aclocal -I m4 -I m4/mac && autoconf -I .` → rc 0, a 1,712,088-byte
  `configure`; bare `autoconf` → a 1,151,105-byte one that dies at runtime.
* `sh ./configure --help` lists `--with-wasm-module=<writer/calc/impress>`,
  `--enable-wasm-strip`, `--enable-emscripten-jspi`.
* `sh ./config.sub wasm32-unknown-wasip2` → `wasm32-unknown-wasip2` (parses).
* Unpatched `configure --host=wasm32-unknown-wasip2` → `configure: error:
  wasip2 operating system is not suitable to build LibreOffice for!`; with the
  throwaway WASI arm → 160+ checks and into the BUILD sub-configure, dying on
  GNU Make 3.81. **`configure.ac` was reverted; `git status --porcelain` in
  `src/` is empty.**
* `clang --target=wasm32-wasip2 -Wl,--start-group` → rejected by
  `wasm-component-ld`; `--target=wasm32-wasip1` → rejected by `wasm-ld`.
  `--gc-sections` and `--whole-archive` accepted; `--no-as-needed` rejected.
* Under wasmtime: `pthread_create` → 58; `std::thread` ctor throws;
  `hardware_concurrency()` → 1; `pthread_cond_wait` traps on `unreachable`;
  `std::condition_variable::wait_for` → `abort()`, exit 134; recursive mutexes,
  `pthread_key`, `sleep_for` and `sched_yield` all fine.
* **The thread shim, end to end.** (a) The five-line `wait_for` program aborts
  under wasmtime — `__do_timed_wait` → `std::terminate` → `abort`, exit 134 —
  *before* the shim existed. (b) `pthread_cond_timedwait` called directly
  returns **58 in 0.0 ms** with both a normal and a recursive mutex, and the
  reason is the clock, not the mutex: `clock_nanosleep` on `CLOCK_REALTIME`
  returns 58 immediately in both the absolute and the relative form, while
  `nanosleep` and both `CLOCK_MONOTONIC` forms sleep the requested 50 ms.
  (c) Linked against the shim, the same program prints `timeout` and exits 0,
  and a timing harness shows `wait_for` sleeping 51.9 ms / 201.3 ms for 50 ms /
  200 ms, with the `wait_for(…, predicate)` form — `osl_waitCondition`'s exact
  shape — returning `false` after 102 ms. (d) `-Wl,--why-extract` names
  `libwkwasithreads.a(wk-wasi-threads.o)` as the archive member that satisfied
  libc++'s `pthread_cond_wait` reference, and **neither**
  `libc.a(pthread_cond_wait.c.obj)` nor `libc.a(pthread_cond_timedwait.c.obj)`
  appears in the extraction list at all. (e) Ordering is not academic: with
  `libc.a` named explicitly *before* the shim the abort comes straight back —
  which is why the makefile uses `--whole-archive`, verified to win even in
  that adversarial order. (f) The whole thing relinked through
  `toolwrap/wasi-clang++` with the exact `gb_LinkTarget_LDFLAGS` string the
  platform makefile now emits, `-Wl,--start-group`/`--end-group` included and
  dropped by the wrapper: links, runs, times out. (g) `WASI_INTEL_GCC.mk`
  parsed under GNU Make 4.x both ways — the `$(error)` fires with the
  "run build-shim.sh" message when the archive is absent, and expands to the
  `--whole-archive` triple when it is present.
* An `llvm-objdump` sweep over all 847 members of wasi-libc's `libc.a`: exactly
  three functions have a bare `unreachable` body (`abort`,
  `__stack_chk_fail_local`, `pthread_cond_wait`), plus `pthread_barrier_wait`
  and `__wasilibc_futex_wait`, which trap only on the branch that would block.
  `pthread_join` returns 0 without touching `*retval`; `pthread_mutex_timedlock`
  and the `pthread_rwlock_*` family are real, working, uncontended
  implementations. No object inside `libc.a` references `pthread_cond_wait`,
  `pthread_cond_timedwait` or `__pthread_cond_timedwait`, so libc's own members
  can never be pulled in behind the shim's back.
* `#include <pwd.h>` → *file not found*; `getuid()` → undeclared;
  `dlsym`/`dlopen` compile but fail to link with `undefined symbol`.
* **`sal` cross-compiles.** `./build-lo.sh sal.allbuild` from a pristine tree
  with all eight patches applied: `[build MOD] sal`, no errors,
  `build/instdir/program/libuno_sal.a` = **2,401,182 bytes / 85 members**,
  `llvm-objdump -h` → `file format wasm`, `llvm-nm --defined-only` → 276
  `osl_*` symbols including `osl_executeProcess`, `osl_createPipe`,
  `osl_getCurrentSecurity` and `onInitSignal()`. `llvm-ar t` confirms the
  substitution really happened: `pipe_wasi.o`, `process_wasi.o` and
  `signal_wasi.o` are members; `pipe.o`, `process.o` and `signal.o` are not.
  The qa helper `Executable/osl_process_child` links too, which is the first
  time anything in this port has been through `wasm-component-ld` end to end.
* **`salhelper` cross-compiles with the patch set unchanged.**
  `./build-lo.sh salhelper.allbuild` → `[build MOD] salhelper`, **`grep -c
  "error:"` on the log = 0**, first attempt, no eighth-patch edit and no ninth
  patch. `build/instdir/program/libuno_salhelpergcc3.a` = **36,028 bytes /
  5 members**, `llvm-objdump -h` → `file format wasm`. Every `warning:` in the
  log is from the BUILD-side (native arm64) generated lexers and parsers
  (`cfglex.cxx`, `xrmlex.cxx`, `sourceprovider-*.cxx`) — **not one warning came
  from a wasm compile**. Log: `logs/session-salhelper-1.log`.
* **The shim's two halves both appear in `timer.o`, and neither is reachable.**
  `llvm-nm --undefined-only` on the archive lists
  `_ZNSt3__218condition_variable15__do_timed_waitE…` *and*
  `_ZNSt3__218condition_variable4waitERNS_11unique_lockINS_5mutexEEE`, alongside
  `osl_createSuspendedThread`, `osl_joinWithThread` and `osl_waitCondition`.
  Both condvar references are inside `TimerManager::run()`, an `osl::Thread`
  body — see decision 17.
* **`CppunitTest_salhelper_testapi` was correctly skipped**, confirming the
  `gb_CAN_EXECUTE_HOST_CODE` reasoning in the traps section holds in practice:
  `grep -c CppunitTest` on the build log = 0, because
  `post_SpeedUpTargets.mk:15` puts `check` in `gb_Module_SKIPTARGETS`. Flipping
  that one line is what it will take to run cppunit under wasmtime.
* **`gb_Library_set_soversion_script` is inert under `DISABLE_DYNLOADING`.**
  `salhelper` sets one (`source/gcc3.map`), as do `sal`, `cppu`, `cppuhelper`
  and `purpenvhelper`. `unxgcc.mk:153-154` only expands `SOVERSIONSCRIPT` into
  `--soname`/`--version-script` inside `gb_LinkTarget__command_dynamiclink`,
  which a static `Library` target never reaches — so none of those five needs a
  WASI arm, and `wasm-ld` never sees a version script.
* **The eight patches still describe the tree exactly after a full session.**
  All eight pass `git -C src apply --reverse --check` against the working tree
  at the end of the `salhelper` work, and `git -C src status --porcelain` lists
  only the four expected untracked files (`WASI_INTEL_GCC.mk` and the three
  `*_wasi.cxx`) — i.e. this milestone added nothing to `src/` at all.
* **The patch set round-trips.** `git -C src diff` of the working tree is
  byte-identical to the tree produced by reverting to pristine and replaying
  all eight patches, and every patch passes both `git apply --check` on a
  pristine tree and `git apply --reverse --check` on a patched one, which is
  what makes `build-configure.sh` re-runnable. **Re-measured 2026-09-01: the
  working-tree diff is 1,443 lines and `cat patches/core-*.patch` is 1,443
  lines** — the two agree exactly, so nothing is duplicated between the eight
  files or missing from them. (It was 1,338 when this bullet was first written,
  before `core-0006` grew the `getentropy` arm; a bare line count is a fragile
  thing to quote and it should be re-run, not trusted.)
* **`/bin/sh -c 'echo -n foo bar'` prints `-n foo bar` on this machine**, while
  `$(shell echo -n a b)` in a standalone makefile under the same GNU Make 4.x
  prints `a b` — make execs `/bin/echo` directly when there is no pipe, and
  macOS's `/bin/echo` honours `-n`. That difference is the whole of decision 15.
* **`tm_gmtoff` resolves on wasip2 in C++ but the name is conditional.**
  wasi-libc declares the member as `__tm_gmtoff` and `<time.h>` renames it under
  `_BSD_SOURCE`/`_GNU_SOURCE`; `clang++ --target=wasm32-wasip2 -std=c++20
  -E -dM` shows `_GNU_SOURCE 1` (C++ mode sets it for libc++), so the rename is
  in effect for every LibreOffice compile. A C compile with `-std=c23` would
  NOT get it — worth knowing before adding a `.c` file that touches `struct tm`.
* The `native-code.py` constructor-table pattern (`void X(void);` +
  cast-and-call) reproduced standalone: links silently, runs correctly, works
  from inside a static archive.
* `-fwasm-exceptions -###` selects `eh/`; without it, `noeh/`. `preflight.sh`
  re-checks this every run rather than assuming it, along with the presence of
  `eh/libc++.a`, `eh/libc++abi.a` and `eh/libunwind.a`.
* `./preflight.sh` end to end: wasi-sdk complete (no `llvm-readelf`, as
  expected); GNU Make 3.81 and gperf 3.0.3 rejected with the `configure.ac`
  line that rejects them; `bison` 2.3 accepted; `ccache`, `meson` and `nasm`
  reported advisory-only; `config.sub wasm32-unknown-wasip2` round-trips; and
  `autogen.sh --help` (i.e. `aclocal -I m4 -I m4/mac` + `autoconf -I .` +
  `./configure --help`) runs out of tree and lists all nine options the flag
  wall depends on. `src/` is untouched afterwards. Exit 1, 2 blocking.
* Each build stage refuses correctly with nothing in `patches/`:
  `build-configure.sh` prints what both structural patches must contain and
  exits 1; `build-host.sh` and `build-lo.sh` exit 1 on the missing
  `config_host.mk`; the `WASI_SDK` guard exits 1 on a wrong SDK; and
  `mise run build` self-skips.
* Meson's *availability* is **not** a risk: `configure.ac:14739-14755` falls
  back to the internal `meson-1.8.3` when none is found. This retires only the
  "is meson installed" question — kill-shot 5, the cairo/pixman cross-file that
  claims `system='linux'` against a wasm32 compiler, is untouched and still the
  load-bearing externals risk.
* `llvm-nm --defined-only plugins/qt/sysroot/lib/libQt6Gui.a | grep -c
  QAccessible` → **0**, against `vcl/inc/qt5/QtAccessibleWidget.hxx`'s six
  unconditional `<QtGui/QAccessible*>` includes.
* `python3 solenv/bin/native-code.py -g core -g draw` under the host's Python
  3.14.7 → exit 0, 1447 lines, 508 map entries.
* A `.component` scan over all 232 non-external files: **1189 implementations,
  1064 with `constructor=`**, and the 125 without are exactly the Java/Python
  loaders, tests, Windows/Base-only components, plus `i18npool` (74) and
  `svtools` (2) — which are precisely the two entries hardcoded in
  `native-code.py`'s factory list.
* `share/config/soffice.cfg` size breakdown (1650 of 1656 manifest entries
  resolved to sources and summed) and `icon-themes/colibre` zipped.
* `df -h` → 309 GB free; `sysctl` → 10 cores, 32 GB.
* **`store` and `registry` cross-build untouched.** `./build-lo.sh
  store.allbuild` and `./build-lo.sh registry.allbuild`, each from the freshly
  patched tree: `grep -c 'error:'` → **0** on both logs, and `grep 'warning:'`
  filtered of the build-side noise → **0** on both. `git -C src status
  --porcelain` afterwards lists exactly the 22 modified and 4 untracked paths
  the eight patches describe and nothing else, so this milestone added nothing
  to `src/`. Artefacts and member lists are in the M2¾ row.
* **The on-disk `store` format is word-size independent.** Thirteen page
  structs dumped with `-Xclang -fdump-record-layouts` under
  `--target=wasm32-wasip2 -I build/config_host` and under native
  `/usr/bin/clang++ -I build/config_build`; `diff` of the two → identical.
  This is the fact that says the native build-side tools' `services.rdb` and
  `types.rdb` are readable by the wasm32 binary. See decision 18, including the
  `config_host`-vs-`config_build` mistake that makes the check look like it
  fails when it does not.
* **`mmap` of a real file works under wasmtime on wasip2.** `MAP_SHARED` and
  `MAP_PRIVATE` with `PROT_READ` both return the file's bytes; a non-zero
  unaligned `offset` is honoured (offset 6 of `"hello store"` → `"store"`); a
  length past EOF zero-fills. It is `libwasi-emulated-mman.a`, whose
  `mman.c.obj` undefines only `malloc`, `free`, `pread` and `errno` — so the
  "mapping" is a copy. `llvm-nm` finds **no `mmap` in `libc.a` at all**; the
  emulation archive is the only provider, and `core-0002` already links it.
* **`fcntl(fd, F_SETLK, &flock)` returns `-1` with `errno` 58 (`ENOTSUP`) under
  wasmtime.** Harmless today only because `osl_file_queryLocking` is gated on
  `SAL_ENABLE_FILE_LOCKING`; see decision 18 for why that variable must never
  be set in the runtime image.
* **`getcwd` returns `"/"` (non-`NULL`) under wasmtime, `PATH_MAX` is 4096.**
  Which makes `osl_getProcessWorkingDir` — reached from `store`'s
  relative-filename branch — return `osl_Process_E_None` and `file:///`.
* **`unoidl` cross-compiles untouched, and a wasm32 binary then READ an `.rdb`
  written by the native arm64 build-side tool.** `./build-lo.sh unoidl.allbuild`
  → `[build MOD] unoidl`, `grep -c 'error:'` = 0, first attempt.
  `build/instdir/program/libunoidllo.a` = **983,308 bytes / 7 members**,
  `llvm-objdump -h` → `file format wasm`. Then: three one-entity `.idl` files
  → `instdir_for_build/LibreOfficeDev26.2_SDK/bin/unoidl-write` (arm64) → a
  347-byte `probe.rdb` starting `55 4e 4f 49 44 4c ff 00` → a `wasm32-wasip2`
  executable linked from `libunoidllo.a + libreglo.a + libstorelo.a +
  libuno_salhelpergcc3.a + libuno_sal.a + libzlib.a` and run under
  `wasmtime --dir=.::/`, which printed the five struct members with their types,
  `MaxSlides = 305419896`, `HugeValue = 1311768467463790320`,
  `Ratio = 0.1562500000` and `RED/GREEN/BLUE = 1/2/4`, exit 0.
  **The first LibreOffice code this port has executed.**
* **`-pthread` on the wasm COMPILE line breaks every `catch`, and the bisect is
  reproducible in three commands.** Running the above with a *relative* URI
  takes `Manager::loadProvider`'s `catch (FileFormatException &)` fallback and
  trapped: `memory fault at wasm address 0x12f4678 in linear memory of size
  0x980000`, exit 134, with `loadProvider` the only LibreOffice frame. A
  verbatim copy of `loadProvider` compiled in a separate TU against the *same*
  archives ran the fallback correctly — so the difference was the flags, not the
  code. Recompiling that copy with `-pthread` added and nothing else reproduced
  the identical fault. Putting `-pthread` on the link line too turns the same
  bug into a diagnosable error: `relocation R_WASM_MEMORY_ADDR_TLS_SLEB cannot
  be used against non-TLS symbol '__wasm_lpad_context'`, plus `--shared-memory
  is disallowed by Unwind-wasm.c.o`. After the `core-0002` fix and a full
  rebuild, the same binary reports `FileFormatException on probe.rdb: cannot
  open legacy file: 6` and exits 1. See decision 16.
* **`make verbose=t` is the only way to see a compile flag in this build**, and
  it is worth the two minutes. It is what showed `-pthread` sitting between
  `-std=c++20` and `-fwasm-exceptions` on the wasm command line, and what showed
  that the first attempted fix (`gb_CXXFLAGS := $(filter-out …)`) had changed
  nothing, because `unxgcc.mk:107` had already copied the value into
  `gb_LinkTarget_CXXFLAGS`.
* **gbuild does not rebuild when a platform makefile changes.** After editing
  `WASI_INTEL_GCC.mk`, `./build-lo.sh unoidl.allbuild` compiled nothing and
  exited 0. The five modules had to be forced with `rm -rf
  build/workdir/{CxxObject,GenCxxObject,LinkTarget,Module}` and their `Dep/`
  twins; `workdir_for_build` is a separate tree and was left alone, so the
  native bootstrap did not have to be repeated.
* **Six LibreOffice archives linked into one wasm component with no group
  flags** — `wasm-component-ld` resolved `unoidl → reg → store → salhelper →
  sal → zlib` in a single left-to-right pass, 4,099,070 bytes out. That is not
  E1 (the real graph is ~200 archives and genuinely circular; this one is a
  chain), but it is the first evidence in that direction and the first time
  LibreOffice archives have been through the component linker at all.
* **`sysconf(_SC_PAGESIZE)` returns 65536 under wasmtime** (`_SC_NPROCESSORS_ONLN`
  returns 1). Non-zero, so `osl_mapFile`'s `osl_File_MapFlag_RandomAccess`
  page-touch loop terminates instead of spinning; and `FileHandle_Impl`'s read
  buffer is a 64 KiB `calloc` per open file handle rather than 4 KiB.
* **`cppu` runs on wasm32-wasip2.** Eight checks, exit 0, under wasmtime: the
  static types; `XInterface`'s type description completing with 3 members out of
  `static_types.cxx` and no registry; `Any` round-tripping `long`, `string`,
  `hyper` and `double` plus copy-compare; `Sequence` copy-on-write, `realloc`
  and `Any` round-trip; `uno_getEnvironment("uno")`; `uno_getMapping(env, env)`
  → the identity mapping; and a `css::uno::RuntimeException` thrown and caught
  **by its base class** with `Message` intact.
* **`getentropy()` works on wasip2 and caps at 256 bytes per call** — 16 bytes
  succeeds, 300 fails with `EIO` (29). `fopen("/dev/urandom")` returns `NULL`.
  Both matter because `rtl_random_getBytes` `abort()`s rather than degrading
  when `osl_get_system_random_data` says no (decision 20).
* **A synthesized `type_info` can never be caught on wasi-sdk 34.** A
  hand-built `__si_class_type_info` with the byte-identical mangled name
  compares unequal to the real `typeid` (`_LIBCPP_TYPEINFO_COMPARISON_IMPLEMENTATION`
  defaults to 1, address comparison, on every non-COFF non-arm64-Apple target),
  and `__cxa_throw` with it falls through `catch (Derived&)` **and**
  `catch (Base&)` into `catch (...)`. This is what disqualifies the obvious
  one-line "fix" for the UNO bridge; see blocker 4.
* **A weak `extern "C"` reference to a `_ZTI…` symbol does the right thing in
  both directions on wasm32-wasip2** — it binds to the real RTTI address when
  the type is in the image (and a throw with it is caught by the real base
  class), and links to `nullptr` when it is not, without dragging anything out
  of an archive. That is the mechanism a WASI replacement for
  `jsGetExportedSymbol` needs, and it is the half of E4 that was in doubt.
* **`cppu`'s thread pool: create/destroy is clean, `putJob` aborts.**
  `uno_threadpool_create()` spawns nothing and `uno_threadpool_destroy()`
  returns normally; one `uno_threadpool_putJob` traps at
  `abort` ← `ORequestThread::launch()` ← `ThreadPool::createThread` ←
  `ThreadPool::addJob`, exit 134, every frame named. Only `binaryurp` and
  `jni_uno` reach it, so it is dead code here — see decision 20.

### What is asserted from reading only

Everything else in this document, including: the `--enable-cairo-rgba` byte
order (read in `CairoFormats.hxx` and in `cairo.GL_RGBA.patch`, **not** observed
in a rendered frame); the claim that svp draws the full application chrome
(a four-link chain through `salvtables.cxx:157`, `svpframe.cxx`,
`syswin.cxx:877` and `brdwin.cxx:1963`, plus the fact that LO's entire test
suite runs under `SAL_USE_VCLPLUGIN=svp`); every size estimate marked
"estimated"; the native-bootstrap time estimate **for a cold `ccache`** (the
warm number is now measured — see "Explicitly unknown"); and the whole
reachability analysis of the threading sites.

### Explicitly unknown

* Whether `wasm-ld` resolves LO's static archive graph without group flags.
  **E1** — narrowed, not answered. gbuild's own recipe now links a real wasm
  component (`osl_process_child`) from six archives with the group flags
  stripped by `toolwrap/wasi-clang++`, and it runs. Six archives in a chain is
  not the ~200-archive cycle of `static.mk:26-51`.
* Whether wk's wasmtime instantiates a component of LibreOffice's size, and
  what it costs in compile time and RSS. **E2.**
* How many condvar waits are reachable in practice. **M4.**
* Whether cairo, pixman, fontconfig, freetype, harfbuzz, ICU and boost actually
  cross-build under wasi-sdk 34 — LO builds all of them for Emscripten, which is
  strong evidence the recipes are cross-friendly, but none were compiled. **E5.**
* Whether ICU's `--with-cross-build` split works with
  `HOST_PLATFORM=wasm32-unknown-wasip2`; ICU's configure has explicit
  Emscripten handling upstream, WASI handling unchecked.
* Whether PCH can be used. `configure.ac:6777` hard-errors for Emscripten+PCH
  citing "missing Sj/Lj support with nEH in clang"; whether clang 23 with
  `-mllvm -wasm-enable-sjlj` can do PCH is untested — and PCH is worth a large
  fraction of LO's build time.
* Whether the bundled `config.sub` inside each of the 149 external tarballs
  accepts `wasm32-unknown-wasip2` as a `--host`.
* Whether `osl::Directory` enumeration (used for the
  `<$ORIGIN/services>*` glob) works over wk-vfs, and whether `realpath()` —
  called on every font path by `psp::normPath` — behaves with wk-vfs symlinks.
* ~~Whether `/dev/urandom` exists in the wk VFS; `sal/osl/unx/random.cxx` falls
  back to it.~~ **Resolved, and the question turned out to be the wrong one.**
  `fopen("/dev/urandom")` returns NULL under wasmtime — a component's
  filesystem is its preopens and there are no device nodes — so the fallback
  cannot be reached at all, whatever the wk VFS does. `core-0006` puts
  `osl_get_system_random_data` on `getentropy()` instead (decision 20).
* ~~Total download size of the 149 external tarballs. `make fetch` was not
  run.~~ **Resolved:** `make fetch` runs inside `build-host.sh` and has run
  many times. `tarballs/` holds **106 files, 542 MB** — not 149; the count in
  the earlier text was of `download.lst` entries, several of which this
  configuration never fetches.
* ~~Wall-clock for either build stage on this host. Not attempted, per the
  house rules.~~ **Partially resolved, on a warm `ccache` only:**
  `build-configure.sh` ≈ 2 min, `build-host.sh` **24.8 s** from deleted gbuild
  outputs, the six module builds **49 s** together. A **cold**-cache number for
  either stage is still unmeasured, and that is the number
  `build-host.sh`'s "20–40 minutes" header is guessing at.
