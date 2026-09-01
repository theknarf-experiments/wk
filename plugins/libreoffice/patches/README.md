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

Every patch file is *supposed* to start with a header answering three
questions:

    WHAT      one line: what it changes.
    WHY       why this port needs it, with the file:line that forces it.
    UPSTREAM  yes | not-as-is | no

**None of the eight currently does, and this rule has been broken for as long
as the patches have existed.** The cause is mechanical rather than careless:
patches are regenerated with `git -C src diff`, which writes the file from
scratch and drops anything above the first `diff --git` line. Restoring the
headers means either re-adding them by hand after every regeneration — which
will be forgotten — or writing them somewhere `git diff` cannot clobber. The
Ledger below is that somewhere, for now; treat it as the interim answer and
the per-file header as unfinished business. Do not "fix" this by hand-editing
a diff: `build-configure.sh` reverse-applies every patch to decide whether it
is already applied, so a patch that has been touched by hand is a patch that
can break re-runnability.

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
| — | `core-0002-gbuild-wasi-platform.patch` | new `solenv/gbuild/platform/WASI_INTEL_GCC.mk` | yes |

**Both are written.** The sentence that used to stand here — *"Neither is
written yet"* — was wrong from the moment `core-0001` landed and stayed in the
file for six sessions.

The other correction on this table: `core-0002` does **not** drop
`--start-group`/`--end-group` from `unxgcc.mk:159,166`, as this README used to
claim. Nothing patches those lines. gbuild still emits both flags on every
`DISABLE_DYNLOADING` link, and `toolwrap/wasi-clang++:37` filters them out on
the way to the linker — a wrapper, not a patch, because the flags also arrive
from externals' own build systems where no LibreOffice patch could reach them.

## Ledger

One row per patch. Line counts are from 2026-09-01 and are the cheapest way to
notice that a patch has drifted from what this table describes.

| patch | lines | what | upstream |
|---|---|---|---|
| `core-0001-configure-wasi-host-arm.patch` | 134 | `configure.ac`: the `wasi*)` host arm and its four satellites | yes |
| `core-0002-gbuild-wasi-platform.patch` | 233 | new `solenv/gbuild/platform/WASI_INTEL_GCC.mk`: the compiler/linker recipe, `gb_WASI_SHIM` on every link line under `--whole-archive`, `gb_CXX_LINKFLAGS :=`, and the `filter-out -pthread` of **both** `gb_CXXFLAGS` and `gb_LinkTarget_CXXFLAGS` (decision 16) | yes |
| `core-0003-wasm-build-type-token.patch` | 64 | `Repository.mk`, `RepositoryModule_build.mk`, `pre_BuildTools.mk`, `static/Module_static.mk`: the four `BUILD_TYPE_FOR_HOST=EMSCRIPTEN` gates, so `wasmbridgegen` and `embindmaker` are built for a WASI host too | yes |
| `core-0004-sal-wasi-platform-macros.patch` | 53 | `include/osl/endian.h`, `include/sal/alloca.h`, `include/sal/config.h`: the platform `#if` ladders that have no WASI arm | yes |
| `core-0005-uno-bridge-wasi.patch` | 31 | `bridges/Library_cpp_uno.mk`, `bridges/Module_bridges.mk`: select the `gcc3_wasm` bridge for a WASI host. Selection is correct; the bridge itself does not yet compile (E4) | yes |
| `core-0006-osl-wasi-gaps.patch` | 487 | ten files under `sal/osl/unx/`, all in-file `#if defined(WASI)`: `chown`/`getuid`, a locally declared `struct passwd`, the synthetic user, socket option/type constants and the errnos wasi-libc lacks, `OSL_UNIX_PATH_MAX`, `tzset`/`timezone`, and `random.cxx` on `getentropy` (decisions 14, 20) | yes |
| `core-0007-osl-wasi-subsystems.patch` | 411 | `sal/Library_sal.mk` + `include/osl/process.h` + three new `sal/osl/unx/*_wasi.cxx`: the three `osl` subsystems that diverge whole rather than locally — process, pipe, signal (decision 14) | yes † |
| `core-0008-gbuild-macos-host-echo.patch` | 30 | `solenv/gbuild/platform/unxgcc.mk`: `$(shell echo -n …)` → `printf '%s '`. A macOS **build-host** bug in shared gbuild with no wasm content at all (decision 15) | yes |

Total 1,443 lines, which is exactly the working tree's own `git -C src diff`
after all eight are applied — so nothing is duplicated between them or missing
from them. Re-check with `wc -l` on both sides after any regeneration.

The `upstream` column is **this port's own judgement and has never been tested
against upstream** — nothing here has been posted to Gerrit. † `core-0007` is
the one most likely to come back as `not-as-is`: it adds an enumerator to the
public stable `oslProcessError` in `include/osl/process.h`, and its pipe stub
encodes a wk-specific premise ("the node is the process") that upstream has no
reason to share.
