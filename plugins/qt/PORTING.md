# Qt 6.8.4 on wasm32-wasip2 — a real widget toolkit as a wk node

The goal: run unmodified Qt applications — Widgets first, Quick after — as wk
nodes, with each app's windows composited guest-side into the single RGBA8
surface wk gives a node. KDE Frameworks 6 is the eventual destination, which
is why the target is 6.8 **LTS** and not 6.9.

Six scripts, our own QPA plugin, three test assets and a patch dir:

* `./build-host.sh` — native Qt 6.8.4 tools into `./host`. Qt cannot
  cross-compile without a `QT_HOST_PATH` of the *same version*, so we build it
  rather than borrow Homebrew's.
* `./build-qtbase.sh` — qtbase cross-compiled to wasm32-wasip2 into
  `./sysroot`, using `./wasip2.cmake`.
* `./build-qpa.sh` — the **wk QPA plugin** (`./qpa`) into
  `./sysroot/lib/libqwk.a`. Minutes, not hours.
* `./build-smoke.sh` — `./qt-smoke.wasm`, a Qt Widgets app as a wk node.
* `./build-net.sh` — `./qt-net.wasm`, the `QSocketNotifier` asset: a Qt node
  with no window and no timer, so only an fd can wake its loop. Links no
  QtNetwork on purpose.
* `./build-qtnetwork.sh` — `./qt-qtnetwork.wasm`, the QtNetwork asset:
  `QHostInfo` + `QTcpSocket` + `QNetworkAccessManager` against another wk node.
* `./wasip2.cmake` — the toolchain. Its header is the primary document for the
  EH/sjlj flag set and the platform strategy; this file does not repeat it.
* `./qpa/` — **our** code: `QWkIntegration`/`QWkScreen`/`QWkWindow`/
  `QWkBackingStore`/`QWkCompositor`/`QWkEventDispatcher`/`QWkInput`/
  `QWkKeyTranslator`/`QWkFontDatabase`/`QWkTheme`. Not a patch, not upstream.
* `./smoke/`, `./net/`, `./qtnetwork/` — the three test assets' sources.
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
`eventfd`, `mremap`, `flock`, and — once QtNetwork came on — `resolv.h`,
`sendmsg`/`recvmsg` and the whole `CMSG_*` layer. That is a couple of dozen
one-line guards plus two small shims, not a rewrite. (`ppoll` is **not** one of
them: wasi-sdk 34-rc.2 defines it, `QT_FEATURE_poll_ppoll` is 1, and that is
load-bearing — see [Socket notifiers](#socket-notifiers-the-frame-became-a-file-descriptor).)

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

The clipboard gap is **fixed** too, and it needed a whole new wk capability
rather than a QPA change: `wk:clipboard`, a WIT interface, a host
implementation over `arboard`, `plugins/clipboard-compat`, and
`qpa/qwkclipboard.cpp`. See "The clipboard" below.

What is left on the input path is smaller and listed under "What does NOT work"
below: no IME, and a Ctrl/Meta convention that depends on the host telling the
guest what it is (`WK_HOST_OS`).

The third gap, in `plugins/gfx-compat`, is **done** too:
`wkgfx_wait_frame_timeout()` exists and `wkgfx_wait_frame()` is now
`wkgfx_wait_frame_timeout(-1)`. See M2 below.

Smaller, noted and deferred: no cursor-shape control (the host draws the
pointer; `QWkScreen::cursor()` returns `nullptr`); `wkgfx_poll_event` drains by
fixed priority rather than chronologically, so key/click interleaving within a
frame is lost; `QDesktopServices::openUrl` compiles and does nothing.

---

## Current state

**Five real, unmodified third-party Qt applications run as wk nodes and render,
and one of them brings KDE Frameworks 6 with it.** Qt 6.8.4 cross-builds for
wasm32-wasip2; the wk QPA plugin composites a Qt app's windows into the single
wk surface and feeds it real pointer input, real key events, sockets and the
host clipboard. Qt Widgets and Qt Quick both work.

Everything in this section was **re-verified independently** after the fact —
artifacts listed on disk, builds re-run, every component executed on wk's real
runtime, frames dumped and looked at. Where a claim did not survive that, it is
marked below. The most recent pass reverted each new feature and watched its
test fail before restoring it; see *Negative controls actually run*.

| milestone | state |
|---|---|
| M0 host Qt 6.8.4 (native tools) | **done** — `./host`, 184 MB |
| M1 qtbase → wasm32-wasip2 | **done** — `./sysroot`, 9 patches (0008/0009 are QtNetwork) |
| M2 wk QPA plugin | **done** — `sysroot/lib/libqwk.a`, `qt-smoke.wasm` |
| M3 a real Widgets app | **done** — `plugins/qt-torrentfileeditor103` |
| M4 qtdeclarative + a real Quick app | **done** — `plugins/qt-slatepixelarteditor…` |
| M5 typing | **done** — the host fills `text`; a real `QLineEdit` takes a letter |
| M5 socket notifiers | **done** — `QSocketNotifier` over the fabric, `qt-net.wasm` |
| M5 clipboard | **done** — `wk:clipboard`, a wired + token-gated capability |
| M5 QtNetwork | **done for TCP+HTTP+DNS** — `QTcpSocket`, `QNetworkAccessManager` and `QDnsLookup` reach another wk node; **no TLS**, `QUdpSocket` unproven. See [QtNetwork](#qtnetwork) |
| M6 a compute-heavy Widgets app | **done** — `plugins/qt-qalculate` (Qalculate! 5.12.0 + GMP/MPFR/libxml2) |
| M6 a MIDI app on the fabric | **done** — `plugins/qt-drumstickvpiano2111` (Drumstick VPiano; MIDI reaches a *second* node) |
| M6 KDE Frameworks 6 | **yes** — `plugins/qt-kcalc`, 14 of 15 frameworks (**KCrash not ported**); opens a KXmlGuiWindow, computes through KNumber/GMP/MPFR, and paints the result (pixel-asserted) |
| M5 decorations, IME, real-UI use | **not started** |

#### Negative controls actually run

Each of these was performed by editing the source, rebuilding the affected
artifact, observing the failure, then restoring and re-observing the pass. A
test nobody has watched fail is not evidence.

* **Socket notifiers.** `registerSocketNotifier()` reverted to its old
  warn-once no-op, `./build-qpa.sh && ./build-net.sh` re-run →
  `qt_socket_notifier_wakes_on_the_fabric` hangs at `SOCKET WAITING` and fails
  on its 300 s deadline. Restored: `libqwk.a` came back **byte-identical**, and
  the test passes in 1.6 s.
* **The clipboard read gate.** The `clip_read` check in `clipboard.rs::get`
  disabled → the test fails on its *first* assertion with
  `a node with NO clipboard grant read the host clipboard; the token gate is a
  hole`. This is the one that matters: it proves the gate is load-bearing and
  not merely present.
* **The clipboard write path.** `set()`'s store into `outbox` removed → the
  test fails on the outbox assertion.

### What was verified, by running it

* `plugins/qt/qt-smoke.wasm` (21,199,783 B) — wk-server test
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
* **A socket readiness callback, end to end.** `plugins/qt/qt-net.wasm` — a
  `QGuiApplication` with **no window and no QTimer** — starts a non-blocking
  `connect()` to `plugins/netserve` on a shared fabric, addressed by node name,
  and then sits in `exec()` with nothing but a `QSocketNotifier`. The wk-server
  test `plugin::tests::qt_socket_notifier_wakes_on_the_fabric` **never pumps a
  frame**, so the fd is the only thing in the guest's world that can wake the
  dispatcher's single `ppoll`. It logs
  `SOCKET CONNECTING rc=-1 errno=26 | SOCKET WAITING | SOCKET CONNECTED |
  SOCKET SENT 18 | SOCKET READ 85 | SOCKET RECV 85 [hello from a wk node]` —
  `CONNECTED` can only come from a **Write** activation and `READ` only from a
  **Read** one. Checked as a *negative control*: forcing `includeNotifiers` to
  false in `pollOnce()` and rebuilding leaves the guest stuck at
  `SOCKET WAITING` until the test's deadline.
* **QtNetwork, end to end.** `plugins/qt/qt-qtnetwork.wasm` runs `QHostInfo`,
  `QTcpSocket` and `QNetworkAccessManager` against `plugins/netserve` on one
  shared Network, addressed by node name. The wk-server test
  `plugin::tests::qt_network_speaks_to_a_wk_node` passes:
  `TLS ABSENT | DNS OK 10.0.0.157 | TCP CONNECTED peer=10.0.0.157:8080 |
  TCP RECV 85 [hello from a wk node] | HTTP GET http://netserve:8080/ |
  HTTP STATUS 200 | HTTP RECV 21 [hello from a wk node] |
  TLS REJECTED 301 Protocol "https" is unknown`. Each stage is started from
  inside the previous one's callback, so the ordering asserts are evidence that
  the event loop delivered all four. The last one is a *negative* stage aimed
  at the **plaintext** peer, so a silent https→cleartext downgrade would have
  succeeded and failed the test. See [QtNetwork](#qtnetwork).
* `plugins/qt-torrentfileeditor103/torrent-file-editor.wasm` (22,666,495 B) —
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
* `cargo nextest run --workspace --no-fail-fast`: **265 run, 265 passed, 0
  failed, 3 skipped**, and `cargo clippy --workspace --all-targets` clean (the
  single warning is a pre-existing future-incompat notice about the transitive
  `block v0.1.6` crate). The five new tests over the 260 at `cb9a700` are
  `plugin::tests::qt_socket_notifier_wakes_on_the_fabric`,
  `plugin::tests::qt_app_copies_and_pastes_through_the_host_clipboard`,
  `plugin::tests::qt_network_speaks_to_a_wk_node`,
  `auth::tests::clipboard_needs_a_wire_and_splits_into_read_and_write` and
  `clipboard::tests::seq_advances_only_when_the_text_changes`.

  An earlier revision of this file recorded one failure here,
  `workspace::tests::repo_example_resolves_against_the_root_deps`, caused by a
  working-tree edit to `example/live-coding.wk` that deleted the `midi` line
  the test asserts on. **That file is now identical to `HEAD` again** and the
  test passes. Nothing in this work touched `example/` — the file's mtime
  (09:47) predates the first edit of this session — but the change is worth
  knowing about, because it means an uncommitted edit of the user's silently
  disappeared. The other seven `example/*` modifications are untouched.
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
* **The clipboard carries TEXT ONLY across the host, and
  `QClipboard::dataChanged()` does not fire for a copy made in ANOTHER
  application.** Both are deliberate; see "The clipboard" below for why, and
  for what still works despite them (in-node copy/paste of any format is
  lossless, and a paste always reads the current host clipboard).
* **`QWkClipboard` supports only `QClipboard::Clipboard`.** `supportsMode()`
  returns false for `Selection` and `FindBuffer`, so an app that sets either
  gets Qt's own `Data set on unsupported clipboard mode. QMimeData object will
  be deleted.` on stderr and the data is dropped. Observed live in
  `plugins/qt-qalculate`, which sets the X11 selection on every expression
  edit — harmless (there is one host clipboard, and no X11 selection to own)
  but noisy, and it means middle-click paste can never work.
* **An app port configured BEFORE the clipboard landed fails to relink, with a
  message that does not say so.** `libqwk.a` now always carries the
  `wk:clipboard` shim, so every app must also link
  `gen/wkclipboard_component_type.o`; a build directory whose CMake cache
  predates that has `WK_CLIP_COMPONENT_TYPE:FILEPATH=` empty, and each port's
  `build.sh` only re-runs `cmake` when `build.ninja` is missing. The result is
  `wasm-ld: error: libqwk.a(wkclipboard.c.obj): undefined symbol:
  __component_type_object_force_link_wkclipboard`. **Verified both ways** on
  `plugins/qt-torrentfileeditor103`: plain `./build.sh` fails exactly like
  that, `WK_TFE_RECONFIGURE=1 ./build.sh` succeeds and the resulting binary
  imports `wk:clipboard` and passes its harness. The fix belongs in each
  `build.sh` — reconfigure when the cached value is empty, or key the stamp on
  the flag set rather than on `build.ninja`.
* **`plugins/qt-slatepixelarteditor…/slate.wasm` has not been rebuilt or
  re-verified** against the current `libqwk.a`. Its `build.sh` and
  `node/CMakeLists.txt` were edited for the clipboard, but the shipped binary
  predates the socket-notifier dispatcher rewrite *and* the clipboard, so it
  exercises neither, and it will hit the reconfigure trap above on its next
  incremental build.
* ~~**KCalc computes but its display paints nothing.**~~ **FIXED, and the cause
  was a stale artifact — not KCalc, not the QPA.** The first build narrated
  `STATE input='1÷8' display='0.125'` truthfully while both display rectangles
  dumped pure `(255,255,255)` with zero glyph pixels. The `KColorScheme`
  hypothesis was wrong, and worth recording as wrong because it is the obvious
  one: instrumenting the widget showed healthy state throughout —
  `text=#232629` on `base=#ffffff`, `visible=1`, `1002x118`, resolved family
  `DejaVu Sans` at `px=33`, and `QFontMetricsF::horizontalAdvance("0.125") =
  94.4`, i.e. real glyphs for the exact string being drawn. Tracing the
  compositor showed `requestUpdateWindow → deliverUpdateRequests →
  onFrame damage=1` running every frame, so paints were being presented.
  Widening the flush damage to the whole window changed nothing.

  What fixed it was rebuilding `libqwk.a` and relinking: `kcalc.wasm` had been
  linked against an **older QPA**. No source changed. This is the same trap
  that left `plugins/qt-slate…` unrebuilt against the socket-notifier and
  clipboard work, and that made `qt-torrentfileeditor103` fail to relink — see
  *Sharp edges in the build layout*. **After any change to `plugins/qt/qpa`,
  relink every app port**; a stale one fails in ways that look like app bugs
  and cost hours.

  The harness now asserts on **pixels** as well as narration (`display pixels:
  expression=87 dark, result=881 dark`), because every assertion it had read
  the app's own narration and none of them could see a blank screen.
* **The clipboard reconciler itself has no end-to-end test.**
  `Server::sync_clipboard` is what actually turns a token plus a `Wire::
  Clipboard` into the two permits in production, and it runs every tick beside
  `sync_captures`/`sync_exec` — but the end-to-end test drives
  `node.clip_read`/`clip_write` directly, and `auth.rs` tests only the Datalog.
  Nothing covers the join between them. Not a hole (both halves are default-
  deny and both are tested), but the seam is unexercised.
* **The port harnesses do not create their `--dump` directory.** All three new
  ones panic with `write frame dump: No such file or directory` if the
  directory does not already exist; they only work because `mise run test`
  points them at a `logs/` that does. One `create_dir_all`.
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
* **No TLS, therefore no `https://`.** `QT_FEATURE_ssl` is 0 and nothing can
  turn it on today: SecureTransport is `CONDITION APPLE` (the *target* is not),
  Schannel is `WIN32`, and no OpenSSL is cross-built for wasm32-wasip2 here. So
  `QSslSocket` does not exist, `QNetworkAccessManager` refuses `https://` URLs
  outright, and every "TLS" answer in this port is *no*. See
  [QtNetwork](#qtnetwork) for what does work.
* **`QNetworkInterface` enumerates nothing, and `QUdpSocket` is unproven.**
  Both are `FEATURE_networkinterface=OFF` consequences and libc gaps; details
  in [QtNetwork](#qtnetwork).
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
* **Nothing is committed.** `plugins/qt/`, `plugins/qt-torrentfileeditor103/`,
  `plugins/qt-slatepixelarteditor…/`, `plugins/qt-qalculate/`,
  `plugins/qt-drumstickvpiano2111/`, `plugins/qt-kcalc/` and
  `plugins/clipboard-compat/` are all untracked, and `crates/wk-server/`,
  `crates/wk-protocol/`, `crates/client-local-ui/`, `plugins/gfx-compat/`,
  `Readme.md` and `.gitignore` are modified in place. A `git add -An` dry run
  over the whole tree lists 117 paths and **no** `.wasm`, `.a`, `.o`, tarball,
  `src/`, `build/`, `sysroot/` or `logs/` entry — the derived paths are all
  covered by `.gitignore`.

### The clipboard

`Cmd/Ctrl+C` and `Cmd/Ctrl+V` in a `QLineEdit` now reach the machine's real
clipboard. Nothing in the app changes: it calls `QClipboard`, which reaches
`QWkClipboard` only because `QWkIntegration::clipboard()` returns one. Before
that override existed, `QPlatformIntegration` handed back the DEFAULT
`QPlatformClipboard` — a process-global `QMimeData` holder — so copy and paste
worked perfectly *inside* one node and were invisible to the machine it ran on.

Fixing it needed a new wk capability, not a QPA change, because no WIT
interface reached the host clipboard at all. Four layers:

| layer | where |
|---|---|
| WIT | `crates/wk-server/wit-clipboard/world.wit` (`wk:clipboard`) |
| host | `crates/wk-server/src/clipboard.rs` + the pump in `client-local-ui`'s `compositor.rs` |
| guest shim | `plugins/clipboard-compat/wkclip.[ch]` |
| Qt | `qpa/qwkclipboard.[ch]pp` |

#### It is a granted capability, and that is the point

**A node gets nothing by default.** The host clipboard is a genuine
cross-sandbox side channel: `get` returns whatever the user last copied
*anywhere* — a password manager, a terminal, a bank site — and `set` lets a
node plant content for the user to paste somewhere else. So it is gated twice
over, and both gates are wk's existing machinery rather than anything new:

1. **A wire.** The app must be wired to a **Clipboard** node on the canvas
   (`wk create clipboard`, or "Add Clipboard" in the palette). That produces
   the `wired("clipboard", <node>)` fact the base token rule needs:
   `can_use($k,$t,$a) <- wired($k,$t), operation($k,$t,$a)`. Unwired derives
   nothing. There is deliberately **no** no-wire exception of the kind `scene`
   and `exec` carry — those grant a node no authority it does not already have,
   and this does.
2. **The token.** `read` and `write` are separate actions on that one wire, so
   a token can grant copy-out without paste-in:

   ```
   wk token attenuate qt 'check if operation($k,$t,$a), $k != "clipboard" || $a == "write"'
   wk token attenuate qt 'check if operation($k,$t,$a), $k != "clipboard"'
   ```

   `Server::sync_clipboard` re-derives both every tick and stores them into the
   node's two atomics, so attenuating a token blinds a *running* guest without
   restarting it. `crates/wk-server/src/auth.rs`'s
   `clipboard_needs_a_wire_and_splits_into_read_and_write` pins all of it.

A denied `get` returns `none`, which is the same answer as "the clipboard is
empty" and "this machine has no clipboard". That conflation is deliberate — a
sandbox must not be able to probe for a capability it was refused — and it is
why the host logs the first denied read per node instead: the refusal is
visible in `wk logs`, just not to the guest. The Clipboard node's own widget
shows the live grant (`● read + write`, `write only`, `denied by token`, `no
host clipboard`), which is half the security argument: the user can see at a
glance which node can read what they copied.

#### Text only, and no pollable

Both cuts come from what `arboard` (the host library, already in
`client-local-ui`) can actually do, not from laziness:

* **Text only.** `arboard` reads text or an RGBA image; there is no
  `get_html`. A `list<tuple<string, list<u8>>>` MIME map would be a two-entry
  map pretending to be general. Qt loses nothing in-process, because
  `QWkClipboard` follows **Haiku's** model rather than Qt's own wasm one: the
  `QMimeData` the app itself set is kept and handed back verbatim for as long
  as the app still owns the clipboard, so copying a pixel selection or rich
  text inside Slate and pasting it back round-trips every format. Only the hop
  *across the host* is text. Images can be added later as a separate
  `get-image`/`set-image` pair — `arboard`'s `image-data` feature is already on
  and its RGBA8+w+h shape is byte-for-byte `wk:capture`'s `frame`.
* **No `wasi:io/poll` pollable.** `arboard` has no change notification of any
  kind — no `changeCount`, no watcher — so a pollable would be a promise the
  host cannot keep. Change detection is a `u64 seq` that the host's pump
  increments only when it observes the text actually change, and the pump is a
  250 ms poll plus a forced re-read on `WindowEvent::Focused(true)`. On X11
  each read is a synchronous round trip to whichever process owns the
  selection, which is why it is throttled rather than run per frame.

The consequence for Qt is one real gap: **`QClipboard::dataChanged()` does not
fire when another application copies.** Qt learns about a foreign copy the next
time it asks — which is what a paste does, so paste is always current — but a
widget greying out its Paste button on that signal will not refresh on its own.
Closing it means polling the host from `QWkEventDispatcher`; it was left out on
purpose rather than overlooked.

#### `ownsMode()` without a signal

`QWkClipboard` answers "is this clipboard still mine?" from two facts, and both
halves are needed:

* the host is showing the text we wrote (the steady state once the pump ran), **or**
* the host's `seq` has not moved since we wrote. This covers the gap between
  `wkclip_set()` — which only queues into the host's outbox — and the client's
  next event-loop pass, and it covers a `setMimeData()` that carried no text at
  all, since an image copied inside the node leaves the host clipboard
  untouched and we do still own it.

The client's pump closes the loop from the other end: after it drains a guest's
write to `arboard`, it publishes that same string as the board's current text
itself, so the node's own copy never reads back a moment later as "somebody
else changed the clipboard".

#### Where it is proven

`plugin::tests::qt_app_copies_and_pastes_through_the_host_clipboard` runs the
real `qt-smoke.wasm` on wk's real runtime and walks seven claims in order: a
board in reach but no grant reads as empty; granting `read` makes
`QClipboard::text()` return what the host holds; with `write` still denied,
typing into the `QLineEdit` then `Cmd/Ctrl+A`+`Cmd/Ctrl+C` leaves Qt owning its
own clipboard while nothing reaches the machine; granting `write` and repeating
the chord puts that string in the board's outbox (which is exactly what the
client hands to `arboard::set_text`); publishing our own write back keeps
`ownsClipboard()` true; a foreign change takes ownership away; and revoking
`read` blinds the guest mid-run. The chord uses meta on macOS and ctrl
elsewhere for the same reason `QWkKeyTranslator` does — `Ctrl+A` is
`MoveToStartOfLine` under the Mac convention, and it would clear the very
selection the copy needs.

#### Build consequence: a second component-type object

`libqwk.a` now contains `QWkClipboard` and the `wk:clipboard` shim, and
`clipboard()` references it, so **every** app linking `libqwk.a` pulls the shim
out of the archive — whether or not it copies anything. wit-bindgen's output
deliberately references a symbol that lives only in
`gen/wkclipboard_component_type.o`, so omitting that object is a hard

    undefined symbol: __component_type_object_force_link_wkclipboard

at link, not a silently import-less component. It is on the link line of
`smoke/`, `net/`, `qt-torrentfileeditor103` (patch `0006`) and the Slate node
for that reason. Same rule as `wkgfx_component_type.o`: an OBJECT, never an
archive member.

`plugins/clipboard-compat` is its own shim rather than an addition to
`gfx-compat` because that world imports only `wasi:` packages — it is the WASI
graphics shim — and folding a `wk:`-custom import into it would make doom,
quake, netsurf, mupdf, paint, triangle and gfx-smoke each link a clipboard
import none of them uses, and grow a Clipboard port on the canvas to match.

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
wk-server test still passes.

#### Socket notifiers: the frame became a file descriptor

The obvious design — append each watched socket's `wasi:io/poll` pollable to the
list `wkgfx_wait_frame_timeout()` already builds — **is not possible**, and this
is worth writing down so nobody spends another day proving it again:

* the descriptor vtable's only pollable-producing hook is
  `poll_register(void *, poll_state_t *, short)`, and `poll_state_t` is an
  **incomplete type** whose definition lives inside wasi-libc's `ppoll.c`. There
  is no accessor and no supported way to reach the pollable from an `int` fd;
* the one nearly-public route, `get_read_stream()` → `subscribe()`, yields a
  pollable only for a **connected** socket. A connecting socket and a listening
  socket both fail it with `ENOTCONN` — i.e. exactly the two states
  `connectToHost()` and `listen()` need.

So the frame travels the other way instead. `plugins/gfx-compat/wkgfx_poll.c`
wraps the frame pollable in a descriptor of wk's own and inserts it into
wasi-libc's descriptor table, which makes it an ordinary fd
(`wkgfx_frame_fd()`). `QWkEventDispatcher` then blocks in `ppoll()` over
`[notifier fds…, frame fd]` — and on wasip2 **`ppoll()` IS
`wasi:io/poll.poll`**: libc's `ppoll_impl` asks each descriptor to register its
pollables, appends a `monotonic-clock.subscribe-duration` for the timeout, and
makes exactly one `poll` over the lot. Same single blocking call the design
always wanted, assembled by libc instead of by hand, and the sockets get in for
free through wasi-libc's own `tcp_poll_register` — which handles connecting,
listening and connected sockets alike and even completes `finish-connect` in its
`poll_finish`, so `QAbstractSocket`'s "connected" arrives as a plain
`POLLOUT` activation.

Consequences worth knowing:

* the dispatcher is now `QEventDispatcherUNIX`'s shape, deliberately: a
  `QHash<int, QSocketNotifierSetUNIX>`, a `QList<pollfd>` with the frame fd
  **last** where the thread pipe would be, one `qt_safe_poll`, then `revents`
  mapped back to notifiers. Level-triggered, like `poll(2)`;
* `wkgfx_wait_frame_timeout()` is no longer the dispatcher's blocking call. It
  survives for every other gfx-compat consumer, but **do not mix the two on one
  surface**: wk's frame readiness is one-shot and is consumed by whichever
  `poll` observes it. `wkgfx_take_frame()` is the other half for a caller that
  learned about the frame from `ppoll` — and it is not optional bookkeeping,
  because `get-frame` is where a *closed* surface is reported (by trapping,
  which is how the node exits);
* `POLLPRI` is a dead letter on this platform — WASI has no out-of-band data —
  so `QSocketNotifier::Exception` only ever fires on hangup or error. Nothing in
  Qt's own networking uses it;
* a bad descriptor is **not** `POLLNVAL` on one entry as it is on Linux: it
  fails the whole `ppoll`. Left alone that would freeze the UI, so the
  dispatcher probes and disables the offending notifiers instead of retrying;
* `wkgfx_poll.c` transcribes wasi-libc's **private** descriptor-table layout,
  exactly as `plugins/pipe-compat` does, so it is pinned to wasi-sdk-34-rc.2 and
  carries that guard. It is deliberately not part of `wkgfx.c`: doom, netsurf
  and every other gfx-compat consumer compile that file and none of them should
  inherit libc internals to draw a rectangle. The two libc symbols it calls,
  `__wasilibc_poll_add` and `__wasilibc_poll_ready`, are real exported symbols
  and take `poll_state_t *` opaquely — no private struct layout is involved in
  *that* half.

Qt's own Emscripten answer (`qeventdispatcher_wasm.cpp`: edge-triggered
`emscripten_set_socket_*_callback` hooks, a parallel sticky-state map, and a
separate `waitForSocketState()` blocking channel) exists because Emscripten
cannot block. wk can block on its own stack, so none of it is needed here.

### QtNetwork

`-DFEATURE_network=ON`. **`QTcpSocket` and `QNetworkAccessManager` both work
over wk's fabric; TLS does not exist.** Proven by the wk-server test
`plugin::tests::qt_network_speaks_to_a_wk_node`, which runs
`plugins/qt/qt-qtnetwork.wasm` against `plugins/netserve` on one shared Network
and logs:

```
NET START platform=wk peer=netserve:8080 | TLS ABSENT | DNS LOOKUP netserve |
DNS OK 10.0.0.157 | TCP CONNECTING | TCP CONNECTED peer=10.0.0.157:8080 |
TCP READ 85 | TCP RECV 85 [hello from a wk node] |
HTTP GET http://netserve:8080/ | HTTP STATUS 200 |
HTTP RECV 21 [hello from a wk node] | TLS GET https://netserve:8080/ |
TLS REJECTED 301 Protocol "https" is unknown
```

Four stages in ascending order of how much of Qt they drag in, so a failure
names its own layer, and each one is *started from inside the previous one's
callback* — which is what makes the ordering assertions evidence that the event
loop delivered all four rather than that four independent things happened.

| | Status |
|---|---|
| `QHostInfo` (peer by **node name**) | works |
| `QTcpSocket` / `QTcpServer` | works |
| `QNetworkAccessManager`, `http://` | works — with `patches/qtbase-0009` |
| TLS / `https://` / `QSslSocket` | **does not exist**, and cannot today — `https://` errors out, verified, no cleartext downgrade |
| `QUdpSocket` | links, **unproven**, ancillary data gone |
| `QNetworkInterface` | compiled out entirely |
| `QLocalSocket` / `QLocalServer` | off — wasi:sockets has no AF_UNIX |
| `QDnsLookup` | **works** — `patches/qtbase-0010` + `plugins/resolv-compat` |
| `QNetworkProxy`, cookies' `qIsEffectiveTLD()` | off |

#### Names resolve, and that was free

A wk node addresses its peers by **node name**, and `QHostInfo::lookupHost()`
answers them: `getaddrinfo()` → `wasi:sockets` `ip-name-lookup` → the fabric's
own resolver. Nothing Qt-specific was needed — `plugins/fetch/fetch.c` does the
identical call with no Qt at all. `FEATURE_libresolv` is off, so `QHostInfo`
takes its plain-`getaddrinfo` path rather than the `res_ninit` one, which is
exactly what we want. With `FEATURE_thread=OFF`, `qhostinfo.cpp`'s `QThreadPool`
branch compiles out and the lookup runs inline — but the *result* still comes
back as a posted event, so even stage one needs a working event loop.

#### `QDnsLookup` without threads, and a libresolv that did not exist

Two independent blockers, and it is worth separating them because only one is
about threads.

**The feature was thread-gated.** `qt_feature("dnslookup" ... CONDITION
QT_FEATURE_thread ...)`, because `QDnsLookupThreadPool` derives from
`QThreadPool` and `qthreadpool.cpp` is itself inside
`CONDITION QT_FEATURE_thread` — with threads off the base class does not exist,
so the file cannot link. `patches/qtbase-0010` drops the condition and runs the
runnable inline: **DirectConnection** for delivery (BlockingQueued on one
thread is a deadlock Qt asserts on, and Direct is what it degenerates to), and
a **queued** invocation so `lookup()` stays asynchronous. Same shape as the
`QNetworkAccessManager` fix above — correct single-threaded semantics, not an
approximation.

**There was no resolver.** wasi-libc ships `<arpa/nameser.h>` — every DNS
constant, type code, rcode enum, and the BIND `HEADER` struct — but nothing
that speaks to a nameserver, so no `<resolv.h>`. Upstream therefore selects
`qdnslookup_dummy.cpp`, whose entire body sets `ResolverError`. **This fails
silently**: it builds, links, and errors only at runtime.

`plugins/resolv-compat` is the answer, and the shape is the point: Qt only
borrows the *transport* from libresolv (`res_nmkquery`, `res_nsend`) plus
`dn_expand`; `qdnslookup_unix.cpp` parses every record type itself. Supplying
those four functions over ordinary BSD sockets — which already reach the fabric
— makes that upstream file build **unmodified**. `qhostinfo_unix.cpp`
additionally wants the BIND-era global `_res`/`res_init()` for
`QHostInfo::localDomainName()`, so the shim provides those too.

Nothing in it is wasm-specific, which is deliberate: `./build.sh --native`
builds it for the host, and its DNS logic was exercised against a real resolver
(A/MX/TXT/NS, compression pointers, the TCP-on-truncation retry) before ever
being pointed at the fabric.

**Two traps this cost, both worth knowing before adding any other native
dependency to this port.**

*CMake finds nothing under a WASI find-root.* `find_library`/`find_path`/
`find_package` combine each `CMAKE_FIND_ROOT_PATH` entry with the platform's
relative prefixes from `CMAKE_SYSTEM_PREFIX_PATH` — and CMake's
`Platform/WASI.cmake` is a stub that seeds none, so there are no combinations
to try and every find fails no matter what is on the root path. `wasip2.cmake`
now appends `"/"`. Same class of gap as `UNIX` not being set.

*A forced feature does not fail loudly.* `-DFEATURE_libresolv=ON` with an unmet
condition does not error; Qt prints `Resetting 'FEATURE_libresolv' from 'ON' to
'OFF' because it doesn't meet its condition` in the middle of thousands of
configure lines and carries on with the dummy backend. If `QDnsLookup` ever
starts returning `ResolverError`, grep the configure log for that line first.

*And a consequence:* turning the feature on makes **WrapResolv a recorded
third-party dependency of the Qt6Network package**, so every downstream app
doing `find_package(Qt6 COMPONENTS Network)` re-resolves it at its own
configure time. `wasip2.cmake` therefore wires `resolv-compat` in by default
rather than each build script doing it — otherwise an app port fails with
`Qt6Network could not be found because dependency WrapResolv could not be
found`, which names nothing about DNS.

**One-time upgrade step for existing build trees.** An app port configured
*before* this change has `CMAKE_CXX_FLAGS` cached without the `resolv-compat`
`-I`, and `*_FLAGS_INIT` only seeds a cache on its FIRST configure — so it
re-runs, fails to find WrapResolv, and reports
`Qt6Network could not be found because dependency WrapResolv could not be found`
with nothing pointing at the cause. Delete that port's build directory (or its
`CMakeCache.txt`) once; a fresh clone never sees this. The already-built
`.wasm` files are unaffected — the qtbase rebuild that came with this change
altered no ABI, and all of them still pass their tests — so this is a
configure-time upgrade step, not a reason to relink everything.

**Tested hermetically.** `plugins/dnsstub` is a ~180-line authoritative server
for `wk.test` and nothing else, wired onto the same Network, so every asserted
field is one this repo wrote — no internet, no third-party records. The test
asserts `DNSREC MX mail.wk.test pref=10`: the exchange proves `dn_expand`
walked the name, the preference proves the MX RDATA was read at the right
offset. Changing the stub's preference to 20 fails the test, which is how that
was confirmed to be load-bearing rather than decorative.

#### `QNetworkAccessManager` without threads

Upstream, `qt_feature("http" ... CONDITION QT_FEATURE_thread)` deletes the
entire HTTP stack from `libQt6Network` when threads are off, because Qt 6.8 runs
`QHttpThreadDelegate` on a `QThread` the manager creates. Qt's own wasm build
dodges this by handing the job to the browser's `fetch()`
(`qnetworkreplywasmimpl.cpp`, `CONDITION WASM`); we are not WASM and have no
browser.

`patches/qtbase-0009` drops the condition and fixes the three assumptions
behind it. The load-bearing observation is that **with `QT_CONFIG(thread)` off,
running the delegate on the calling thread is *correct*, not approximate**:

* `QNetworkAccessManagerPrivate::createThread()` returns
  `QThread::currentThread()`, so `delegate->moveToThread()` short-circuits to
  "already in this thread". (Left alone it would move the delegate onto a
  `QThread` that, without threads, is an object with its **own `QThreadData`,
  no event dispatcher and nothing draining its post-event queue** — every
  `QueuedConnection` to the delegate would vanish silently.)
* The delegate's `QueuedConnection`s are then delivered by the application's own
  event loop, in order.
* Its `BlockingQueuedConnection`s become **direct calls**, because
  `qobject.cpp:4094` wraps that whole arm of `QMetaObject::activate()` in
  `#if QT_CONFIG(thread)` and it falls through to the direct path. Which is what
  "block until the other thread answers" degenerates to when there is no other
  thread.
* `QHttpThreadDelegate::connections` is a `QThreadStorage`, whose out-of-line
  half (`qthreadstorage.cpp`) is compiled only `CONDITION QT_FEATURE_thread` —
  header compiles, **application link** fails on `QThreadStorageData`. With one
  thread, thread-local is global; it is a three-method struct over a static now.

HTTP/1.1 cleartext. h2c is not attempted (`isH2cAllowed()` is false by default,
so `startRequest()` falls back to `ConnectionTypeHTTP`).

#### TLS: no, and not "not yet configured" — and it does not downgrade

`QT_FEATURE_ssl` is **0** and no flag turns it on. SecureTransport is
`CONDITION APPLE` — the *target* is wasm32-wasip2, so this is false even though
the build host is a Mac — Schannel is `WIN32`, and there is no OpenSSL
cross-built for this target in the tree. `build-qtbase.sh` pins
`FEATURE_ssl/dtls/ocsp/openssl*` off explicitly rather than leaving them
derived, so that the day someone cross-builds OpenSSL those are the lines they
delete on purpose.

The question that actually matters is not "is TLS missing" — the build says so
— but whether `https://` then **silently downgrades to cleartext**, which would
be a security hole rather than a missing feature. It does not, and that is
checked by running it rather than by reading: the guest aims an `https://` URL
at the **plaintext** peer, so a downgrade would *succeed*, and instead it gets

```
TLS REJECTED 301 Protocol "https" is unknown
```

`301` is `QNetworkReply::ProtocolUnknownError`.
`qnetworkaccessmanager.cpp:1290` lists `u"https"` in its httpScheme table only
`#ifndef QT_NO_SSL`, so with `QT_FEATURE_ssl` 0 the URL never reaches the HTTP
backend at all — it falls through to the generic backend factory, which has
`file`, `data` and `qrc` and nothing else. The test also asserts the guest
printed `TLS ABSENT`, so that a future TLS backend breaks the test loudly
instead of leaving this document overclaiming in reverse.

#### The libc gaps QtNetwork walks into

Only two, both in `patches/qtbase-0008`, and neither is what the feature flags
suggest:

* **`<resolv.h>` does not exist.** `qnet_unix_p.h` includes it unconditionally
  for everything that is not VxWorks, so *every* file including that header
  dies at the `#include` — which also masks all the errors below until it is
  fixed. Nothing in the header uses the resolver.
* **`sendmsg`, `recvmsg`, `struct cmsghdr`, every `CMSG_*`, `SO_BROADCAST`,
  `SO_OOBINLINE` and `MSG_EOR` are all absent** — wasi-libc hides them behind
  `#ifdef __wasilibc_unmodified_upstream` with the comment "WASI has no
  sendmsg/recvmsg". Note this is *not* the eventfd/flock/mremap trap: they are
  not declared either, so it is a compile error, not a link error.
  **`-DFEATURE_udpsocket=OFF` does not avoid it**: `nativeSendDatagram()`,
  `nativeReceiveDatagram()` and `nativePendingDatagramSize()` sit **outside**
  `QT_CONFIG(udpsocket)` — the feature gates `QUdpSocket`, not the engine.

`QUdpSocket` therefore links, and is **unproven**: nothing drives it, ancillary
data is dropped (no `IP_PKTINFO` destination address, no hop limit, no
`MSG_TRUNC`), and `SO_BROADCAST`/`SO_OOBINLINE` will be refused by wasi-libc's
`setsockopt` at runtime. If UDP is wanted, drive it against `plugins/udpecho`
before believing it — that is a one-afternoon job and this port's whole history
says do not skip it.

#### Features forced off, and why each one

`build-qtbase.sh` carries the list with a reason per line. The pattern behind
them: **Qt writes half of QtNetwork's `CONDITION`s as `NOT WASM`**, so under a
genuine `WASI` platform they all autodetect back ON against a libc that cannot
honour them. `networkinterface` is the one that bites first (`<net/if.h>` is
absent from the sysroot entirely, and `getifaddrs`/`if_nametoindex` are
declared-but-undefined), and it is the one the old `FEATURE_network=OFF` comment
already warned about.
