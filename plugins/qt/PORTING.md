# Qt 6.8.4 on wasm32-wasip2 — a real widget toolkit as a wk node

The goal: run unmodified Qt applications — Widgets first, Quick after — as wk
nodes, with each app's windows composited guest-side into the single RGBA8
surface wk gives a node. KDE Frameworks 6 is the eventual destination, which
is why the target is 6.8 **LTS** and not 6.9.

Four scripts, our own QPA plugin, a smoke app and a patch dir:

* `./build-host.sh` — native Qt 6.8.4 tools into `./host`. Qt cannot
  cross-compile without a `QT_HOST_PATH` of the *same version*, so we build it
  rather than borrow Homebrew's.
* `./build-qtbase.sh` — qtbase cross-compiled to wasm32-wasip2 into
  `./sysroot`, using `./wasip2.cmake`.
* `./build-qpa.sh` — the **wk QPA plugin** (`./qpa`) into
  `./sysroot/lib/libqwk.a`. Minutes, not hours.
* `./build-smoke.sh` — `./qt-smoke.wasm`, a Qt Widgets app as a wk node.
* `./wasip2.cmake` — the toolchain. Its header is the primary document for the
  EH/sjlj flag set and the platform strategy; this file does not repeat it.
* `./qpa/` — **our** code: `QWkIntegration`/`QWkScreen`/`QWkWindow`/
  `QWkBackingStore`/`QWkCompositor`/`QWkEventDispatcher`/`QWkInput`/
  `QWkKeyTranslator`/`QWkFontDatabase`/`QWkTheme`. Not a patch, not upstream.
* `./smoke/` — the test asset. `plugins/qt/qt-smoke.wasm`.
* `./patches/` — every change to upstream Qt, with a convention in
  `patches/README.md`. `src/` is a gitignored tarball extraction and is never
  edited in place.

`mise run build` does host-then-target; `mise run build-smoke` adds the plugin
and the app. The first two halves are long; run them detached and tail `./logs`.

---

## The strategy: a genuine WASI platform, not Qt's wasm-emscripten

This is the decision everything else follows from, so it is worth stating
plainly.

Qt already has a wasm port. It is keyed entirely to
`CMAKE_SYSTEM_NAME STREQUAL "Emscripten"` (`cmake/QtPlatformSupport.cmake:21`)
and it is **not** the port we want. Taking it would mean inheriting:

* ~4,000 lines of `emscripten::val` / `EM_ASM` / `emscripten/fetch.h` code in
  corelib, gui and network, plus a 7,012-line DOM platform plugin — none of
  which wasi-sdk can compile, because embind has no wasi-sdk equivalent;
* emcc-only link flags (`-s FETCH=1`, `-s STACK_SIZE=5MB`,
  `-s ALLOW_MEMORY_GROWTH`) injected onto the global `Platform` target and
  therefore onto every downstream app;
* a hard `FATAL_ERROR: Can't find an Emscripten SDK!`;
* a broken architecture config test (Qt rewrites the expected artifact suffix
  to `.wasm` when `WASM=1`; CMake under this toolchain emits no suffix);
* and the loss of the `minimal` and `offscreen` QPA plugins, which are
  explicitly excluded on WASM.

So instead we keep `CMAKE_SYSTEM_NAME=WASI` — which wasi-sdk's own toolchain
file already sets — and Qt's `WASM` stays 0. Qt then compiles its generic UNIX
corelib, the `minimal`/`offscreen` QPA plugins build for free, and the
architecture test passes unmodified.

The price is exactly one thing: CMake's `Platform/WASI.cmake` is `set(WASI 1)`
and nothing else, where `Emscripten-Initialize.cmake` also does `set(UNIX 1)`.
Qt gates its entire POSIX layer on `CONDITION UNIX`. `UNIX` cannot be set from
a toolchain file (CMake clears it afterwards — verified by probe: a project
configured with this toolchain prints `UNIX=[] WASI=[1] SYS=[WASI]`). Qt's own
idiom for precisely this is `cmake/platforms/Platform/Integrity.cmake`, a
one-line `set(UNIX 1)` that Qt finds because `QtAutoDetectHelpers.cmake:619`
PREPENDs `cmake/platforms` to `CMAKE_MODULE_PATH` before `project()`. We add
the WASI sibling as a patch. `build-qtbase.sh` refuses to configure without it
and says why.

In exchange for not inheriting the emscripten code, we have to plug the
wasi-libc gaps that Qt's generic UNIX backends assume: `sigaction`,
`getuid`/`geteuid`/`getpwuid`/`getgrgid`, `sys/wait.h`, `pwd.h`/`grp.h`,
`eventfd`, `ppoll`. That is a dozen one-line guards, not a rewrite.

## Decisions already made (do not relitigate)

1. **One surface.** Multiple Qt top-level windows are composited *guest-side*
   into the single wkgfx surface. wk is not being changed to give a node
   several surfaces. Structurally this is `QFbScreen`/`QFbWindow`/
   `QFbBackingStore` from `src/platformsupport/fbconvenience` — which already
   z-orders N windows into one `QImage` with damage tracking — rather than Qt
   6.8's `QWasmCompositor`, which despite the name composites nothing (each
   window owns its own DOM canvas).
2. **Widgets and Quick, both software.** Widgets is the priority. Quick uses
   `QT_QUICK_BACKEND=software` + `QSG_RENDER_LOOP=basic` and must never block
   Widgets progress.
3. **Qt 6.8.4 LTS**, because KF6 is the goal.
4. **wasip2 direct**, not wasip1 + the preview1 adapter. `wasm-component-ld`
   emits the component at link time, which is what lets CMake own the link —
   the same choice `plugins/netsurf` and `plugins/bun` made.
5. **No threads.** `FEATURE_thread=OFF`, explicitly. See the trap below.

## Traps this build already accounts for

* **The pthread stub trap.** wasi-libc *defines* `pthread_create` — as a
  two-instruction body returning `ENOTSUP`. So Qt's config test passes, the
  build links, and threads silently never run. `FEATURE_thread` is
  `AUTODETECT NOT WASM`, and we are not WASM, so it would come back ON by
  itself. It is forced OFF, along with `future`, `concurrent`, `cxx11_future`
  (whose test also wrongly passes — libc++'s `__config_site` declares
  `_LIBCPP_HAS_THREADS 1` for wasip2), `process`, `processenvironment` and
  `dbus`.
* **The EH trap.** wasmtime runs with `wasm_exceptions` (exnref) enabled and
  *rejects* wasi-sdk's default legacy EH encoding. It rejects at instantiate
  time, so one badly-compiled translation unit poisons the whole component and
  the error points nowhere near the offending file. Every object therefore
  gets `-fwasm-exceptions -mllvm -wasm-enable-sjlj -mllvm
  -wasm-use-legacy-eh=false`, links with `-lunwind -lsetjmp`, and never sees
  LTO. Verified end to end in this tree: a C file doing setjmp/longjmp and a
  C++ file doing throw/catch both linked with exactly these flags and printed
  their output under `wasmtime run -W exceptions`.
* **The wasm-opt trap.** clang runs `wasm-opt` as an optional post-link pass
  and the `wasm-opt` on PATH (here: `~/.cargo/bin/wasm-opt`) cannot parse
  exnref. Every cmake/ninja invocation runs under a PATH that omits it. Note
  that CMake bakes absolute compiler paths into `build.ninja`, so the PATH must
  be scrubbed for the *build* step too, not only for configure.
* **The host-headers trap.** `CMAKE_FIND_ROOT_PATH_MODE_{LIBRARY,INCLUDE,
  PACKAGE}=ONLY` plus `FEATURE_system_*=OFF` plus no pkg-config. Everything Qt
  needs — freetype, harfbuzz, libpng, libjpeg, zlib, pcre2, md4c,
  double-conversion — is bundled in `qtbase/src/3rdparty`.
* **The stack trap.** Qt's raster engine and QML's JS engine recurse deeply;
  the toolchain links with `-Wl,-z,stack-size=8388608`. Getting this wrong
  presents as a mystery trap, not as a stack-overflow message.

## Milestone ladder

**M0 — QtCore console node.** `libQt6Core.a` cross-built; a trivial
`QCoreApplication` + `QString`/`QFile`/`QTimer` program linked into a
wasip2 component that runs under wk and prints. Proves the platform strategy,
the UNIX corelib, the EH flags and the event loop's non-GUI path. This is where
every wasi-libc gap lives, so `build-qtbase.sh` builds `Core` as its own stage.

**M1 — QtGui offscreen render to PNG.** `libQt6Gui.a` + the `offscreen` QPA
plugin + bundled FreeType/HarfBuzz. A headless program that paints text and
shapes into a `QImage` and writes a PNG. Proves the font stack, the raster
engine and the image plugins, with no wk graphics involved at all — and it
produces an artifact you can *look at*.

**M2 — wk QPA plugin + Widgets.** The `wk` platform plugin
(`QWkIntegration`/`QWkScreen : QFbScreen`/`QWkWindow : QFbWindow`/
`QWkBackingStore : QFbBackingStore`/`QWkCompositor`) over `plugins/gfx-compat`,
plus a `QWkEventDispatcher : QAbstractEventDispatcherV2` that blocks in
`wkgfx_wait_frame` instead of `poll()`. Smoke order: a coloured `QRasterWindow`
→ resize → a `QPushButton` → `QMenu` + `QMessageBox` (multi-window compositing
and nested `exec()`) → typing in a `QLineEdit`.

**M3 — a real app.** `torrent-file-editor` v1.0.3: Widgets, three `.ui` files,
`QTreeView`/`QTableView` with custom models and delegates, SVG icons, modal
dialogs — and *zero* non-Qt dependencies. Needs `qtsvg` and `qt5compat`
cross-built as well. Wired to a BindMount carrying a `.torrent` in and out.

**M4 — Quick on the software backend.** `qtdeclarative` cross-built,
`QT_QUICK_BACKEND=software`, `QSG_RENDER_LOOP=basic`. Target app: Slate, a
pixel-art editor whose canvas is `QQuickPaintedItem` (so the software
adaptation genuinely renders it) with no non-Qt dependencies.

## Known gaps to fix along the way

The two **host**-side gaps that blocked the typing smoke test are now **fixed**
in `crates/client-local-ui/src/compositor/input.rs`, and the QPA's layout branch
is live: key events carry winit's resolved `text` (so `wkgfx_event.ch` is a real
character), and `map_key()` covers all 171 W3C codes the WIT enum defines rather
than 55. `qt_widgets_app_paints_through_the_wk_qpa` types a letter into a real
`QLineEdit` to keep it that way.

What is left on the input path is smaller and listed under "What does NOT work"
below: no IME, no clipboard bridge, and a Ctrl/Meta convention that depends on
the host telling the guest what it is (`WK_HOST_OS`).

The third gap, in `plugins/gfx-compat`, is **done** too:
`wkgfx_wait_frame_timeout()` exists and `wkgfx_wait_frame()` is now
`wkgfx_wait_frame_timeout(-1)`. See M2 below.

Smaller, noted and deferred: no cursor-shape control (the host draws the
pointer; `QWkScreen::cursor()` returns `nullptr`); `wkgfx_poll_event` drains by
fixed priority rather than chronologically, so key/click interleaving within a
frame is lost; `QDesktopServices::openUrl` compiles and does nothing.

---

## Current state

**Two real, unmodified third-party Qt applications run as wk nodes and render.**
Qt 6.8.4 cross-builds for wasm32-wasip2; the wk QPA plugin composites a Qt app's
windows into the single wk surface and feeds it real pointer input. Qt Widgets
and Qt Quick both work.

Everything in this section was **re-verified independently** after the fact —
artifacts listed on disk, builds re-run, every component executed on wk's real
runtime, frames dumped and looked at. Where a claim did not survive that, it is
marked below.

| milestone | state |
|---|---|
| M0 host Qt 6.8.4 (native tools) | **done** — `./host`, 184 MB |
| M1 qtbase → wasm32-wasip2 | **done** — `./sysroot`, 7 patches |
| M2 wk QPA plugin | **done** — `sysroot/lib/libqwk.a`, `qt-smoke.wasm` |
| M3 a real Widgets app | **done** — `plugins/qt-torrentfileeditor103` |
| M4 qtdeclarative + a real Quick app | **done** — `plugins/qt-slatepixelarteditor…` |
| M5 typing | **done** — the host fills `text`; a real `QLineEdit` takes a letter |
| M5 decorations, socket notifiers, IME, clipboard, real-UI use | **not started** |

### What was verified, by running it

* `plugins/qt/qt-smoke.wasm` (21,186,908 B) — wk-server test
  `plugin::tests::qt_widgets_app_paints_through_the_wk_qpa` passes:
  `qt-smoke frame: 3118 dark px, 782289 light px` and
  `real pointer click at (512, 426) reached the QPushButton`.
* **Typing, end to end.** The same test sends a `wasi:surface` key event with
  `text: Some("a")` at a focused `QLineEdit` and waits for the widget's own
  `textChanged` to print `EDIT 'a'`; it then sends a Cmd chord and a Ctrl chord
  and asserts neither typed its letter. Checked as a *negative control*, which is
  the only thing that makes it worth anything: patching the event back to
  `text: None` makes both this and `gfx_smoke_c_guest_paints_and_consumes_events`
  fail, so the assertions ride on `text` and not on the key code.
* `plugins/qt-torrentfileeditor103/torrent-file-editor.wasm` (22,654,387 B) —
  its harness runs the node on `PluginHost` and passes. The dumped frame is a
  complete Qt Widgets window: toolbar with PNG icons, four tabs, line edits, a
  combo box, a checkbox, group boxes, antialiased text, and every field filled
  in from the bencode (`name='wk-qt-demo'`, the 40-hex info hash, `3 MiB`,
  `files=2`). A real `wasi:surface` click on About opens the About dialog and
  `tops` goes 1 → 2 — so **multi-top-level compositing and a nested
  `QDialog::exec()` are now demonstrated**, not just claimed, together with SVG
  decoding and rich-text links.
* `plugins/qt-slatepixelarteditor…/slate.wasm` (41,820,255 B) — the harness
  paints a full Qt Quick UI on the software backend: menu bar, icon toolbar,
  both rulers with tick labels, the canvas, the Colour/Swatches/Layers panels,
  the status bar. A click on File opens the menu (second top-level window).
* `./build-qpa.sh` and `./build-smoke.sh` were re-run **from a deleted
  `build-target/qpa`, `build-target/smoke`, `libqwk.a`, `qt-smoke.wasm` and
  `smoke/fonts/`**: exit 0, zero `FAILED:` lines, `libqwk.a` byte-identical,
  `qt-smoke.wasm` the same size (one byte range differs — the build is
  reproducible, not bit-reproducible), and the test passes against the fresh
  binary.
* `cargo nextest run --workspace --no-fail-fast`: **260 run, 259 passed, 1
  failed, 3 skipped.** The one failure is
  `workspace::tests::repo_example_resolves_against_the_root_deps`
  (`assertion left == right failed: piano wired to shader, left: 0, right: 1`).
  It is **pre-existing and unrelated**: it reads `example/live-coding.wk`, which
  is modified in the working tree by an unrelated earlier edit that deleted the
  `midi` line the test asserts on. No file under `example/` was touched by this
  work.
* `git status`: nothing fetched or derived is staged for commit. A `git add -An`
  dry run over all four touched directories lists only scripts, `patches/`,
  `qpa/`, `smoke/`, harnesses, docs and three checked-in PNGs. Tarballs, `src/`,
  `host/`, `build-*/`, `sysroot/`, `logs/`, `*.wasm`, harness `target/` and the
  staged fonts are all ignored.

### What does NOT work

* **No IME, so no dead keys and no CJK composition.** Typing itself works now
  (the host fills `text`, `QWkKeyTranslator` prefers it, and the smoke test
  types into a `QLineEdit`), but every character has to come from one key
  event: the compositor has no `WindowEvent::Ime` arm and never calls
  `set_ime_allowed`, so `QPlatformInputContext` has nothing to drive.
  `wkgfx_event.ch` is also a single scalar, so the accent-plus-letter string
  winit produces for an uncombinable dead key on Windows loses its letter (see
  `plugins/gfx-compat/wkgfx.h`).
* **No clipboard.** `Cmd+V` reaches Qt as a `QKeySequence::Paste` and Qt reads
  its *own* clipboard, which nothing ever fills: the host's clipboard is not
  bridged into a guest. Copying out has the same gap in reverse.
* **The Ctrl/Meta convention depends on the host saying what it is.** A macOS
  host sends Command as `meta`, and Qt's Mac convention wants that to be
  `Qt::ControlModifier` so `Cmd+C` is Copy. The sandbox cannot see the host, so
  wk sets `WK_HOST_OS` on every node and `QWkKeyTranslator::macModifiers()`
  reads it (`QT_WK_MAC_MODIFIERS=1/0` overrides by hand). Run a node against a
  host that does not set it and the shortcuts land on the wrong key — the
  *characters* are safe either way, since `translate()` drops the text for
  whichever chord `QInputControl`'s exact-`ControlModifier` guard would miss.
* **Nothing has ever run in the real `wk` UI.** Every proof so far is headless,
  with the harness pacing frames itself. Vsync pacing, hover states, drag grabs
  and the modifier swap are all unobserved.
* **Qt Quick popups with a Material `layer.effect` render with no background.**
  Confirmed by experiment, not inferred: Slate's File menu composites and takes
  input, but the ruler shows through it, because
  `Controls/Material/Menu.qml` puts `layer.effect: RoundedElevationEffect` on
  the background and that ends at a `ShaderEffect` the software adaptation
  cannot run — so the *whole* layered background disappears, Rectangle and
  shadow together. Re-running the same binary with
  `QT_QUICK_CONTROLS_STYLE=Basic` gives an opaque menu. This affects every
  elevated Material popup, not just menus.
  (Note the harness takes `[seconds]` as its third positional argument, so the
  env override must be passed *after* one: `slate-harness slate.wasm out.ppm
  600 QT_QUICK_CONTROLS_STYLE=Basic`. Passing it third is silently swallowed.)
* **Slate's saturation/lightness picker is blank** — a `ShaderEffect`, disclosed
  up front.
* **No socket notifiers.** `registerSocketNotifier()` warns once and no-ops, so
  async socket I/O would hang. The design routes everything through one `poll()`
  precisely so this is cheap to add; do not paper over it with a polling QTimer.
* **No window decorations.** `QT_WK_WINDOW_MODE=windows` sets
  `DontForceFirstWindowToFullScreen` but there is no chrome to drag yet. The
  default (one app filling the surface, popups floating above) is what linuxfb
  and vnc ship and is sufficient for all of Widgets.
* **No cursor shape.** `QWkScreen::cursor()` returns nullptr and the host draws
  the pointer, so an I-beam over a `QLineEdit` is impossible until
  `wasi:surface` grows a cursor-shape call.
* **No threads.** `FEATURE_thread=OFF`, so anything on a `QThread` links and
  then never runs: torrent-file-editor's "create torrent from files" progress
  dialog sits at 0%, Slate's auto-swatch panel stays empty.
* **Surface resize is still untested.** `QWkScreen::handleResize` is wired to
  `WKGFX_RESIZE`, but no test drives it.
* **`wkgfx_poll_event` drains by fixed priority, not chronologically**, so
  key/click interleaving within one frame is lost.
* `QDesktopServices::openUrl` compiles and does nothing.
* **Nothing is committed.** `plugins/qt/`, `plugins/qt-torrentfileeditor103/`
  and `plugins/qt-slatepixelarteditor…/` are all untracked, and
  `crates/wk-server/src/plugin.rs`, `plugins/gfx-compat/` and `.gitignore` are
  modified in place.

### Sharp edges in the build layout

* **`mise run build-plugins` will now start an hours-long Qt build.** This
  plugin's `build` task self-skipped while `patches/` was empty; patches now
  exist, so the guard is gone. On this machine it is a fast no-op because
  `./host` and `./build-target` are populated, but on a fresh clone the
  repo-wide sweep will sit in qtbase for hours with no opt-out.
* **The three Qt plugins are a dependency chain across gitignored output.**
  `qt-torrentfileeditor103` and the Slate port both link against
  `plugins/qt/sysroot`, which is derived and not committed. Each checks and says
  so, but the ordering is not expressed to mise.
* **Two host Qt trees, 438 MB total.** `plugins/qt/host` is `FEATURE_gui=OFF`,
  and qtdeclarative builds *no Qt Quick at all* without `qsb`, which needs
  qtshadertools, which needs QtGui — so the Slate port builds a second native Qt
  with QtGui purely to obtain `qsb`. Folding the two together is the obvious
  cleanup.
* **The Slate harness's `Cargo.lock` is gitignored; the tfe harness's is
  committed.** `wk-server` depends on `wasi-graphics-context-wasmtime` by bare
  git URL with no rev, and that crate no longer exists at the repo's HEAD, so a
  fresh resolve fails. The Slate `mise run test` task copies the workspace lock
  in as a fallback; a fresh clone of the tfe harness works because its lock is
  tracked. Pick one convention.
* **The tfe harness panics if its `--dump` directory does not exist**
  (`write frame dump: NotFound`), and `mise run test` points it at the
  gitignored `./logs`. It needs a `create_dir_all`.

### Findings worth keeping

#### `UNIX` cannot be set from `cmake/platforms/Platform/WASI.cmake`

The Integrity idiom was tried first and does nothing: `EnableLanguage` resolves
`include(Platform/${CMAKE_SYSTEM_NAME})` against CMake's own `Modules` directory
in preference to `CMAKE_MODULE_PATH`, and CMake 4.4.2 ships
`Platform/WASI.cmake`. Integrity works only because CMake ships no
`Platform/Integrity.cmake`. `set(UNIX 1)` therefore lives in
`QtPlatformSupport.cmake`. The symptom was not "UNIX is unset" but
`private/qcore_unix_p.h file not found` in a dozen unrelated files — because the
`CONDITION UNIX` sources never join the target and syncqt therefore never copies
the header.

#### `qt_set01(WASI ...)` silently evaluates to 0

`qt_set01` does `if(${ARGN})`, which re-expands unquoted, and `if()` dereferences
a bare word naming a defined variable — and CMake's
`Platform/WASI-Initialize.cmake` has already done `set(WASI 1)`. So
`CMAKE_SYSTEM_NAME STREQUAL WASI` degrades to `"WASI" STREQUAL "1"`. Every other
platform in that file escapes this only because nothing defines a variable named
`Linux` or `QNX`. WASI is now set with a directly quoted `if()`. Both findings
are written up at length in `patches/qtbase-0001-wasi-platform.patch`.

#### wasi-libc's headers are not a contract

Three bugs — `eventfd`, `flock`, `mremap` — share one shape: the header declares
the function and defines its feature macros, and no library in the SDK defines
the symbol. `__has_include`, `#ifdef LOCK_EX` and `#if defined(MREMAP_MAYMOVE)`
all say yes; configure passes, all of qtbase builds, and the failure appears only
when a real application is **linked**. A fourth of the same family bit at
*runtime*: `pipe()` exists but a stock wasip2 host returns `ENOTSUP`, and
upstream `qFatal()`s in `QEventDispatcherUNIXPrivate`'s constructor — so
`QApplication` could not be constructed at all. Patch 0003 makes the thread pipe
optional; it is pure thread-wakeup machinery and this build is
`FEATURE_thread=OFF`.

**Link and run a real executable before believing a change to this port works.**

#### `mmap` is `malloc` on wasi, and something will mask it back to a page

The qtdeclarative counterpart of the above, and the one to remember. Measured
under wasmtime: `sysconf(_SC_PAGESIZE)` is 65536 but `mmap(65536)` returns
`0x11498` — not page-aligned. QtQml's `qv4persistent.cpp` recovers a page header
from any `Value` by masking to a page boundary, so the mask lands on an unrelated
address: Slate loaded all its QML, started evaluating bindings, and died in the
GC with `memory fault at wasm address 0xffff00fc`. A debug build would have
asserted; release silently corrupts. Fixed by making the wasi `OSAllocator` use
`aligned_alloc(65536, …)`. `grep -rn 'pageSize() - 1'` is the search for the next
one.

#### Three QPA gotchas

* **`QFreetypeFace::getFace()` branches on the FILENAME first and ignores the
  font data you hand it** (`qfontengine_ft.cpp`). Registering a font from a Qt
  resource with a bare basename gives `families=1`, correct advance widths and a
  perfectly laid-out window in which **every glyph is a hollow box** —
  `QFontEngineBox` — plus a quiet `QFontEngineFT: Failed to create FreeType font
  engine` per style. The second argument to `addTTFile()` has to be the full
  `:/fonts/...` path.
* **`handleScreenAdded()` snapshots the screen's geometry.** Setting `mGeometry`
  in `initialize()` instead of the constructor left `QScreen::geometry()`
  reporting `0x0` forever while `QPlatformScreen::geometry()` was correct — and
  windows still came out the right size, because `QFbWindow::setVisible` asks the
  platform screen. It would have surfaced much later as broken popup positioning.
* **A frame credit, not a frame callback.** `processEvents()` presents only when
  it is holding a credit taken from `wkgfx_wait_frame_timeout()`. Pending update
  requests are deliberately NOT a reason to skip the block: the next host frame
  is when they are supposed to be delivered, and treating them as work-to-do
  busy-loops the guest.

#### The plugin is out of tree on purpose

`./qpa` is compiled against the installed `./sysroot`, not added to
`qtbase/src/plugins/platforms`. That works because `Qt6FbSupportPrivate` is a
findable package there — fbconvenience is built for every platform
unconditionally — and it means an edit to our own code costs a 20-second rebuild
instead of a qtbase reconfigure. `patches/` is for changes to upstream Qt; this
is not upstream Qt. The price is that Qt's plugin glue does not write the
`Q_IMPORT_PLUGIN` for us, so every app must (see `smoke/main.cpp`).

Structurally it is fbconvenience for the compositing and Qt's `wasm` plugin for
the update-request scheduling. `QFbScreen` already z-orders N windows into one
`QImage` with damage tracking, which IS decision #1; `QWkCompositor` only pairs
`QWindow::requestUpdate()` with the host frame. The dispatcher is where the port
stops resembling emscripten's: `wkgfx_wait_frame_timeout()` is an ordinary
blocking call on the guest's own stack, so `QCoreApplication::exec()` and nested
`QDialog::exec()` need no inversion at all.

#### Building an app against this Qt

The toolchain pins `CMAKE_FIND_ROOT_PATH` to the wasi-sysroot so no host library
can leak in, so an app must add the Qt sysroot to it *and* to
`CMAKE_PREFIX_PATH`:

```
cmake -DCMAKE_TOOLCHAIN_FILE=plugins/qt/wasip2.cmake \
      -DWASI_SDK_PREFIX=$WASI_SDK \
      -DCMAKE_FIND_ROOT_PATH=plugins/qt/sysroot \
      -DCMAKE_PREFIX_PATH=plugins/qt/sysroot \
      -DQT_HOST_PATH=plugins/qt/host ...
```

`wasip2.cmake` uses `list(APPEND CMAKE_FIND_ROOT_PATH ...)` rather than `set()`
for exactly this: a plain `set()` defines a *normal* variable that silently
shadows the caller's `-D` cache entry, and the symptom is `find_package(Qt6)`
failing with "Could not find a package configuration file" while `Qt6Config.cmake`
sits right there.

Fonts are not optional and not shipped — Qt says so itself. Without a font
directory the app runs, `QFontDatabase` is empty and text is invisible. Each
node here compiles one TTF in as a Qt resource; `QT_QPA_FONTDIR` still works for
a node that mounts a real font directory.

#### gfx-compat grew a timeout

`wkgfx_wait_frame()` was a timeout-free block on a single pollable, which cannot
honour a `QTimer` deadline. `wkgfx_wait_frame_timeout(int64_t ns)` now polls the
frame pollable together with a `wasi:clocks` deadline pollable through
`wasi:io/poll`'s multi-pollable `poll`; the old function is
`wkgfx_wait_frame_timeout(-1)`. This added `wasi:clocks/monotonic-clock@0.2.3`
to the `wkgfx` world, which unifies with the `@0.2.12` wasi-libc already imports.
Verified non-breaking: `plugins/gfx-smoke` rebuilt against the new world and its
wk-server test still passes. That same `poll` list is where socket notifiers go
when they are needed.
