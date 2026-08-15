#!/usr/bin/env bash
# Build UNMODIFIED upstream GNU coreutils as ONE wasm32-wasip2 component: a
# real multicall binary, the way busybox works.
#
# Why multicall: WASI has no fork/exec, so a node can only ever run one
# program. coreutils' own --enable-single-binary=symlinks builds every tool
# into a single executable that dispatches on argv, which is exactly the shape
# a fork-less sandbox needs. Invoke as:
#
#     coreutils --coreutils-prog=NAME NAME [args...]
#
# (the name appears twice — the third argument is what the tool sees as
# argv[0]).
#
# No coreutils or gnulib source is edited. Everything WASI lacks is supplied by
# compat/: header wrappers that chain to the real ones with #include_next, plus
# compat.c implementing the missing calls. The interesting ones, each of which
# was a build failure first:
#
#   * mount table — gnulib's mountlist (required unconditionally, for df) is
#     built around glibc's mtab reader. compat/mntent.h + an always-empty
#     table: df builds and honestly reports nothing mounted.
#   * users/groups — no user database in WASI; a single synthetic "wk" user so
#     ls -l/id/whoami print something coherent.
#   * signals — wasi-libc keeps the signal-set API behind
#     `__wasilibc_unmodified_upstream`. It has to *exist*, because sort.c
#     redefines `sigset_t` to `int` when the platform looks signal-less, which
#     then collides with the real sigset_t everywhere else. Declared and inert.
#     gnulib's own sigaction replacement refuses to build where SIGCHLD is
#     defined (WASI defines the number), so we claim sigaction and supply it.
#   * opendirat — a genuine name collision: wasi-libc has a 2-argument
#     `opendirat` extension, gnulib a 4-argument one. compat/dirent.h renames
#     wasi-libc's as its header is pulled in.
#   * rlimits — wasi-libc ships <sys/resource.h> with its body disabled.
#     compat supplies it, guarded on RLIMIT_DATA (the sentinel sort.c probes),
#     and getrlimit returns modest *finite* values: several tools size their
#     buffers from these, and RLIM_INFINITY makes that arithmetic produce
#     absurd numbers ("memory exhausted" before any work happens).
#   * PTRDIFF_MAX — the one that broke *everything*. A cross build can't run
#     the type-size probes, so configure recorded BITSIZEOF_PTRDIFF_T as 0 and
#     substituted gnulib's own <stdint.h>, making PTRDIFF_MAX ~0. gnulib's
#     rpl_malloc rejects any size for which xalloc_oversized() is true, so
#     *every* allocation returned NULL and every tool that allocates died with
#     "memory exhausted" (echo and --help, which don't allocate, worked fine).
#     wasi-libc's stdint.h is correct: gl_cv_header_working_stdint_h=yes.
#   * getcwd — gnulib substitutes its own, which walks up the tree with ".."
#     and readdir; that can't terminate on wk's vfs, so pwd/ls/stat/du hung
#     allocating. wasi-libc's getcwd is fine: gl_cv_func_getcwd_*=yes.
#   * processes — fork/exec/pipe/wait/chroot/priority all fail with ENOSYS.
#     They're referenced by env/nohup/timeout/chroot/nice, which this
#     configuration keeps out of the single binary (`ls single_binary_progs`
#     in the generated Makefile shows what's in). We build only the multicall
#     target, so the standalone copies of those — plus stty and pinky, which
#     need termios and utmp — are never linked.
#
# Requires wasi-sdk (WASI_SDK, default ~/wasi-sdk). Source is fetched (and
# cached) under coreutils-<ver>/ on first run.
#
# Known cosmetic wart: error messages print "(null):" instead of the tool name,
# because gnulib's error() resolves the program name differently here.
set -euo pipefail
cd "$(dirname "$0")"

WASI_SDK="${WASI_SDK:-$HOME/wasi-sdk}"
CU_VER=9.5
SRC="coreutils-$CU_VER"
COMPAT="$PWD/compat"

# wasi-sdk's clang runs wasm-opt as an optional post-link step; the wasm-opt on
# PATH can't parse the new exnref EH we emit (same as the lua/php plugins).
BUILD_PATH="$WASI_SDK/bin:/usr/bin:/bin"

if [ ! -d "$SRC" ]; then
    echo "fetching GNU coreutils $CU_VER..."
    curl -fsSL "https://ftp.gnu.org/gnu/coreutils/$SRC.tar.xz" -o "$SRC.tar.xz"
    tar xf "$SRC.tar.xz"
    rm -f "$SRC.tar.xz"
fi

CFLAGS="--target=wasm32-wasip2 -O2 -I$COMPAT \
    -mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false \
    -DHAVE_GETRLIMIT=1 \
    -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_PROCESS_CLOCKS -D_WASI_EMULATED_GETPID"

# The shim is linked in as an object (it defines what wasi-libc leaves out).
# exit_shim.o routes exit through wasi:cli/exit.exit-with-code so a tool's real
# status (e.g. `false` -> 1, `test` -> 1/0) reaches the host instead of the
# boolean ok/err the default exit() collapses to.
for src in compat exit_shim; do
    "$WASI_SDK/bin/clang" --target=wasm32-wasip2 -O2 -I"$COMPAT" \
        -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_PROCESS_CLOCKS -D_WASI_EMULATED_GETPID \
        -c "$COMPAT/$src.c" -o "$COMPAT/$src.o"
done

LDFLAGS="$COMPAT/compat.o $COMPAT/exit_shim.o -lsetjmp \
    -lwasi-emulated-signal -lwasi-emulated-process-clocks -lwasi-emulated-getpid"

cd "$SRC"
# configure's answers describe the toolchain, so they go stale when it moves:
# a sysroot that gains a function leaves config.h still saying it is missing,
# and the tree silently keeps using a replacement. Reconfigure on a change.
TOOLCHAIN="$("$WASI_SDK/bin/clang" --version | head -1)"
if [ -f Makefile ] && [ "$(cat .wk-toolchain 2>/dev/null)" != "$TOOLCHAIN" ]; then
    echo "toolchain changed since configure; reconfiguring coreutils"
    env PATH="$BUILD_PATH" make distclean >/dev/null 2>&1 || true
    rm -f .wk-toolchain
fi
if [ ! -f Makefile ]; then
    # Cache variables stand in for the runtime probes a cross build can't run,
    # and for the functions compat.c supplies (see the header above).
    CC="$WASI_SDK/bin/clang" \
    AR="$WASI_SDK/bin/llvm-ar" \
    RANLIB="$WASI_SDK/bin/llvm-ranlib" \
    CFLAGS="$CFLAGS" \
    LDFLAGS="$LDFLAGS" \
    ./configure --host=wasm32-wasi \
        --enable-single-binary=symlinks \
        --enable-no-install-program=env,nohup,timeout,stdbuf,chroot,runcon,nice,who,users,pinky,uptime,df,stty,kill \
        --disable-nls --disable-acl --disable-xattr \
        --without-selinux --without-openssl \
        ac_cv_func_getmntent=yes fu_cv_sys_mounted_getmntent1=yes \
        ac_cv_func_geteuid=yes ac_cv_func_getuid=yes \
        ac_cv_type_sigset_t=yes gl_cv_type_sigset_t=yes \
        ac_cv_func_sigprocmask=yes ac_cv_func_sigaction=yes \
        ac_cv_member_struct_sigaction_sa_sigaction=yes \
        ac_cv_func_pipe=yes \
        ac_cv_header_sys_resource_h=yes ac_cv_func_getrlimit=yes \
        gl_cv_func_getcwd_null=yes gl_cv_func_getcwd_posix_signature=yes \
        gl_cv_func_getcwd_path_max=yes ac_cv_func_getcwd=yes \
        gl_cv_header_working_stdint_h=yes
    printf '%s' "$TOOLCHAIN" > .wk-toolchain
fi

# Two passes. The first (-k, failures expected) generates the headers and
# libraries the tree needs; it also *attempts* standalone copies of the
# excluded programs, which cannot build here (stty needs termios, pinky needs
# utmp) — that's fine, we don't link them. The second builds the one target we
# actually want: the multicall binary.
JOBS="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)"
env PATH="$BUILD_PATH" make -k CFLAGS="$CFLAGS" LDFLAGS="$LDFLAGS" -j"$JOBS" || true
# make tracks neither LDFLAGS nor the shim objects, so drop the binary to force
# the final link every run (it is only the link — a second or two).
rm -f src/coreutils
env PATH="$BUILD_PATH" make src/coreutils \
    CFLAGS="$CFLAGS" LDFLAGS="$LDFLAGS" -j"$JOBS"

cd ..
cp "$SRC/src/coreutils" coreutils.wasm
echo "built plugins/coreutils/coreutils.wasm (GNU coreutils $CU_VER, wasm32-wasip2 component)"
