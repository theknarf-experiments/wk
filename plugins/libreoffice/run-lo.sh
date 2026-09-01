#!/usr/bin/env bash
# Run the built soffice.bin under wasmtime, with the mounts it cannot do without.
#
# This exists because the invocation is not guessable and getting it subtly
# wrong looks like a LibreOffice bug rather than a missing directory. Three
# things have to be true, and each of them failed at least once during the port:
#
#   /instdir      the install tree, at exactly that path. Nothing at run time can
#                 discover where it is -- no dladdr, no realpath, and wasmtime
#                 hands a component only the BASENAME in argv[0] -- so
#                 cppuhelper/source/paths.cxx and sal/osl/unx/process_impl.cxx
#                 both hardcode file:///instdir/program, and fontconfig was
#                 compiled with --with-baseconfigdir=/instdir/share/fontconfig.
#                 Mount it anywhere else and the failure surfaces as a missing
#                 UNO service, not as a missing path.
#   /work         somewhere writable. LibreOffice creates its user profile
#                 before it will open anything.
#   /tmp          also writable, and separate. Without it osl's mkdir walks up
#                 looking for a parent that exists, and every path outside a
#                 preopened directory answers ENOENT -- see
#                 patches/core-0019-mkdir-terminates.patch for what that used
#                 to do to the stack.
#
# Usage: ./run-lo.sh --convert-to pdf --outdir /work /work/test.odp
#        ./run-lo.sh --version
#        WORK=/tmp/mydir ./run-lo.sh ...
#
# Everything after the script name is passed through, so the guest-visible paths
# are /work/... and /instdir/..., not host paths.
#
# Knobs, all from shim/wk-wasi-threads.c and solenv/gbuild/platform/WASI_INTEL_GCC.mk:
#   SAL_LOG=+WARN                 LibreOffice's own diagnostics (needs a build
#                                 configured with WK_LO_DEBUG=log; a release
#                                 build compiles SAL_WARN out entirely)
#   WK_LO_TRACE_THROW=1           print every C++ throw's mangled type
#   WK_LO_TRAP_THROW=bad_alloc    abort AT the throw, so wasmtime's backtrace
#                                 names the code that threw rather than wherever
#                                 std::terminate happened to be reached
#   WK_LO_TRACE_ALLOC=<bytes>     print allocations at least this large
set -uo pipefail
cd "$(dirname "$0")"
LO_STAGE=run
# shellcheck source=common.sh
. ./common.sh

SOFFICE="$LO_BUILD/instdir/program/soffice.bin"
[ -f "$SOFFICE" ] || lo_die "$SOFFICE does not exist — run ./build-lo.sh"

WORK="${WORK:-$LO_ROOT/work}"
TMP="$WORK/.tmp"
mkdir -p "$WORK" "$TMP" || lo_die "cannot create $WORK"

command -v wasmtime >/dev/null 2>&1 || lo_die "wasmtime not on PATH"

# UserInstallation is passed as a -env: argument rather than an environment
# variable because that is the only form rtl::Bootstrap reads before the ini
# files are found, and the profile has to exist before the first service is
# created.
exec wasmtime run \
    --dir "$LO_BUILD/instdir::/instdir" \
    --dir "$WORK::/work" \
    --dir "$TMP::/tmp" \
    --env HOME=/work \
    --env TMPDIR=/tmp \
    ${SAL_LOG:+--env "SAL_LOG=$SAL_LOG"} \
    ${WK_LO_TRACE_THROW:+--env "WK_LO_TRACE_THROW=$WK_LO_TRACE_THROW"} \
    ${WK_LO_TRAP_THROW:+--env "WK_LO_TRAP_THROW=$WK_LO_TRAP_THROW"} \
    ${WK_LO_TRACE_ALLOC:+--env "WK_LO_TRACE_ALLOC=$WK_LO_TRACE_ALLOC"} \
    "$SOFFICE" \
    -env:UserInstallation=file:///work/.profile \
    "$@"
