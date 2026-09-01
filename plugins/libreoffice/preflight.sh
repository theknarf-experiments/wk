#!/usr/bin/env bash
# Preflight for the LibreOffice Impress port. Probes only — compiles nothing,
# configures nothing, downloads nothing, and never touches src/.
#
# Run this BEFORE the first real build, and again whenever the machine changes.
# It answers four questions:
#
#   1. Is the wasi-sdk the scripts expect actually there, and does
#      -fwasm-exceptions really select its eh/ sysroot variant? (That second
#      half is not cosmetic: without eh/ you get the noeh libc++/libc++abi,
#      -lunwind does not resolve, and LibreOffice cannot throw.)
#   2. Do the host tools LibreOffice's configure hard-requires resolve, at the
#      versions it hard-requires? Two of them do NOT on stock macOS and each is
#      an AC_MSG_ERROR, not a warning.
#   3. Is src/ the pinned checkout, and are the structural patches there yet?
#   4. Does `configure --help` run — i.e. does aclocal+autoconf produce a
#      working configure on this host's autotools?
#
# Exit status: 0 if a build could be started, 1 if something would hard-fail.
# Optional/soft findings are reported but do not fail the run.
#
# Knobs: WK_LO_SKIP_CONFIGURE_HELP=1  skip (4); it is the only slow part
#                                     (aclocal+autoconf, ~30-60 s).
set -uo pipefail
cd "$(dirname "$0")"
LO_STAGE=preflight
# shellcheck source=common.sh
. ./common.sh

# Build the symlink farm before probing, so section 4 exercises the SAME narrow
# PATH the build stages use rather than the ambient one. A tool that resolves
# for you interactively and not for the build is exactly the failure this
# script exists to catch.
lo_link_toolbin

fail=0
warn=0
ok()   { printf '  \033[32mok\033[0m    %s\n' "$*"; }
bad()  { printf '  \033[31mFAIL\033[0m  %s\n' "$*"; fail=$((fail+1)); }
soft() { printf '  \033[33mwarn\033[0m  %s\n' "$*"; warn=$((warn+1)); }

echo "== 1. wasi-sdk =============================================================="
echo "WASI_SDK=$WASI_SDK"
for b in clang clang++ wasm-ld wasm-component-ld llvm-ar llvm-nm llvm-ranlib llvm-strip llvm-objdump; do
    if [ -x "$WASI_SDK/bin/$b" ]; then ok "bin/$b"; else bad "bin/$b missing"; fi
done
# wasi-sdk 34 ships no llvm-readelf and macOS has no readelf. Stated here so
# nobody spends an evening on configure's empty READELF; see common.sh.
[ -x "$WASI_SDK/bin/llvm-readelf" ] || soft "no llvm-readelf (expected; unused under DISABLE_DYNLOADING)"

for d in eh noeh; do
    if [ -d "$WASI_SDK/share/wasi-sysroot/lib/wasm32-wasip2/$d" ]; then
        ok "sysroot lib/wasm32-wasip2/$d"
    else
        bad "sysroot lib/wasm32-wasip2/$d missing"
    fi
done
for l in libc++.a libc++abi.a libunwind.a; do
    if [ -f "$WASI_SDK/share/wasi-sysroot/lib/wasm32-wasip2/eh/$l" ]; then ok "eh/$l"
    else bad "eh/$l missing"; fi
done

# The load-bearing check: prove the flag SELECTS eh/, don't assume it.
sel_eh="$("$WASI_SDK/bin/clang++" --target=wasm32-wasip2 -fwasm-exceptions -### -x c++ /dev/null 2>&1 \
            | tr ' ' '\n' | grep -c 'wasm32-wasip2/eh' || true)"
sel_noeh="$("$WASI_SDK/bin/clang++" --target=wasm32-wasip2 -### -x c++ /dev/null 2>&1 \
            | tr ' ' '\n' | grep -c 'wasm32-wasip2/noeh' || true)"
if [ "$sel_eh" -ge 2 ] && [ "$sel_noeh" -ge 2 ]; then
    ok "-fwasm-exceptions selects eh/ (and its absence selects noeh/)"
else
    bad "-fwasm-exceptions does not switch the libc++ variant (eh hits=$sel_eh noeh hits=$sel_noeh)"
fi

echo
echo "== 2. host tools ============================================================"
# The two hard ones first. Both are AC_MSG_ERROR in configure.ac and both fail
# on a stock macOS. Note the ORDER in which they bite: the BUILD-side
# sub-configure (configure.ac:6170) runs before the make check (:6907), so the
# make failure arrives indented under "Running the configure script for BUILD
# side failed" and reads like a cross-compilation problem. It is not.
if gm="$(lo_find_gnumake)"; then
    ok "GNU Make >= 4.2: $gm ($("$gm" --version | head -1))"
else
    bad "GNU Make >= 4.2 not found (configure.ac:6907). /usr/bin/make here is $(make --version 2>/dev/null | head -1). Fix: mise run deps"
fi
if gp="$(lo_find_gperf)"; then
    ok "gperf >= 3.1: $gp ($("$gp" --version | head -1))"
else
    bad "gperf >= 3.1 not found (configure.ac:8201). /usr/bin/gperf here is $(gperf --version 2>/dev/null | head -1). Fix: mise run deps"
fi

# Hard requirements that do resolve on a normal developer machine.
for t in perl python3 flex bison m4 autoconf aclocal automake zip xsltproc tar curl git sed awk; do
    p="$(command -v "$t" 2>/dev/null || true)"
    if [ -n "$p" ]; then ok "$t -> $p"; else bad "$t missing"; fi
done
# bison 2.3 (Apple's) is fine: configure.ac:12352 needs 2.0+, and the 2.4+
# requirement at :12348 is only for --enable-compiler-plugins, which the WASI
# host arm switches off.
if command -v bison >/dev/null 2>&1; then
    bv="$(bison --version | head -1 | sed -e 's@^[^0-9]*@@' -e 's@ .*@@')"
    if [ "$(echo "$bv" | awk -F. '{print $1*1000+$2}')" -ge 2000 ]; then
        ok "bison $bv >= 2.0"
    else
        bad "bison $bv < 2.0 (configure.ac:12352)"
    fi
fi

# Soft: absent but survivable, each for a specific reason.
command -v ccache >/dev/null 2>&1 \
    && ok "ccache -> $(command -v ccache)" \
    || soft "ccache absent. Not fatal, but this is a two-stage build of one of the largest C++ codebases in existence on $LO_JOBS cores: without it every configure-flag experiment re-pays the whole native bootstrap. Fix: mise install (it is pinned in mise.toml [tools])"
command -v meson >/dev/null 2>&1 \
    && ok "meson -> $(command -v meson)" \
    || soft "meson absent. NOT fatal: configure.ac:14751 warns and falls back to the internal meson-1.8.3 from download.lst. cairo/pixman/harfbuzz build through it."
command -v nasm >/dev/null 2>&1 \
    && ok "nasm -> $(command -v nasm)" \
    || soft "nasm absent. NOT needed: configure.ac:10049 only probes for it when host_cpu is x86-ish, and it only feeds libjpeg-turbo SIMD. The brief was wrong about this one."
# python3 version: the BUILD side runs AM_PATH_PYTHON([3.7]) and solenv's
# generators run on the host interpreter. 3.14 is newer than anything upstream
# tests, but native-code.py -g core -g draw has been run on it here (exit 0).
command -v python3 >/dev/null 2>&1 && ok "python3 $(python3 --version 2>&1 | awk '{print $2}')"

echo
echo "== 3. source tree and patches ==============================================="
if [ -d "$LO_SRC/.git" ]; then
    tag="$(git -C "$LO_SRC" describe --tags 2>/dev/null || echo '?')"
    [ "$tag" = "$LO_TAG" ] && ok "src/ at $tag" || bad "src/ at '$tag', expected '$LO_TAG'"
    if [ -z "$(git -C "$LO_SRC" status --porcelain 2>/dev/null)" ]; then
        ok "src/ is clean (no patches applied)"
    else
        soft "src/ has local modifications ($(git -C "$LO_SRC" status --porcelain | wc -l | tr -d ' ') paths) — patches applied, or something was edited in place"
    fi
else
    bad "src/ is not a LibreOffice checkout"
fi
if ls "$LO_PATCHES"/core-*.patch >/dev/null 2>&1; then
    ok "patches/: $(ls "$LO_PATCHES"/core-*.patch | wc -l | tr -d ' ') patch(es)"
else
    soft "patches/ is empty. Expected at this stage — nothing can configure yet. build-configure.sh will refuse with the exact list of what to write; see PORTING.md."
fi
# config.sub parses the triple. Cheap, and it isolates a class of failure to
# configure.ac's own case statement rather than to autoconf's machinery.
if [ -f "$LO_SRC/config.sub" ]; then
    got="$(sh "$LO_SRC/config.sub" "$LO_HOST_TRIPLE" 2>/dev/null || true)"
    [ "$got" = "$LO_HOST_TRIPLE" ] \
        && ok "config.sub $LO_HOST_TRIPLE -> $got" \
        || bad "config.sub rejected $LO_HOST_TRIPLE (got '$got')"
fi

echo
echo "== 4. autotools / configure --help =========================================="
if [ "${WK_LO_SKIP_CONFIGURE_HELP:-0}" = "1" ]; then
    soft "skipped (WK_LO_SKIP_CONFIGURE_HELP=1)"
else
    # Out of tree, in a throwaway directory, so src/ is untouched. autogen.sh
    # runs aclocal (with -I m4 -I m4/mac on Darwin) then autoconf, then
    # ./configure --help, and exits. Bare `autoconf` is NOT equivalent: it
    # emits a smaller configure whose LO-local m4 macros were never expanded
    # and which dies at runtime on `libo_FUZZ_ARG_ENABLE(android-editing,`.
    probe="$LO_LOGDIR/preflight-configure-help"
    rm -rf "$probe"; mkdir -p "$probe"
    log="$LO_LOGDIR/preflight-configure-help.log"
    if (cd "$probe" && env PATH="$LO_HOST_PATH" "$LO_SRC/autogen.sh" --help) >"$log" 2>&1; then
        # Every option build-configure.sh's flag wall depends on, checked by the
        # spelling `configure --help` actually uses. Note --enable-mergelibs:
        # we pass --disable-mergelibs, which autoconf accepts (it is the same
        # AC_ARG_ENABLE and configure.ac:15805 handles the value "no"), but
        # only the --enable form appears in the help text.
        for f in --with-wasm-module --enable-wasm-strip --enable-cairo-rgba \
                 --enable-customtarget-components --disable-dynamic-loading \
                 --enable-mergelibs --with-external-tar \
                 --with-build-platform-configure-options --with-parallelism; do
            grep -q -- "$f" "$log" && ok "configure --help lists $f" || bad "configure --help does not list $f"
        done
        ok "log: $log"
    else
        bad "autogen.sh --help failed; see $log"
    fi
    rm -rf "$probe"
fi

echo
echo "============================================================================"
if [ "$fail" -gt 0 ]; then
    echo "preflight: $fail blocking, $warn advisory. A build cannot start yet."
    exit 1
fi
echo "preflight: 0 blocking, $warn advisory."
echo "Next: ./build-configure.sh  (it will still refuse until patches/ has the"
echo "      configure.ac host arm and the gbuild platform file — by design)."
