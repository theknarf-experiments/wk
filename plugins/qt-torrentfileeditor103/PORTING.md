# torrent-file-editor 1.0.3 as a wk node

A **real, unmodified Qt 6 Widgets application** — 6,838 lines of C++, three
`.ui` files through `uic`, `QTreeView`/`QTableView` with custom models and item
delegates, a `QProxyStyle`, SVG and PNG icons, modal dialogs — cross-compiled
to `wasm32-wasip2` and running as a wk node, painting through the wk QPA plugin
onto the single RGBA8 surface wk gives a node.

```
cd plugins/qt-torrentfileeditor103
mise run build      # or ./build.sh
mise run test       # runs it headless on wk's real runtime and checks pixels
wk run plugins/qt-torrentfileeditor103/example.wk    # from the repo root
```

`build.sh` needs `plugins/qt` built first (`build-host.sh` → `build-qtbase.sh`
→ `build-qpa.sh`, hours). It fetches upstream itself and never vendors it.

## It works

`./harness` ran the component on `PluginHost` — wk's own runtime, the one the
daemon uses — pumping one frame per iteration the way the compositor does:

```
using fixture .../test/demo.torrent
seeding /data/example.torrent (510 bytes)
surface opened
frame: 6624 dark px, 773818 light px
STATE title='/data/example.torrent - Torrent File Editor' name='wk-qt-demo'
      hash='7ca0c4f9d7405387c5e011bd22fd0bac8e6be838' size='3 MiB' files=2 tops=1
      focus='btnNew'
clicking the Name field at (321, 99) to focus it
the Name field has keyboard focus
a key with no text typed nothing, as it must
STATE title='* ...' name='wk-qt-demoxyz' hash='13a5b2202dfccebd98df3d0812cad6d87cce5fd5'
      ... focus='leName'
the Name field repainted with the typed text
Backspace took the 'z' back: name='wk-qt-demoxy'
clicking the About button at (973, 24)
STATE ... tops=2
the About dialog is up: a SECOND top-level window is composited
PASS
```

and the frames it dumped were **looked at**, because a pixel histogram proves
"not blank", never "the right window". The main window shows the toolbar with
its PNG icons, the four tabs, and every field filled in from the bencode: name,
created-by, a formatted creation date, piece size, piece count, total size, the
40-hex info hash, the generated magnet link and the tracker list. The About
dialog shows the app's **SVG** logo, so the statically-imported `qsvg` image
plugin decodes.

Five independent claims, not one:

1. **It comes up.** `platform=wk`, `families=1`, `screen=1024x768` — the wk QPA
   plugin resolved statically and the compiled-in font registered.
2. **It is a widget frame.** ~6.6k dark pixels of text and borders against
   ~774k light pixels of background at 1024×768.
3. **The .torrent parsed.** Name, info hash, total size and file count all come
   out of `BencodeModel`, so this is the app doing its actual job on a file
   that arrived through the node's filesystem — which is exactly what a
   BindMount wire delivers.
4. **It is an editor, not a viewer.** A genuine pointer click focused the Name
   field, and real key events **typed into it**: `wk-qt-demo` became
   `wk-qt-demoxyz` — inserted at the cursor, so an exact string and not a
   `contains`, which is what tells insertion apart from a selection being
   replaced. The pixels inside the field's rect changed with it (the crop was
   looked at: the characters are drawn, with a caret after them), the info
   hash changed from `7ca0c4f9…` to `13a5b220…` because renaming a torrent
   re-encodes its info dict, and the title gained its modified marker — so the
   edit reached `BencodeModel`, not just a widget. Then `Backspace`, a key
   carrying no text at all, took the `z` back and nothing else.
   The harness sends the OLD event shape first — a good `key`, `text: none` —
   and asserts it types **nothing**. Without that control, "the field says
   `wk-qt-demoxyz`" would be equally consistent with Qt reconstructing the
   letter from the key code, and the run would prove nothing about `text`.
5. **Real input reaches a real widget, and a second window.** A genuine
   `wasi:surface` pointer press/release aimed at the About button's published
   rect opened the About dialog. That is the whole path — host queue →
   `wkgfx_poll_event` → `QWkInput` → `QGuiApplication` hit-testing →
   `QPushButton` — plus **multi-top-level compositing** and a nested
   `QDialog::exec()` inside the frame-paced event dispatcher, neither of which
   had been exercised before.

## The shape of the build

Three cross-builds and a link, all driven by `./build.sh`:

| stage | what | where it installs |
|---|---|---|
| `qt5compat` | `Qt6::Core5Compat` — `QTextCodec` and `QRegExp`, both of which upstream requires | `./sysroot` |
| `qtsvg` | `Qt6::Svg` + the `qsvg` image-format and icon-engine plugins | `./sysroot` |
| `app` | `torrent-file-editor.wasm` (22.6 MB, a wasip2 **component**) | `./` |

**Two prefixes, deliberately.** `plugins/qt/sysroot` holds qtbase and the wk
QPA plugin and is shared; the two extra Qt repos install into `./sysroot` here,
so this port never writes into another plugin's tree and several ports can grow
their own module set from one qtbase. Both go on `CMAKE_PREFIX_PATH` and
`CMAKE_FIND_ROOT_PATH`.

Everything else is inherited from `plugins/qt/wasip2.cmake` unchanged: the
exnref EH flag set, the 8 MB stack, static-only, `find_package` pinned to the
sysroots, and the scrubbed PATH that keeps `wasm-opt` (which cannot parse
exnref) away from the link.

## The four things that actually cost time

**1. `WrapIconv`, and it is a two-prefix problem, not a missing library.**
wasi-libc inherits musl's `iconv`, so qt5compat's config test genuinely passes
and `qiconvcodec.cpp` builds. The damage is downstream:
`Qt6Core5CompatDependencies.cmake` then records `WrapIconv` as a third-party
dependency, `FindWrapIconv.cmake` is installed into **our** prefix, and the
module path an app inherits from `Qt6Config` points at the **qtbase** prefix —
so every app dies with

```
Qt6Core5Compat could not be found because dependency WrapIconv could not be found.
```

while the find module sits right there. Fixed by `-DFEATURE_iconv=OFF`, which
removes the dependency rather than papering over the search path (the app's own
`CMakeLists.txt` overwrites `CMAKE_MODULE_PATH`, so `-D` cannot fix it from
outside anyway). What is lost is only the iconv-backed codecs; `QTextCodec`
keeps UTF-*, Latin-1, every ISO-8859-*, the windows-125x family, KOI8 and the
big codecs — the whole of what a `.torrent`'s `encoding` field realistically
names.

**2. `cmake -P` reports the HOST's platform.** `cmake/Version.cmake` runs in
script mode, where there is no toolchain and `APPLE`/`UNIX`/`WIN32` describe
the build machine. Cross-compiling to wasm *from macOS* therefore takes the
Apple branch while the project took the non-Apple one, and the build stops on
its very first step with `File .../MacOSXBundleInfo.plist.in does not exist` —
which reads like a missing source file. Upstream already knows about this class
of bug in the same place (it passes `-DWIN32=${WIN32}` for exactly this
reason). Patch 0004; this one is worth sending upstream as-is.

**3. `Qt6::FbSupportPrivate` is not optional and not obvious.** Linking
`libqwk.a` alone gives a page of `undefined symbol: QFbScreen::…` — because the
wk QPA plugin *is* fbconvenience: `QWkScreen : QFbScreen`,
`QWkWindow : QFbWindow`, `QWkBackingStore : QFbBackingStore`, and that is where
the guest-side compositing of N top-level windows into one surface lives. It is
an `INTERNAL_MODULE`, so it is its own package rather than a component of
`Qt6Gui`.

**4. `-Werror` on a 2026 codebase with Clang 23.** Upstream turns warnings into
errors when the build type is exactly `Release` (or its typo'd
`RelWithDbInfo`). `CMAKE_BUILD_TYPE=MinSizeRel` is `-Os -DNDEBUG`, skips that
branch entirely, and needs no patch to upstream's warning policy.

## What did NOT need a patch

**The application's own C++ is completely unmodified.** All four patches are
build-system plumbing (no lrelease, the static-plugin/font wiring, the
`cmake -P` guard) or test scaffolding. Nothing in `mainwindow.cpp`,
`bencodemodel.cpp`, the delegates or the custom widgets was touched. That is
the actual result being reported: Qt Widgets applications port to wk by
*configuration*.

## Known gaps, honestly

* **Typing was a host bug, it is fixed, and claim 4 above is the proof.** The
  compositor used to hardcode `text: None` on every key event, so
  `wkgfx_event.ch` was always 0 and no character reached a `QLineEdit` — which
  is why the earliest screenshots show every field populated from the file
  rather than typed. Key events now carry winit's resolved character
  (`crates/client-local-ui/src/compositor/input.rs`). What is still missing on
  the input path is IME and the clipboard — see `plugins/qt/PORTING.md`.
* **Creating a torrent from a folder sits at 0%.** `mainwindow.cpp:789` moves a
  `Worker` to a `QThread` to SHA1 the piece hashes, and this Qt is
  `FEATURE_thread=OFF` (wasi-libc's `pthread_create` is a stub returning
  `ENOTSUP`, so a threaded build would link and then hang). Opening, inspecting
  and editing an existing torrent — the app's reason to exist — never touches
  that path.
* **No file dialog usefulness.** `QFileDialog` opens and browses the node's
  VFS, which is empty apart from what a wire mounted. Fine, but not obvious.
* **No translations.** No host `lrelease`; the UI is English (patch 0001).
* **`QDesktopServices::openUrl`** compiles and does nothing — the About
  dialog's links are dead ends in the sandbox.
* **Only run headless.** The harness pumps frames itself; nobody has yet
  watched this node in the real `wk` UI at vsync, with hover states and drag
  grabs.

## Files

| path | what |
|---|---|
| `build.sh` | fetch + three cross-builds + the link. Heavily commented; read its header before changing a flag |
| `mise.toml` | `build` and `test` |
| `patches/` | four patches to upstream, each with WHAT/WHY/UPSTREAM. `patches/README.md` is the ledger |
| `harness/` | a standalone cargo project that runs the node on `PluginHost` and checks it. Not a `#[test]` in `crates/wk-server` because that file is shared |
| `Dockerfile` | `FROM scratch` + the component. `wk images build … --tag tfe` |
| `example.wk` | a self-contained workspace (its own `dependencies` block) that wires `test/demo.torrent` in |
| `test/demo.torrent` | a 510-byte synthetic multi-file torrent, the fixture the harness verifies |
| `src/`, `sysroot/`, `build/`, `gen/`, `logs/`, `fonts/`, `*.wasm` | all derived, all gitignored |
