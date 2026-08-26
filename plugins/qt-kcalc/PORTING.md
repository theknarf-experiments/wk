# KDE runs in wk

`plugins/qt-kcalc` builds **KCalc 26.04.3** — a real KDE application, not a Qt
app wearing KDE's name — as a `wasm32-wasip2` component that paints into a wk
node's surface through the wk QPA plugin.

Underneath it are **fourteen KDE Frameworks**, cross-compiled from upstream
sources at tag `v6.24.0`, plus qtsvg, zlib, a passthrough libintl, and GMP,
MPFR and MPC.

```
$ ls -l plugins/qt-kcalc/kcalc.wasm
-rw-r--r--  1 knarf  staff  25702240  kcalc.wasm

$ wasm-tools validate --features all kcalc.wasm && wasm-tools component wit kcalc.wasm
world root {
  import wasi:graphics-context/graphics-context@0.0.1;
  import wasi:io/poll@0.2.12;
  import wasi:surface/surface@0.0.2;
  import wasi:frame-buffer/frame-buffer@0.0.1;
  import wk:clipboard/clipboard;
  ...
  export wasi:cli/run@0.2.0;
}
```

**And it runs.** `harness/` drives the component on wk's real `PluginHost`,
headless, and asserts on what the app says it is showing:

```
$ cd harness && cargo run -- --dump ../logs
surface opened
frame: 3629 dark px, 780213 light px
frame dumped to ../logs/kcalc-main.ppm
KXmlGuiWindow visible: STATE input='' display='' tops=1
pressing 8: input = '8'
8 / 2 evaluated (GMP integer path): saw input='8÷2' display='4'
Enter committed the result: input = '4'
Delete cleared: input = ''
1 / 8 evaluated (MPFR float path): saw input='1÷8' display='0.125'
frame dumped to ../logs/kcalc-result.ppm
```

`logs/kcalc-result.ppm` shows the KXmlGui menubar (File / Edit / Settings /
Help, with accelerator underlines), the expression line reading `1÷8`, the
result display reading `0.125`, the full keypad and the `NORM` status bar.

Each line above is a different claim, and the last two are the ones worth
having: a real `wasi:surface` key event went through Qt's `QShortcut` matching
into KCalculator, through KCalc's own tokenizer (which is why the echo is
`8÷2`, U+00F7, and not the `/` that was typed), through its expression parser,
and was evaluated by **KNumber — i.e. by GMP for the integer division and MPFR
for the one that cannot stay an integer**. No pixel histogram can make that
claim; it is why `patches/kcalc-0004-selftest` exists.

---

## The result, stated precisely

**Fourteen frameworks built. One did not.** The one that did not is **KCrash**,
and it is not a near miss — see [What is not here](#what-is-not-here). KCalc
therefore runs with no crash handler, which is the honest state of affairs on a
platform with neither signals nor `fork`.

| framework | built | what it needed |
|---|---|---|
| KCoreAddons | yes | 3 patches — the whole no-process/no-dlopen/no-timezone tax lives here |
| KCodecs | yes | **nothing** |
| KWidgetsAddons | yes | 3 patches; 4 widgets dropped (see below) |
| KGuiAddons | yes | 1 patch (`QProcess`) |
| KI18n (+ KI18nLocaleData) | yes | 1 patch; needs a libintl that wasi-sdk does not have |
| KConfig (Core + Gui) | yes | 1 patch; needs a **native** `kconfig_compiler` |
| KColorScheme | yes | **nothing** |
| KConfigWidgets | yes | **nothing** |
| KArchive | yes | 1 patch (`<pwd.h>`/`<grp.h>`); needs a standalone zlib |
| KIconThemes (+ KIconWidgets) | yes | 1 patch (the icon-engine plugin must be static) |
| KItemViews | yes | **nothing** |
| KBookmarks (+ KBookmarksWidgets) | yes | 1 patch (`QProcess`) — *not in KCalc's graph; built to widen the proof* |
| KXmlGui | yes | 1 patch — the highest-risk framework, and it landed |
| KNotifications | yes | 1 patch, **two deleted lines**, and it removes DBus from the graph entirely |
| **KCrash** | **no** | nothing short of a rewrite; see below |

Four frameworks needed **no patch at all**: KCodecs, KColorScheme,
KConfigWidgets and KItemViews. KConfigWidgets is the interesting one — it is
`KStandardAction`, `KConfigDialog`, the whole settings-dialog machinery, and it
cross-compiled to wasm untouched.

Nineteen patches in total. Eight are pure build-system plumbing (CMake), and of
the eleven that touch C++, most take a `#if` around a code path whose fallback
upstream had already written — the `#else` was there, it just had no way to be
reached. The changes that genuinely reduce capability are listed under
[What is not here](#what-is-not-here); do not skip that section.

**KCalc's own application logic needed exactly four lines changed**, all of them
`setAccessible*` guards. 8,000-odd lines of a real KDE application —
`KXmlGuiWindow`, `KActionCollection`, `KConfigSkeleton`, `KConfigDialog`,
`KLocalizedString`, a custom expression parser and an arbitrary-precision
number class — cross-compiled to `wasm32-wasip2` otherwise untouched.

---

## DBus was the wrong suspect

The obvious guess is that KDE needs DBus and wasm has no DBus. That guess is
wrong, and the reason is a happy accident of CMake.

Every framework in KCalc's graph gates DBus behind the same idiom:

```cmake
set(USE_DBUS_DEFAULT OFF)
if(UNIX AND NOT APPLE AND NOT ANDROID AND NOT HAIKU)
    set(USE_DBUS_DEFAULT ON)
endif()
option(USE_DBUS "Build components using DBus" ${USE_DBUS_DEFAULT})
```

CMake does **not** set `UNIX` for `CMAKE_SYSTEM_NAME=WASI` (Qt only gets it
because `plugins/qt` patches it into its own build — see
`plugins/qt/wasip2.cmake`'s header). So `USE_DBUS` defaults to `OFF` here with
no flag at all. And KCalc's graph never reaches KIO, KService, KDBusAddons,
KGlobalAccel, KAuth or Solid — the frameworks that genuinely cannot lose DBus.

The single exception was **KNotifications**, whose guard reads

```cmake
if (NOT APPLE AND NOT ANDROID AND NOT WIN32 AND NOT HAIKU OR (WIN32 AND NOT WITH_SNORETOAST))
    find_package(Qt6 ... CONFIG REQUIRED DBus)
    find_package(Canberra REQUIRED)
```

CMake binds `AND` tighter than `OR`, so on WASI the first conjunction is
all-true and the block fires. It is a **CMake bug, not a code dependency**:
`src/CMakeLists.txt` already gates every DBus source on `if (HAVE_DBUS)` and
every audio source on `if (TARGET Canberra::Canberra)`, so with `USE_DBUS=OFF`
zero sources need either. Demoting those two lines to non-`REQUIRED` is the
highest-leverage patch in the series.

**No dbus-daemon node was built, and none is needed for this workstream.** It
stays a genuinely appealing idea for the day a KIO/KService/Plasma-class app is
the target, where `KGlobalAccel` and `KDBusAddons` are unconditionally DBus and
there is no switch to flip.

---

## What the walls actually were

Sorted by how many patches each accounts for. Almost none of this is about KDE.

| root cause | Qt feature | why it is off |
|---|---|---|
| no dlopen | `FEATURE_library=OFF` | wasm has no shared objects; every plugin is `Q_IMPORT_PLUGIN`ed |
| no fork/exec | `FEATURE_process=OFF` | a WASI component boundary has neither; a node runs one program and reaches others through `wk:exec` |
| no threads | `FEATURE_thread=OFF` | kills `QFuture`, `QFutureInterface`, `QThreadPool` |
| no zone database | `FEATURE_timezone=OFF` | a node has no `/etc/localtime` and no host tzdata |
| no accessibility | `FEATURE_accessibility=OFF` | no AT-SPI bus, no UIAutomation, no VoiceOver behind a node's surface |
| no print spooler | `FEATURE_printsupport=OFF` | a node paints into one RGBA8 surface |
| wasi-libc gaps | — | no `socketpair`, `getuid`, `getgrgid`, `getpwuid`, `struct rlimit`, `statfs`, `<pwd.h>`, `<grp.h>` |

**The Qt version, not DBus, is the long-term coupling.** KF6 `master` requires
Qt 6.9.0 and `plugins/qt` is 6.8.4. **KF 6.24.0 is the newest tag whose
`REQUIRED_QT_VERSION` is still 6.8.0**, which is why every clone is pinned
there. KDE bumps its Qt floor roughly yearly (6.20→6.8, 6.25→6.9), so staying
current with KF6 eventually forces a Qt 6.9+ port of the wk QPA.

### The one that had already been solved elsewhere

`KXmlGui` hard-requires `Qt6::Network` **and** `Qt6::PrintSupport`. Both live
*inside qtbase*, so — unlike qtsvg, a separate repo this port layers into its
own sysroot — a port-local overlay cannot supply either. That looked like the
wall.

Half of it was removed by someone else: `plugins/qt` now builds **QtNetwork**
over wk's smoltcp fabric, so `Qt6::Network` resolves. Only PrintSupport had to
be patched out, and it is used by exactly one function —
`KShortcutsEditorPrivate::printShortcuts()`, whose body already sat inside an
`#ifndef _WIN32_WCE` because "one can't print on wince".

---

## What is not here

Read this section before concluding anything from "KDE runs in wk".

### KCrash — the one framework that did not make it

KCrash's implementation is `sigaction()`/`sigemptyset()`/`SA_RESTART` signal
handlers that, on a crash, `fork()` a child, `setgroups()`/`setgid()`/`setuid()`
it, `exec()` drkonqi, `waitpid()` for it and `alarm()` a watchdog. Fifteen
distinct undefined identifiers in `kcrash.cpp` alone, plus
`<QUnhandledException>` in `exception.cpp`. wasip2 has no asynchronous signals
and no fork/exec at all.

`patches/kcrash-0001` exists and fixes a real upstream bug (an unconditional
`find_package(Qt6Test REQUIRED)` at top-level scope), so KCrash *configures* —
but it does not compile, and **it is not installed**. It is deliberately absent
from `KF_ORDER` in `build.sh`, so the default build does not stop on it; the
patch is kept so that anyone re-attempting this starts past the configure wall
rather than at it:

```sh
WK_KCALC_STAGES=kf WK_KCALC_KF=kcrash ./build.sh   # reproduces the failure in ~1 min
```

KCalc drops `KF6::Crash` from its component list and never calls
`KCrash::initialize()`.

Building a KCrash that *compiled* would have meant gutting `kcrash.cpp` until
`initialize()` did nothing — a stub wearing a framework's name, which is a
worse lie than an honest absence. When a wk node traps, wasmtime reports it to
the host and the node dies; that is wk's crash story, and drkonqi was never
going to be part of it.

### Capabilities genuinely removed, not rerouted

* **`KPluginMetaData` / `KPluginFactory` cannot load anything.** With
  `QT_CONFIG(library)` off, a `KPluginMetaData` built from a file path comes
  back invalid, `findPlugins()` over a directory finds nothing, and
  `KPluginFactory::loadFactory()` on a non-static plugin returns
  `INVALID_PLUGIN`. Static plugins — everything a wk node actually loads, via
  `Q_IMPORT_PLUGIN` and `QPluginLoader::staticPlugins()` — are untouched.
* **Four KWidgetsAddons widgets are gone**: `KCharSelect`, `KCharSelectData`,
  `KDateTimeEdit`, `KMimeTypeEditor`. Their headers are still installed, so a
  downstream user fails at *link* time rather than at `#include`. Nothing in
  KCalc's graph touches any of them.
* **The stack is English-only by construction.** `poqm/` is deleted from every
  tree (`ecm_install_po_files_as_qm` hard-requires Qt6 LinguistTools, and
  `plugins/qt` has no qttools build), and the libintl is a passthrough that
  returns the msgid. `KLocalizedString` still runs — argument substitution,
  plural selection — it just never finds a catalog. Consequently **no `.mo`
  files need staging into the node's vfs.** Replacing this means cross-building
  a real gettext-runtime, which wants iconv and locale support wasi-libc barely
  has.
* **Notification sounds.** Canberra is libcanberra, an ALSA/PulseAudio event
  player; there is no wasm build. wk *does* have audio
  (`plugins/audio-compat`), so a real backend is possible later — it just will
  not be libcanberra.
* **`QDateTime` config entries lose their timezone.** `KConfigGroup` writes
  them without the zone field, so a config file written here and read by a
  desktop KDE reads back as local time.
* **`KSignalHandler` never emits**, exactly as it already does on Windows.
* **Archives written by a node carry no owner/group name** — there is no
  account database to ask.
* **No icon theme is staged.** `USE_BreezeIcons=OFF` keeps a multi-thousand-SVG
  qrc out of every binary. `KIconEnginePlugin` **is** linked in statically, so
  the moment a theme directory appears under `XDG_DATA_DIRS` in a node's vfs,
  KDE icon-theme semantics are there. Until then `QIcon::fromTheme()` returns
  null icons — blank, not a crash. **This is the most visible cosmetic gap.**

### Not verified

* `KSharedDataCache` **compiles but is unproven at runtime.** The patch removes
  its `mlock` budget, not the cache. `_WASI_EMULATED_MMAN`'s `mmap` is a
  malloc-and-read with no write-back and no sharing, so a "shared" cache is a
  private buffer. `KIconLoader` is its main user; with no icon theme staged
  nothing exercised it here. Suspect this first if icons misbehave.
* `KBookmarks` and `KArchive` **build but nothing drives them.** They are not in
  KCalc's graph.
* Nothing was run on a real compositor — only headless, through `PluginHost`.
  Mouse clicks on the keypad buttons were never exercised; the harness drives
  the app entirely by keyboard.
* `KConfig` **reads** but does not **write**: the run logs
  `Configuration file "/root/.config/kcalcrc" not writable`, which is KConfig's
  own honest diagnostic about the node's vfs, not a port defect. Settings
  therefore do not persist across node restarts yet. Worth chasing before
  anything depends on `KConfigDialog` round-tripping.

---

## Two traps worth writing down

### `git apply` silently skips, and exits 0

A tarball tree at `plugins/qt-kcalc/src/<tree>` has no `.git` of its own but
lives **inside the wk repository**. `git apply` run there discovers the wk repo,
computes a prefix of `plugins/qt-kcalc/src/<tree>/`, compares it against the
paths in the patch — and a patch produced by `git diff` carries
`diff --git a/CMakeLists.txt b/CMakeLists.txt`, which does not start with that
prefix. git prints

```
Skipped patch 'CMakeLists.txt'.
```

and **exits 0**. Nothing fails. The build then configures an unpatched tree and
the error surfaces somewhere else entirely (`Could NOT find KF6Crash`).

`plugins/qt-torrentfileeditor103` uses `git apply` here and gets away with it
only because its patches are plain `diff -u` output with no `diff --git` line —
in that case git falls back to prepending the prefix and it works. That is an
accident, not a design. This port uses `patch -p1`, which has no notion of an
enclosing repository and fails loudly.

### `kconfig_compiler` must be native

`kconfig_add_kcfg_files()` generates `kcalc_settings.{h,cpp}` at build time by
*running* `KF6::kconfig_compiler`. The cross build installs a wasm one, so ninja
tries to exec a `.wasm`:

```
FAILED: [code=126] kcalc_settings.h kcalc_settings.cpp
```

(126 is "cannot execute".) Upstream anticipates this —
`KF6ConfigConfig.cmake.in:21` checks `if(CMAKE_CROSSCOMPILING AND
KF6_HOST_TOOLING)` — so `build.sh` has a `kconfighost` stage that builds a
native `KF6ConfigCore` + `kconfig_compiler` into its own `./host-tooling`
prefix, deliberately kept **off** the app's `CMAKE_PREFIX_PATH` so a host
`KF6ConfigConfig.cmake` can never resolve ahead of the wasm one.

---

## A correction to the recon

The recon that preceded this work recommended **KCharSelect** as a cheaper first
KDE app than KCalc, on the grounds that it needs the same stack minus
KNotifications and minus the three bignum libraries.

**That is wrong, and the reason is instructive.** `KCharSelect` — the *widget*,
in KWidgetsAddons — builds its 3.1 MB Unicode index in a `QRunnable` on a
`QThreadPool` and `waitForFinished()`es a `QFuture` (`kcharselectdata.cpp:34`:
`class RunIndexCreation : public QFutureInterface<Index>, public QRunnable`).
With `FEATURE_thread=OFF`, `<QFuture>` is not even *installed*. It is the one
class in KWidgetsAddons that cannot exist here at all, and it is KCharSelect's
entire reason to exist. A CMake-only reading cannot see this.

GMP, MPFR and MPC, meanwhile, cross-compiled with one flag between them
(`--disable-assembly`) and one surprise: GMP rejects `ABI=32` for an
unrecognised CPU with *"ABI=32 is not among the following valid choices:
standard"* — the `standard` ABI takes its limb size from `long`, which is 32
bits on wasm32, so the right answer is to not interfere.

---

## The exact next experiments, in order

1. **Stage an icon theme.** This is the largest visible gap and the cheapest
   win. Put a Breeze (or hicolor) SVG directory plus `index.theme` into the
   node's vfs under an `XDG_DATA_DIRS` path and check `QIcon::fromTheme()` in
   the harness. `KIconEnginePlugin` and the qsvg image-format plugin are
   already linked in, so nothing needs rebuilding — this is a staging question,
   not a build one. It will also be the first thing that genuinely exercises
   `KSharedDataCache`, which is the runtime unknown flagged above.
2. **Run it on the real compositor**, not just the headless harness — click the
   buttons, open the settings dialog (`KConfigDialog`, i.e. KConfigWidgets, the
   framework that needed no patch at all), and confirm `KXMLGUIFactory`'s
   menubar behaves under the frame-paced event dispatcher.
3. **Decide the `FEATURE_library` question properly.** `dlopen`/`dlsym`/
   `dlclose`/`dlerror` exist as linkable stubs in wasi-sdk, so turning Qt's
   `FEATURE_library` back on would make `QLibrary` compile and fail honestly at
   runtime — and would delete four of the nineteen patches here. It is a full
   qtbase rebuild affecting every Qt port, so it is a decision for
   `plugins/qt`, not for this one.
4. **Try a KIO-class app** — and expect a different answer. KIO drags in
   KService, Solid, KJobWidgets, KWindowSystem, dlopen'd worker plugins and a
   DBus `kiod`. That is where the dbus-daemon-node idea earns its keep, and
   where `patches/kcoreaddons-0002-no-qlibrary` stops being survivable.
5. **A real gettext-runtime**, if translations ever matter. Everything else in
   this port degrades loudly; the passthrough libintl degrades silently, and
   that is the one decision here most likely to be misread as a regression
   later.

---

## Building it

```sh
cd plugins/qt-kcalc
./build.sh                 # 40-70 min cold; run detached and tail ./logs
cd harness && cargo run -- --dump ../logs
```

Both were run end to end for this write-up: `./build.sh` with `sysroot/` and
the KCalc tree deleted, so every framework was rebuilt from a pristine upstream
checkout with only the patches in `patches/` applied, and the harness then
passed against that binary. The transcript at the top of this file is from that
run.

Prerequisite: `plugins/qt` must already be built (`build-host.sh`,
`build-qtbase.sh`, `build-qpa.sh` — hours). `build.sh`'s preflight says so.

Stages, individually runnable via `WK_KCALC_STAGES=...`:
`ecm zlib libintl qtsvg gmp mpfr mpc kf kconfighost app`, and a subset of the
frameworks via `WK_KCALC_KF="kcoreaddons kconfig ..."`.

See `patches/README.md` for the per-patch ledger and the `UPSTREAM:`
self-classification of each one.
