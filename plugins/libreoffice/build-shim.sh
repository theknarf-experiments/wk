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

# A skip rather than an exit: there is a second shim below, and an early exit
# here used to mean the graphics one was silently never built.
if [ -f "$SHIM_LIB" ] && [ "$SHIM_LIB" -nt "$SHIM_SRC" ]; then
    echo "=== shim: up to date ($SHIM_LIB)"
    SHIM_UP_TO_DATE=1
fi

if [ "${SHIM_UP_TO_DATE:-0}" != 1 ]; then
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
fi

# ---------------------------------------------------------------------------
# Stage 0b: the wk graphics shim.
#
# plugins/gfx-compat is the C API every wk GUI port draws through — doom,
# quake, mupdf, netsurf and the Qt QPA all include the same wkgfx.h — and
# vcl/wk is now one of them. It is built here, beside the thread shim, for the
# same reason: WASI_INTEL_GCC.mk names the archive on every link line, so it
# has to exist before make is first invoked.
#
# Two objects, not one archive, because they are not the same kind of thing:
#
#   libwkgfx.a                 wkgfx.c and the wit-bindgen output. Ordinary
#                              code, pulled in by reference from vcl/wk.
#   wkgfx_component_type.o     linked DIRECTLY, never through the archive: it
#                              exists only for a custom section describing the
#                              component's imports, nothing references a symbol
#                              in it, and a static-archive member nothing
#                              references is exactly what a linker drops.
#                              plugins/qt/qpa/CMakeLists.txt:36-39 says the
#                              same thing in its own words.
#
# gen/ is regenerated every build, like every other gfx-compat consumer does
# it. It is shared and disposable and never the source of truth.
GFXCOMPAT="$LO_ROOT/../gfx-compat"
GFXGEN="$GFXCOMPAT/gen"
GFX_LIB="$LO_ROOT/shim/libwkgfx.a"

[ -d "$GFXCOMPAT" ] || lo_die "missing $GFXCOMPAT (plugins/gfx-compat)"
command -v wit-bindgen >/dev/null 2>&1 || lo_die "wit-bindgen not on PATH (mise install)"

echo "=== gfx shim: wit-bindgen (wkgfx world)"
mkdir -p "$GFXGEN"
wit-bindgen c --world wkgfx "$GFXCOMPAT/wit" --out-dir "$GFXGEN" >/dev/null \
    || lo_die "wit-bindgen failed"

# Distinct member names: gfx-compat/wkgfx.c and gen/wkgfx.c share a basename,
# and `ar r` replaces members BY basename -- so naming both objects wkgfx.o
# silently produces an archive with one of them in it.
rm -f "$GFX_LIB"
gfx_obj() {
    "$WASI_SDK/bin/clang" --target="$LO_HOST_TRIPLE" $LO_EH_FLAGS \
        -O2 -I"$GFXCOMPAT" -I"$GFXGEN" -c "$1" -o "$2" || lo_die "compile failed: $1"
    "$LO_AR" crs "$GFX_LIB" "$2" || lo_die "ar failed"
}
gfx_obj "$GFXCOMPAT/wkgfx.c" "$LO_ROOT/shim/wkgfx-shim.o"
gfx_obj "$GFXGEN/wkgfx.c"    "$LO_ROOT/shim/wkgfx-bindings.o"
[ -f "$GFXGEN/wkgfx_component_type.o" ] || lo_die "$GFXGEN/wkgfx_component_type.o missing"

for sym in wkgfx_open wkgfx_present wkgfx_poll_event; do
    "$LO_NM" --defined-only "$GFX_LIB" | grep -q " T $sym\$" \
        || lo_die "$GFX_LIB does not define $sym"
done
echo "    defines wkgfx_open, wkgfx_present, wkgfx_poll_event ($(wc -c <"$GFX_LIB" | tr -d ' ') bytes)"
