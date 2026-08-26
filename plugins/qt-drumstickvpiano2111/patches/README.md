# Patches to drumstick 2.11.1 (`RELEASE_2_11_1`)

Upstream is **fetched, never vendored** (`build.sh` pulls the tag's tarball
into `../src/`, which is gitignored). Every change to it lives here as a file,
applied `-p1` at drumstick's source root, in filename order.

## Convention

Each patch begins with a plain-text header — everything before the first
`diff --git` line, which `git apply` ignores — carrying three fields:

* **WHAT** — one sentence, what the diff does.
* **WHY** — the failure it fixes, quoted where the message is misleading.
* **UPSTREAM** — honest self-classification:
  * `as-is` — upstream would take this diff essentially unchanged. It fixes a
    real bug in their tree, not something wk-specific.
  * `not-as-is` — the change is right for us but trades away behaviour, or is
    wk-specific, and upstream would want it differently (an option, a
    `#ifdef`, a proper backend).
  * `no` — ours, for testing or for wk, and not upstream's business.

## Applying them is a stamp, not a reverse-check

`build.sh` applies the whole series to a freshly-extracted tree and then
touches `.wk-patched`. A tree without that stamp is thrown away and extracted
again. The `git apply --reverse --check` idiom used elsewhere in this repo
cannot work here: patch 0005 adds lines inside patch 0004's context, so once
both are applied 0004 no longer reverse-applies and the check would loop
forever on "patch does not apply".

**`GIT_CEILING_DIRECTORIES` is load-bearing in that loop.** The extracted tree
sits inside the wk repository's working tree, so a plain `git -C "$tree" apply`
finds wk's `.git`, decides the patch's paths are relative to the REPO ROOT,
concludes that `library/widgets/CMakeLists.txt` is outside the current
subdirectory, and **skips every hunk with exit status 0** — visible only under
`--verbose`, as `Skipped patch ...`. The build then proceeds against pristine
sources and dies a minute later somewhere unrelated. Naming a ceiling stops
git's upward search at `src/`, so `git apply` runs outside any repository and
resolves paths against the cwd.

Verified after every edit: a pristine extraction plus all five patches is
byte-identical to the tree the artifact was built from.

## The ledger

| patch | what | upstream |
|---|---|---|
| `0001-no-linguist-tools` | drop `find_package(Qt6LinguistTools)`, the `.qm` compilation and the `update-*-translations` targets | not-as-is |
| `0002-no-dlopen` | guard every plugin-from-disk path in `BackendManager` with `#if QT_CONFIG(library)` | **as-is** |
| `0003-wk-rt-backend` | add the `wk-in`/`wk-out` Drumstick RT backends over `wk:midi`, plus `wk-common`; a `Q_OS_WASI` arm for the default backend names | not-as-is |
| `0004-vpiano-wk-node` | default to the `wk` QPA platform; `Q_IMPORT_PLUGIN` the platform and MIDI plugins; link `libqwk.a`, `Qt6::FbSupportPrivate` and the component-type objects; compile a font in under `:/fonts`; fix the `NET_BACKEND` arm | mixed (see header) |
| `0005-vpiano-selftest` | `WK_VPIANO_SELFTEST=1` narration so a headless harness can assert on more than pixels; `WK_VPIANO_MIDI_THRU` | no |

Only 0001, 0002 and 0005 change what the program does on a desktop, and 0002
only makes an unbuildable configuration build. Everything in 0003 is behind
`CMAKE_SYSTEM_NAME STREQUAL "WASI"` / `Q_OS_WASI`, everything in 0004 behind
`WK_BACKEND` and `if(WK_...)`, everything in 0005 behind `#ifdef __wasi__`.

## Two CMake traps worth remembering

**`if(${CMAKE_SYSTEM_NAME} MATCHES "WASI")` is always FALSE.** That is the form
upstream uses for its Darwin and Windows arms, and copying it is the obvious
thing to do. `${CMAKE_SYSTEM_NAME}` expands to the bare word `WASI`, CMake
dereferences an unquoted `MATCHES` operand when a variable of that name exists,
and `Platform/WASI.cmake` is literally `set(WASI 1)` — so the test becomes
`if(1 MATCHES "WASI")`. Silently. The backends are then never built, the app
links happily, and the first symptom is `qFatal("Unable to find a suitable
input backend")` at runtime. `Darwin` and `Windows` work only because no
variable of those names happens to exist. Use `STREQUAL`.

**`project(... LANGUAGES CXX)` makes a C-only target unlinkable.** The wk shim
is C, and CMake answers "can not determine linker language for target". The
subtree calls `enable_language(C)` rather than editing upstream's `project()`.

## What did NOT need a patch

Worth recording, because it is the interesting result: **the application's own
C++ is untouched apart from test narration.** vpiano's dialogs, its
QGraphicsView keyboard widget, its settings, its menus and its whole MIDI model
cross-compiled to `wasm32-wasip2` and ran unmodified. Patch 0003 does not
change one line of the app — it adds a backend beside the six upstream already
has, and drumstick's own `BackendManager` finds it through
`QPluginLoader::staticInstances()` exactly as designed.
