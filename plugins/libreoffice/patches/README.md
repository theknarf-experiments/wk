# patches/ — every change this port makes to LibreOffice

`plugins/libreoffice/src/` is a `git clone` of upstream LibreOffice core at tag
`libreoffice-26.2.6.2` and is gitignored. **Nothing in it is ever edited in
place.** Every change lives here as a patch file, and `build-configure.sh`
applies them idempotently before running `autogen.sh`.

Same convention as `plugins/qt/patches/README.md`; read that one too.

## Naming

    core-NNNN-<slug>.patch

`core` is the module (there is only one upstream repo here, unlike the Qt port
which has `qtbase-` and `qtdeclarative-`). `NNNN` is a four-digit sequence in
apply order. Applied with `git apply -p1` at the LibreOffice source root.

## Every patch must be reverse-checkable

`build-configure.sh` decides "already applied?" with

    git -C src apply --reverse --check "$p" && continue
    git -C src apply "$p"

so a patch that cannot be cleanly reversed breaks re-runnability. Generate them
with `git -C src diff` (or `git format-patch`), never by hand-editing a diff.

## Commit-message header

Every patch file starts with a header answering three questions:

    WHAT      one line: what it changes.
    WHY       why this port needs it, with the file:line that forces it.
    UPSTREAM  yes | not-as-is | no

`UPSTREAM` matters more here than in the Qt port, because the patch set splits
cleanly in two:

* **`yes`** — a `wasm32-wasi` host triple is genuinely upstreamable. The
  `configure.ac` host arm, the gbuild platform file, the libc gaps in `sal`,
  the `dlsym` guards, the threadless fallbacks: LibreOffice would plausibly
  take all of these, and keeping them separable is what makes a 26.4 rebase
  survivable.
* **`no`** — `vcl/wk/` is a wk VCL backend. It will never go upstream and does
  not need to be written as though it might.

Do not mix the two in one patch.

## Design rule

**Prefer configuring a feature OFF over patching LibreOffice.** Much of what
looks like it needs a patch is already a configure switch: `--enable-wasm-strip`
alone removes about two dozen subsystems, `--with-wasm-module=impress` drops
Writer and Calc, `--disable-dynamic-loading` replaces the whole dlopen story
with static component registration, and `--enable-cairo-rgba` removes a
per-frame pixel swizzle that would otherwise have been our code. See the
commented flag wall in `build-configure.sh`.

## The two structural patches

`build-configure.sh` **refuses to run** until these exist, because their absence
produces failures that read like something else entirely.

| # | file | what | upstream |
|---|---|---|---|
| — | `core-0001-configure-wasi-host-arm.patch` | a `wasi*)` arm beside `emscripten)` at `configure.ac:1247`, the OS/CPUNAME arm at `:5801`, `WASI` on the `ENABLE_WASM_STRIP_*` gate at `:4311`, and the `BUILD_TYPE_FOR_HOST` token | yes |
| — | `core-0002-gbuild-wasi-platform.patch` | new `solenv/gbuild/platform/WASI_INTEL_GCC.mk`, plus dropping `--start-group`/`--end-group` from `unxgcc.mk:159,166` | yes |

Neither is written yet. `build-configure.sh`'s error message spells out what
each one has to contain; `PORTING.md` argues why.

## Ledger

Nothing applied yet. Add a row per patch as they land:

| patch | what | upstream |
|---|---|---|
| _(empty)_ | | |
