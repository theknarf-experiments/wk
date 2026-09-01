#!/usr/bin/env bash
# Stage 0 of the cross build: the wasip2 thread shim.
#
# One C file, one static archive, about a second. It is a separate stage rather
# than part of build-lo.sh because solenv/gbuild/platform/WASI_INTEL_GCC.mk
# names the archive on EVERY link line and hard-errors if it is missing — so it
# has to exist before make is first invoked, including for `make cross-toolset`
# (gbuild includes the host platform file there too).
#
# What is in the archive and why it cannot live in LibreOffice's tree: see
# shim/wk-wasi-threads.c. The short version is that wasi-libc's
# pthread_cond_timedwait returns ENOTSUP, libc++ turns that into abort(), and
# LibreOffice's main event loop and every osl::Condition sit on it. The
# override has to cover libc++, libc++abi and the ~149 external libraries as
# well as LibreOffice's own code, and a patch to src/ could not.
#
# Idempotent: rebuilds only when the source is newer than the archive.
set -uo pipefail
cd "$(dirname "$0")"
LO_STAGE=shim
# shellcheck source=common.sh
. ./common.sh

SHIM_SRC="$LO_ROOT/shim/wk-wasi-threads.c"
SHIM_OBJ="$LO_ROOT/shim/wk-wasi-threads.o"
SHIM_LIB="$LO_SHIM_LIB"   # common.sh owns the path; the makefile patch uses the same one

[ -f "$SHIM_SRC" ] || lo_die "missing $SHIM_SRC"

if [ -f "$SHIM_LIB" ] && [ "$SHIM_LIB" -nt "$SHIM_SRC" ]; then
    echo "=== shim: up to date ($SHIM_LIB)"
    exit 0
fi

echo "=== shim: $SHIM_SRC -> $SHIM_LIB"

# The same target and exception flags as everything else in the build. The shim
# itself contains no C++ and no setjmp, so the EH flags change nothing about
# its code — they are passed anyway so that a `grep -r fwasm-exceptions` over
# this plugin turns up one flag set rather than two, and so that a future
# addition to this file cannot silently become the one object built against the
# noeh sysroot.
#
# LO_CC is not reused here: it is the toolwrap wrapper that strips
# --start-group for gbuild's link lines, and this is a plain compile.
"$WASI_SDK/bin/clang" --target="$LO_HOST_TRIPLE" $LO_EH_FLAGS \
    -O2 -Wall -Wextra -c "$SHIM_SRC" -o "$SHIM_OBJ" || lo_die "compile failed"

rm -f "$SHIM_LIB"
"$LO_AR" crs "$SHIM_LIB" "$SHIM_OBJ" || lo_die "ar failed"

# Prove the archive defines what it claims to, rather than trusting that the
# file compiled. A shim that silently stopped exporting pthread_cond_timedwait
# would hand the abort back at run time, months later.
for sym in pthread_cond_timedwait pthread_cond_wait __wrap___cxa_throw __wrap_malloc; do
    "$LO_NM" --defined-only "$SHIM_LIB" | grep -q " T $sym\$" \
        || lo_die "$SHIM_LIB does not define $sym"
done
echo "    defines pthread_cond_timedwait, pthread_cond_wait, __wrap___cxa_throw, __wrap_malloc ($(wc -c <"$SHIM_LIB" | tr -d ' ') bytes)"
