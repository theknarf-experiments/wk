# Patches to KDE Frameworks 6.24.0 (and to KCalc)

Upstream is **fetched, never vendored**. `../build.sh` shallow-clones each
framework from `invent.kde.org` at tag `v6.24.0` into `../src/` (gitignored) and
pulls qtsvg / zlib / gmp / mpfr / mpc / kcalc as tarballs. Every change to any
of it lives here as a file, applied `-p1` at that tree's root, in filename
order, grouped by the `<repo>-` prefix.

## Convention

Each patch begins with a plain-text header — everything before the first
`diff --git` line, which `git apply` ignores — carrying three fields:

* **WHAT** — one sentence, what the diff does.
* **WHY** — the failure it fixes, quoted where the message is misleading (and
  in this port they are misleading a *lot*: a missing Qt feature usually
  surfaces as "no member named X in <your own subclass>").
* **UPSTREAM** — honest self-classification:
  * `as-is` — upstream would take this diff essentially unchanged. It guards
    for a Qt feature switch that can legitimately be off, or fixes a real
    portability bug in their tree.
  * `not-as-is` — right for us, but it trades away behaviour or is wk-specific,
    and upstream would want it differently (a configure check, an option, a
    proper backend).
  * `no` — ours, for testing or for wk, and not upstream's business.

## Idempotency: `git reset --hard`, not a reverse-check and not a stamp

The KF trees are **git clones**, so `build.sh` resets each one and re-applies
the whole series from scratch every run. That is cheap (no re-clone), it cannot
leave a half-applied tree, and — unlike the `git apply --reverse --check` idiom
in `plugins/qt/build-qtbase.sh` — it keeps working when two patches touch
nearby lines. The tarball trees (qtsvg, zlib, the bignums, kcalc) have no git,
so those keep the `.wk-patched` stamp that `plugins/qt-torrentfileeditor103`
uses.

**Consequence, and it cost an hour here:** never regenerate a patch from a tree
after starting a build that touches it — `apply_patches_git` will have reset it
first and `git diff` comes back empty. Generate the patch immediately after
editing, then build.

## The four things every patch here is about

Almost nothing in this series is about KDE. Sorted by how many patches they
account for:

| root cause | Qt feature | why it is off |
|---|---|---|
| no dlopen | `FEATURE_library=OFF` | wasm has no shared objects; every plugin is `Q_IMPORT_PLUGIN`ed |
| no fork/exec | `FEATURE_process=OFF` | a WASI component boundary has neither; a node runs one program, and reaches others through `wk:exec` |
| no threads | `FEATURE_thread=OFF` | wasip2 has no threads in wk's runtime; kills `QFuture`, `QThreadPool` |
| no zone database | `FEATURE_timezone=OFF` | a node has no `/etc/localtime` and no host tzdata |

Plus two smaller ones: `FEATURE_accessibility=OFF` (no AT-SPI/UIA/VoiceOver
behind a node's surface) and wasi-libc's own gaps (`socketpair`, `getuid`,
`struct rlimit`, `statfs`).

**Notably absent: DBus.** See `../build.sh`'s header — CMake does not set `UNIX`
for `CMAKE_SYSTEM_NAME=WASI`, so every `USE_DBUS` option in the graph defaults
to `OFF` with no flag at all, and KCalc's graph never reaches the frameworks
that cannot lose it.

## The ledger

| patch | what | upstream |
|---|---|---|
| `kcoreaddons-0001-wasi-posix-gaps` | WASI branches for `statfs`, `socketpair`, `struct rlimit`; drop `kprocess.cpp`/`ksandbox.cpp` | mixed |
| `kcoreaddons-0002-no-qlibrary` | guard every dynamic-plugin path in `KPluginMetaData`/`KPluginFactory` on `QT_CONFIG(library)` | not-as-is |
| `kcoreaddons-0003-no-timezone` | guard `KFormatPrivate`'s two `QTimeZone` uses | as-is |
| `kwidgetsaddons-0001-drop-widgets-wasi-cannot-carry` | drop `KCharSelect`, `KCharSelectData`, `KDateTimeEdit`, `KMimeTypeEditor` and the 3.1 MB charselect resource | not-as-is |
| `kwidgetsaddons-0002-no-accessibility` | guard 7 `setAccessible*` calls | as-is |
| `kwidgetsaddons-0003-no-qlibrary` | guard `KMessageBox`'s FrameworkIntegrationPlugin lookup | as-is |
| `kguiaddons-0001-no-qprocess` | guard `KUrlHandler`'s `khelpcenter` launch | as-is |
| `ki18n-0001-no-qlibrary-no-timezone` | guard Transcript plugin loading; guard `KTimeZone::country()` | mixed |
| `kconfig-0001-wasi-no-uid-no-process-no-timezone` | `getuid`, two helper-binary launches, `QDateTime` zone round-trip; skip `kconf_update`/`kreadconfig` | mixed |

See `../PORTING.md` for the full graph, what built, what did not, and the
capabilities these patches remove.
