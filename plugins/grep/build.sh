#!/usr/bin/env bash
# Build UNMODIFIED upstream GNU grep as a wasm32-wasip2 component, so a shell
# script run under bash (or Bun's child_process) can `grep` for real.
#
# grep is the first of the "not coreutils" GNU tools a base image is expected
# to have (sed and awk are the obvious next ones). Unlike coreutils it is a
# single program, not a multicall binary, so there are no argv[0] games: the
# shell's PATH search finds /bin/grep (a symlink onto grep.wasm) and runs it.
# (egrep/fgrep are separate wrapper scripts upstream, not an argv[0] personality
# of this binary, so they are not provided — use `grep -E` / `grep -F`.)
#
# grep never fork()s — even `-r` walks the tree with fts(3), in-process — so the
# WASI "no fork/exec" gap that shaped the bash and coreutils ports doesn't
# touch it.
#
# WORKS: file arguments and recursion — `grep PATTERN file`, `grep -rl P dir`,
# every match option (-i -v -c -n -o -w -E, egrep/fgrep). These are verified.
#
# NOT YET: reading standard input — `... | grep P`, `grep P < file`,
# `execSync("grep P", {input})`. grep rejects the descriptor wk:exec hands it as
# stdin with "(standard input): Invalid argument" before reading a byte. Unlike
# `cat`/`sort`/`tr`, which read that same stream fine, grep introspects the
# descriptor up front (fstat reports filetype "unknown", and the stream is not
# seekable) and bails. Pin down and fix that stdin path before relying on a
# piped grep; for now, grep a file.
#
# What grep shares with coreutils is gnulib, and gnulib's cross-build needs the
# same answers a run-the-probe configure can't get here:
#
#   * PTRDIFF_MAX — a cross build can't size types, so configure substitutes
#     gnulib's <stdint.h> with PTRDIFF_MAX ~0; gnulib's rpl_malloc then rejects
#     every allocation as oversized and every run dies "memory exhausted".
#     wasi-libc's stdint.h is correct: gl_cv_header_working_stdint_h=yes.
#   * getcwd — gnulib's replacement walks up with ".." + readdir, which can't
#     terminate on wk's vfs. wasi-libc's getcwd is fine.
#
# The shims are the same two coreutils links in: exit_shim routes exit() through
# wasi:cli/exit.exit-with-code so grep's status (0 match / 1 no-match / 2 error)
# survives instead of collapsing to ok/err; chdir_shim chdir()s to
# __WK_EXEC_CWD at startup so `cd /x && grep ... file` looks where the shell is.
#
# Requires wasi-sdk (WASI_SDK). Source is fetched (and cached) on first run.
set -euo pipefail
cd "$(dirname "$0")"

WASI_SDK="${WASI_SDK:-$HOME/wasi-sdk}"
GREP_VER=3.11
SRC="grep-$GREP_VER"
COMPAT="$PWD/compat"

# wasm-opt on PATH can't parse the exnref EH clang emits; keep it out (as the
# coreutils/lua/php plugins do).
BUILD_PATH="$WASI_SDK/bin:/usr/bin:/bin"

if [ ! -d "$SRC" ]; then
    echo "fetching GNU grep $GREP_VER..."
    curl -fsSL "https://ftp.gnu.org/gnu/grep/$SRC.tar.xz" -o "$SRC.tar.xz"
    tar xf "$SRC.tar.xz"
    rm -f "$SRC.tar.xz"
fi

CFLAGS="--target=wasm32-wasip2 -O2 -I$COMPAT \
    -mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false \
    -Wno-implicit-function-declaration \
    -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_PROCESS_CLOCKS -D_WASI_EMULATED_GETPID"

for src in compat exit_shim chdir_shim; do
    "$WASI_SDK/bin/clang" --target=wasm32-wasip2 -O2 -I"$COMPAT" \
        -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_PROCESS_CLOCKS -D_WASI_EMULATED_GETPID \
        -c "$COMPAT/$src.c" -o "$COMPAT/$src.o"
done

LDFLAGS="$COMPAT/compat.o $COMPAT/exit_shim.o $COMPAT/chdir_shim.o -lsetjmp \
    -lwasi-emulated-signal -lwasi-emulated-process-clocks -lwasi-emulated-getpid"

cd "$SRC"
TOOLCHAIN="$("$WASI_SDK/bin/clang" --version | head -1)"
if [ -f Makefile ] && [ "$(cat .wk-toolchain 2>/dev/null)" != "$TOOLCHAIN" ]; then
    echo "toolchain changed since configure; reconfiguring grep"
    env PATH="$BUILD_PATH" make distclean >/dev/null 2>&1 || true
    rm -f .wk-toolchain
fi
if [ ! -f Makefile ]; then
    CC="$WASI_SDK/bin/clang" \
    AR="$WASI_SDK/bin/llvm-ar" \
    RANLIB="$WASI_SDK/bin/llvm-ranlib" \
    CFLAGS="$CFLAGS" \
    LDFLAGS="$LDFLAGS" \
    ./configure --host=wasm32-wasi \
        --disable-nls --disable-perl-regexp \
        gl_cv_header_working_stdint_h=yes \
        gl_cv_func_getcwd_null=yes gl_cv_func_getcwd_posix_signature=yes \
        gl_cv_func_getcwd_path_max=yes ac_cv_func_getcwd=yes \
        ac_cv_type_sigset_t=yes gl_cv_type_sigset_t=yes \
        ac_cv_func_sigprocmask=yes ac_cv_func_sigaction=yes \
        ac_cv_member_struct_sigaction_sa_sigaction=yes
    printf '%s' "$TOOLCHAIN" > .wk-toolchain
fi

# make tracks neither LDFLAGS nor the shim objects; drop the binary to force the
# final link every run (a second or two).
rm -f src/grep
env PATH="$BUILD_PATH" make CFLAGS="$CFLAGS" LDFLAGS="$LDFLAGS" \
    -j"$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)"

cd ..
cp "$SRC/src/grep" grep.wasm
echo "built plugins/grep/grep.wasm (GNU grep $GREP_VER, wasm32-wasip2 component)"
