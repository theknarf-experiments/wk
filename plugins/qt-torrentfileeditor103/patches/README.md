# Patches to torrent-file-editor 1.0.3

Upstream is **fetched, never vendored** (`build.sh` pulls the v1.0.3 tarball
into `../src/`, which is gitignored). Every change to it lives here as a file,
applied `-p1` at the app's source root, in filename order.

## Convention

Each patch begins with a plain-text header — everything before the first
`--- a/` line, which `git apply` ignores — carrying three fields:

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
cannot work here: patch 0002 adds lines inside patch 0001's context, so once
both are applied 0001 no longer reverse-applies and the check would loop
forever on "patch does not apply".

Verified after every edit: a pristine extraction plus all four patches is
byte-identical to the tree the artifact was built from.

## The ledger

| patch | what | upstream |
|---|---|---|
| `0001-no-linguist-tools` | drop `find_package(Qt6LinguistTools)`, the `.qm` compilation, `translations.qrc` and the `lupdate_*` targets | not-as-is |
| `0002-wk-node` | default to the `wk` QPA platform; `Q_IMPORT_PLUGIN` the platform and Svg plugins; link `libqwk.a`, `Qt6::FbSupportPrivate` and the component-type object; compile a font in under `:/fonts` | as-is in spirit |
| `0003-selftest` | `WK_TFE_SELFTEST=1` narration so a headless harness can assert on more than pixels | no |
| `0004-version-script-host-apple` | `cmake/Version.cmake` must not take its Apple branch when `cmake -P` reports the HOST's `APPLE` during a cross build | as-is |

Only 0001 and 0003 change what the program does. 0002 is guarded by
`#ifdef __wasi__` and `if(WK_...)`, and 0004 is a strictly-safer guard, so a
desktop build of the patched tree behaves exactly as upstream does.

## What did NOT need a patch

Worth recording, because it is the interesting result: **the application's own
C++ is completely unmodified.** 6,838 lines of Widgets code — three `.ui`
files through `uic`, `QTreeView`/`QTableView` with custom models and item
delegates, a `QProxyStyle`, custom widgets, modal dialogs, the clipboard —
cross-compiled to `wasm32-wasip2` and ran with no source changes at all. Every
patch above is build-system plumbing or test scaffolding.
