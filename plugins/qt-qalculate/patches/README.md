# Patches to libqalculate 5.12.0 and qalculate-qt 5.12.0

Upstream is **fetched, never vendored** — `build.sh` pulls six tarballs
(qalculate-qt, libqalculate, gmp, mpfr, libxml2 and the qtsvg Qt module) into
`../src/`, which is gitignored. Every change to any of them lives here as a
file, applied `-p1` at that tree's root, in filename order.

**gmp, mpfr and libxml2 need no patches at all.** They cross-compile to
wasm32-wasip2 with wasi-sdk 34-rc.2 unmodified; only the `--host` triple and
the flags in `build.sh` are needed. That is worth recording because the
dependency chain was the thing this port was expected to founder on, and it
was not.

The app's `CMakeLists.txt` is **not** here, and is not a patch. qalculate-qt
v5.12.0 has no CMake build system at all — it is qmake-only — so the port
supplies its own at `../cmake/CMakeLists.txt` and `build.sh` copies it over the
fetched tree. See that file's header.

## Convention

Each patch begins with a plain-text header — everything before the first
`--- a/` line, which `git apply` ignores — carrying three fields:

* **WHAT** — one sentence, what the diff does.
* **WHY** — the failure it fixes, quoted or measured where the message is
  misleading.
* **UPSTREAM** — honest self-classification:
  * `as-is` — upstream would take this diff essentially unchanged. It fixes a
    real gap in their tree, not something wk-specific.
  * `not-as-is` — the change is right for us but trades away behaviour, or is
    wk-specific, and upstream would want it differently (an option, a
    `configure` switch, a proper backend).
  * `no` — ours, for testing or for wk, and not upstream's business.

## They are plain diffs, and that is load-bearing

No `diff --git` or `index` lines. `git apply` treats a patch carrying a
`diff --git` header as **repository-relative**, and every one of these trees
sits inside the wk repository, so it silently reports `Skipped patch
'libqalculate/util.cc'` — **with exit status 0** — and applies nothing. The
build then fails a hundred lines later with the error the patch was written to
fix. Strip those two lines from anything you regenerate here.

## Applying them is a stamp, not a reverse-check

`build.sh` applies each series to a freshly-extracted tree and then touches
`.wk-patched`. A tree without that stamp is thrown away and extracted again.
The `git apply --reverse --check` idiom used in `plugins/qt` cannot work here:
patches in the same series touch nearby lines, so once both are applied the
earlier one no longer reverse-applies and the check would loop forever on
"patch does not apply".

## The ledger

| patch | what | upstream |
|---|---|---|
| `libqalculate-0001-wasi-no-pwd-h` | guard `<pwd.h>` and `getHomeDir()`'s `getpwuid()` fallback | as-is |
| `libqalculate-0002-wasi-inline-threads` | **the load-bearing one.** In-memory message FIFO + inline `run()` from `sleep_ms()`, instead of pthreads | not-as-is |
| `qalculate-qt-0001-wk-node` | default to the `wk` QPA platform; `Q_IMPORT_PLUGIN` the platform and Svg plugins | as-is in spirit |
| `qalculate-qt-0002-no-single-instance` | compile out the `QLockFile`/`QLocalSocket` single-instance handshake | not-as-is |
| `qalculate-qt-0003-selftest` | `WK_QALC_SELFTEST=1` narration so a headless harness can assert on the ANSWER, not on pixels | no |

Only `libqalculate-0002` and `qalculate-qt-0002` change what the program does.
The rest are guarded by `#ifdef __wasi__`, so a desktop build of these trees
behaves exactly as upstream does.

## The one that matters

`libqalculate-0002` is why this port is not a two-hour job. Measured on the
unpatched library, cross-compiled and run under `wasmtime -W exceptions`:

```
CALCULATOR->calculate(&m, "6*7", 2000, eo)  ->  rc=0 aborted=1 in 0 ms
```

`Thread::start()` calls `pthread_create`, which wasi-libc defines as a stub
returning `ENOTSUP`, so every caller takes its `mstruct->setAborted()` path and
the GUI displays the word **aborted** for every expression — in a window that
paints perfectly. That is the failure mode the harness asserts against by
name, because nothing about the frame gives it away.

After the patch, from the same test program:

```
THREADED 6*7                    rc=1 aborted=0 -> 42
THREADED 5 m + 2 ft to cm       rc=1 aborted=0 -> 560.96 cm
THREADED sqrt(2)                rc=1 aborted=0 -> interval(1.4142135, 1.4142136)
THREADED solve(x^2-4=0, x)      rc=1 aborted=0 -> [2  -2]
THREADED 2^100                  rc=1 aborted=0 -> 1.2676506E30
ASYNC(msecs=0) busy=0 aborted=0 -> 29.43 N
```

## What did NOT need a patch

Worth recording, because it is the interesting result. **qalculate-qt's own
threading needed no changes at all** — not one line of `qalculatewindow.cpp`'s
`ViewThread`/`CommandThread` message loops, their `start()`/`write()` call
sites, or their wait loops. They inherit libqalculate's `Thread`, and every
waiter in both projects already spins on `sleep_ms()` immediately after handing
the worker its messages, which is exactly where `libqalculate-0002` runs the
body. 32,339 lines of Widgets code — a `QPlainTextEdit` subclass with a live
completer, a `QTextEdit`-backed history with HTML results, a dockable keypad,
a dozen modal dialogs, `QSortFilterProxyModel`s, custom item delegates —
cross-compiled to `wasm32-wasip2` with the four small diffs above and nothing
else.
