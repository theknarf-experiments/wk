# Patches to upstream Qt 6.8.4

`plugins/qt/src/` is an extracted upstream tarball and is **gitignored**.
Nothing in it is ever edited in place: every change to Qt lives here as a
`.patch` file, and `build-host.sh` / `build-qtbase.sh` apply the whole
directory before configuring. Blow away `src/` at any time and the next build
reproduces the exact same tree.

## Convention

**Filename** — `<module>-NNNN-<slug>.patch`

    qtbase-0001-wasi-platform.patch
    qtbase-0002-wasi-system-detection.patch
    qtdeclarative-0001-....patch

The `<module>` prefix is how the scripts route a patch to a source tree: a
`qtbase-*.patch` is applied with `git apply` inside
`src/qtbase-everywhere-src-6.8.4`. The `NNNN` fixes the apply order.

**Format** — a normal `-p1` diff rooted at that module's source directory
(`a/src/corelib/global/qsystemdetection.h`, `b/cmake/platforms/...`). Produce
one with `git diff`, or `diff -u` plus hand-fixed `a/` `b/` prefixes. New files
are fine (`/dev/null` → `b/...`); that is how the mkspec and the Platform
module arrive.

**Idempotency is a hard requirement.** Both scripts test
`git apply --reverse --check` first and skip a patch that is already applied,
so a patch must reverse cleanly. In practice this means: no fuzzy context, no
two patches touching the same hunk, and no patch that partially applies.

**Header** — every patch begins with a commit-message-shaped block that
answers three questions, in this order:

    Subject: one line, imperative

    WHAT   the Qt file(s) touched, with the reason each one had to change.
    WHY    the concrete thing that breaks without it: name the missing
           wasi-libc symbol, the #error, the CMake variable, the FATAL_ERROR.
           "Doesn't build" is not a reason; "wasi-libc has no sigaction, and
           qcore_unix.cpp:23 uses struct sigaction unguarded" is.
    UPSTREAM  yes / no / not-as-is, and why.

## The UPSTREAM field

Qt has no `__wasi__` port, so some of this is genuinely upstreamable and some
of it is only right for wk. Classify honestly — the field is what a future
reader uses to decide whether to rebase onto 6.9 or to carry forever.

* **yes** — a defect or a gap that upstream would plausibly take as-is.
  Extending an existing `!defined(Q_OS_WASM)` guard to also cover
  `Q_OS_WASI`, or adding `Q_PROCESSOR_WASM` for `__wasm32__`, is this kind of
  change: small, mechanical, no policy in it.
* **not-as-is** — the right idea, wrong shape for upstream. A new
  `Platform/WASI.cmake` or a `wasi-clang-wasip2` mkspec is a whole new Qt
  platform; upstream would want it as a proper port with CI, not as four
  files. Note what the real version would need.
* **no** — wk-specific. The wk QPA plugin, anything that hard-codes wkgfx,
  anything that trades away a feature we happen not to need.

## Design rule

**Prefer configuring a feature OFF over patching Qt.** Almost everything
wasi-libc lacks is behind a Qt feature that `build-qtbase.sh` already forces
off, and a flag costs nothing to carry across a Qt upgrade while a patch costs
a rebase. Reach for a patch only when there is no flag — which, so far, is the
platform/mkspec plumbing and the handful of unguarded POSIX calls in the
generic UNIX backends.

## Ledger

Keep this table in sync; `PORTING.md` links to it.

| Patch | Touches | Why | Upstream |
|---|---|---|---|
| `qtbase-0001-wasi-platform.patch` | `cmake/QtPlatformSupport.cmake`, `cmake/QtMkspecHelpers.cmake`, `cmake/QtBuildRepoHelpers.cmake`, new `mkspecs/common/wasi/*`, new `mkspecs/wasi-clang-wasip2/*` | Gives Qt a `WASI` platform variable, sets `UNIX` (nothing else does), maps it to a new mkspec, and keeps pkg-config away from the host | not-as-is |
| `qtbase-0002-wasi-system-detection.patch` | `src/corelib/global/qsystemdetection.h`, `src/corelib/global/qprocessordetection.h` | `__wasi__` matches no arm and hits the `#error`; `__wasm32__` gets no `Q_PROCESSOR_WASM` | yes |
| `qtbase-0003-corelib-wasi-libc-gaps.patch` | `qcore_unix_p.h`, `qcore_unix.cpp`, `qeventdispatcher_unix.cpp`, `qcoreapplication.cpp`, `qfilesystemengine_unix.cpp`, `qstandardpaths_unix.cpp` | No `sys/wait.h`, `sigaction`, `eventfd` (declared but never defined), `getuid`/`geteuid`, `pwd.h`/`grp.h`; and the thread pipe must not be fatal when `pipe()` returns ENOTSUP | mostly yes |
| `qtbase-0004-corelib-missing-includes.patch` | `src/corelib/global/qsimd.cpp` | `getenv` used with no `<stdlib.h>`; other libcs pull it in transitively, wasi-libc does not | yes |
| `qtbase-0005-corelib-no-tz-no-signals.patch` | `qtenvironmentvariables.cpp`, `qlockfile_unix.cpp` | No `tzset`/`tzname`, no `kill`, and `flock` declared but never defined | tz yes, lock semantics not-as-is |
| `qtbase-0006-widgets-no-passwd-db.patch` | `src/widgets/dialogs/qfiledialog.cpp` | No `<pwd.h>`; `~user` expansion takes the VxWorks/Integrity path. **The only thing blocking QtWidgets.** | yes |
| `qtbase-0007-corelib-no-mremap.patch` | `src/corelib/io/qresource.cpp` | `mremap` declared and its `MREMAP_*` flags defined, but no symbol anywhere | yes |

### One file, one patch

Every source file appears in exactly **one** patch, so nothing has to apply on
top of another patch's hunks. That is why the `flock` fix lives in `0005`
(which already owned `qlockfile_unix.cpp`) and the thread-pipe fix lives in
`0003` (which already owned `qeventdispatcher_unix.cpp`), even though both were
found much later, at application link/run time.

### wasi-libc's headers are not a contract

Three separate bugs here — `eventfd` (0003), `flock` (0005) and `mremap` (0007)
— have the identical shape: **wasi-libc declares the function and defines its
feature macros, and then no library in the SDK defines the symbol.** So
`__has_include(<sys/eventfd.h>)` says yes, `#ifdef LOCK_EX` says yes,
`#if defined(MREMAP_MAYMOVE)` says yes; configure passes, all of qtbase builds,
and the failure appears only when a real application is linked, as an undefined
symbol from an object file with no obvious connection to the problem.

**Link a real executable before believing this port works.**
`plugins/qt/` has no test app checked in yet; the one used to find these three
was a throwaway QApplication + QWidget + QPainter program. Adding a permanent
one is the single highest-value next chore.

### The one that cost the most to find

`0001`'s `set(UNIX 1)` lives in `QtPlatformSupport.cmake`, **not** in
`cmake/platforms/Platform/WASI.cmake`. The Integrity idiom was tried first and
silently does nothing: `EnableLanguage` resolves
`include(Platform/${CMAKE_SYSTEM_NAME})` against CMake's own `Modules`
directory in preference to `CMAKE_MODULE_PATH` — which is what
`QtAutoDetectHelpers.cmake`'s "CMake-provided platform modules take precedence"
means. Integrity works only because CMake ships no `Platform/Integrity.cmake`;
CMake 4.4.2 *does* ship `Platform/WASI.cmake`. The symptom is not "UNIX is
unset" but `private/qcore_unix_p.h file not found` in a dozen unrelated files,
because the `CONDITION UNIX` sources never join the target and syncqt therefore
never copies the header.
