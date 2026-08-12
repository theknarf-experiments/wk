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
#   * F_DUPFD — undefined in WASI, so gnulib defines it as 1, colliding with
#     WASI's F_GETFD. POSIX's real value is 0; -DF_DUPFD=0 is both correct and
#     the fix.
#   * rlimits — wasi-libc ships <sys/resource.h> with its body disabled.
#     compat supplies it, guarded on RLIMIT_DATA (the sentinel sort.c probes),
#     and getrlimit returns modest *finite* values: several tools size their
#     buffers from these, and RLIM_INFINITY makes that arithmetic produce
#     absurd numbers ("memory exhausted" before any work happens).
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
# KNOWN GAP: `ls DIR` currently dies with "memory exhausted" while reading a
# directory (ls --help/--version and cat/echo/seq-style tools are fine), so the
# readdir path still needs a look. Everything else here is working.
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
    -DF_DUPFD=0 -DHAVE_LCHOWN=0 -DHAVE_GETRLIMIT=1 \
    -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_PROCESS_CLOCKS -D_WASI_EMULATED_GETPID"

# The shim is linked in as an object (it defines what wasi-libc leaves out).
"$WASI_SDK/bin/clang" --target=wasm32-wasip2 -O2 -I"$COMPAT" \
    -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_PROCESS_CLOCKS -D_WASI_EMULATED_GETPID \
    -c "$COMPAT/compat.c" -o "$COMPAT/compat.o"

LDFLAGS="$COMPAT/compat.o -lsetjmp \
    -lwasi-emulated-signal -lwasi-emulated-process-clocks -lwasi-emulated-getpid"

cd "$SRC"
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
        ac_cv_header_sys_resource_h=yes ac_cv_func_getrlimit=yes
fi

# Only the multicall target: `make all` would also build standalone copies of
# the excluded programs (stty needs termios, pinky needs utmp).
env PATH="$BUILD_PATH" make src/coreutils \
    CFLAGS="$CFLAGS" LDFLAGS="$LDFLAGS" \
    -j"$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)"

cd ..
cp "$SRC/src/coreutils" coreutils.wasm
echo "built plugins/coreutils/coreutils.wasm (GNU coreutils $CU_VER, wasm32-wasip2 component)"
