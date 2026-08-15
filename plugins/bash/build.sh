#!/usr/bin/env bash
# Build UNMODIFIED upstream GNU bash into a wasm component that wk runs in a
# terminal node — the real bash(1), not a shell-alike.
#
# What works: the shell language (variables, arithmetic, [[ ]], for/while/case,
# functions, arrays, every builtin) *and running external commands* — the
# latter through wk's `wk:exec` capability, since WASI has no fork/exec.
# patches/wk-0001 replaces the fork+exec in execute_disk_command() with a
# synchronous run that reports the status the shell would have waited for.
# Verified in a node: `ls -1 /`, `mkdir -p /work && echo ok` (with the new
# directory visible to the next command), `$?` propagation, and
# "command not found" with status 127 for a genuinely missing command.
#
# Command *names* are ordinary symlinks onto the coreutils multicall binary —
# `/bin/ls -> coreutils.wasm` — which is exactly how coreutils installs itself
# everywhere else. wk's filesystem supports real symlinks, so bash's own PATH
# search finds them and argv[0] stays "ls", which is what coreutils dispatches
# on. (An earlier version of this build used a lookup table instead, because
# the vfs had no links; that hack is gone.)
#
# What still cannot work, and why — both are WASI gaps, not port bugs:
#   * pipelines and command substitution need pipe() and a second process;
#     they fail cleanly ("pipe error: Function not implemented").
#   * redirection (`> file`) needs dup()/dup2() to save and restore stdout.
#     preview1 offers `fd_renumber`, which *moves* a descriptor rather than
#     copying it, so bash's save/restore is impossible. For builtins this
#     errors ("cannot duplicate fd"); for external commands the output simply
#     goes to the shell's own stdout.
#
# Targets wasm32-**wasip2**, which links a component directly (no adapter) and
# matters for two reasons beyond tidiness:
#   * the file-descriptor table lives in wasi-libc, in guest memory, rather
#     than inside the prebuilt preview1 adapter — so dup/dup2/pipe become
#     things this build can implement, which is the last thing standing
#     between bash and redirection + pipelines.
#   * wasip2 has real sockets, so bash's /dev/tcp support builds against
#     headers that exist and its connections ride wk's fabric.
# It still reuses ../tty-compat for real termios over `wk:tty/control`; the
# shim's poll/read overrides are wasip1-only (see its #ifndef __wasip2__) and
# a shell doesn't need them.
#
# No bash source is edited. The build knowledge, each item a failure first:
#   * host build tools — bash compiles mksignames/mksyntax for the *build*
#     machine; without CC_FOR_BUILD/CFLAGS_FOR_BUILD the wasm flags (and the
#     WASI config.h) leak into them and they fail against macOS headers.
#   * bash_cv_signal_vintage=posix — the probe can't run, so bash falls back to
#     v7 signals and calls sigblock/sigsetmask, which don't exist. compat/
#     provides the POSIX API (inert: nothing can signal a wasm guest).
#   * ac_cv_have_sig_atomic_t=yes — otherwise config.h does
#     `#define sig_atomic_t int`, which also breaks the *host* tools.
#   * LDFLAGS needs --target too. bash's link step runs $(CC) $(LDFLAGS)
#     *without* CFLAGS, so leaving the target off there silently links against
#     the default (wasip1) sysroot — which has no sockets, and reports
#     `undefined symbol: connect` for symbols that plainly exist in wasip2's
#     libc.a.
#   * signal_names — bash's generated signames.h holds an initialized array,
#     and both trap.o and signames.o carry it. Upstream relies on -fcommon
#     merging them; -fcommon makes this LLVM crash in wasm codegen, so link
#     only trap.o's copy (SIGNAMES_O=).
#   * getcwd — bash_cv_getcwd_malloc=yes keeps bash's own replacement out of
#     libsh, where it collided with wasi-libc's.
#   * main's signature — bash still uses the K&R three-argument
#     `main (argc, argv, env)`. clang only aliases `main` to wasi-libc's
#     expected `__main_argc_argv` for the two-argument form, so the reference
#     stays unresolved and the module traps instantly at
#     `undefined_weak:main`. compat.c supplies the adapter (bash's own
#     NO_MAIN_ENV_ARG path is broken upstream — its body still uses `env`).
#   * dlopen — `enable -f` loads builtins from shared objects; there is no
#     dynamic linker, so compat.c fails them cleanly.
#   * strvec_from_word_list(words, 0, ...) *aliases* the word list's strings
#     rather than copying them, so the patch frees only the vector — freeing
#     the elements corrupts the command name bash is still holding.
#
# Requires wasi-sdk (WASI_SDK), wasm-tools, and wit-bindgen (for tty-compat's
# bindings). Source is fetched (and cached) under bash-<ver>/ on first run.
set -euo pipefail
cd "$(dirname "$0")"

WASI_SDK="${WASI_SDK:-$HOME/wasi-sdk}"
BASH_VER=5.2.37
SRC="bash-$BASH_VER"
COMPAT="$PWD/compat"
TTY="$PWD/../tty-compat"
TTYGEN="$TTY/gen"
EXEC="$PWD/../exec-compat"
EXECGEN="$EXEC/gen"
PIPE="$PWD/../pipe-compat"

BUILD_PATH="$WASI_SDK/bin:/usr/bin:/bin"
if [ ! -d "$SRC" ]; then
    echo "fetching GNU bash $BASH_VER..."
    curl -fsSL "https://ftp.gnu.org/gnu/bash/$SRC.tar.gz" -o "$SRC.tar.gz"
    tar xzf "$SRC.tar.gz"
    rm -f "$SRC.tar.gz"
    # Run external commands through wk:exec instead of fork+exec, and give
    # the unwind-protect cleanups the type wasm insists they be called with.
    for p in ../patches/wk-*.patch; do
        ( cd "$SRC" && patch -p1 --forward < "$p" )
    done
fi

# The shared termios shim + its wk:tty/control bindings (same as vim's build),
# and the wk:exec bindings + shim that let bash run commands.
mkdir -p "$TTYGEN" "$EXECGEN"
wit-bindgen c --world terminal "$TTY/wit/tty.wit" --out-dir "$TTYGEN"
wit-bindgen c --world exec-host "$EXEC/wit" --out-dir "$EXECGEN"

CFLAGS="--target=wasm32-wasip2 -O2 -DWK_EXEC=1 -I$COMPAT -I$EXEC -I$EXECGEN -I$PIPE -I$TTY -I$TTYGEN \
    -DHAVE_TERMIOS_H=1 -DHAVE_TCGETATTR=1 \
    -Wno-implicit-function-declaration -Wno-deprecated-non-prototype \
    -mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false \
    -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_PROCESS_CLOCKS -D_WASI_EMULATED_GETPID"

# ../pipe-compat gives bash a real pipe(): wasi-libc's is ENOSYS on wasip2, and
# without it `a | b` fails before it can even reach a stage.
for src in "$COMPAT/compat.c" "$TTY/termios.c" "$TTYGEN/terminal.c" \
           "$COMPAT/wkbash.c" "$COMPAT/exit_shim.c" "$EXEC/wkexec.c" "$EXECGEN/exec_host.c" \
           "$PIPE/pipe.c"; do
    obj="$COMPAT/$(basename "${src%.c}").o"
    "$WASI_SDK/bin/clang" --target=wasm32-wasip2 -O2 \
        -I"$COMPAT" -I"$EXEC" -I"$EXECGEN" -I"$PIPE" -I"$TTY" -I"$TTYGEN" \
        -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_PROCESS_CLOCKS -D_WASI_EMULATED_GETPID \
        -c "$src" -o "$obj"
done

# --target here as well: see the note above about the link step.
LDFLAGS="--target=wasm32-wasip2 $COMPAT/compat.o $COMPAT/termios.o $COMPAT/terminal.o \
    $COMPAT/wkbash.o $COMPAT/exit_shim.o $COMPAT/wkexec.o $COMPAT/exec_host.o $COMPAT/pipe.o \
    $TTYGEN/terminal_component_type.o $EXECGEN/exec_host_component_type.o -lsetjmp \
    -lwasi-emulated-signal -lwasi-emulated-process-clocks -lwasi-emulated-getpid"

cd "$SRC"
# configure answers questions *about the toolchain*, so its cached answers are
# only good for the toolchain that produced them. The wasi-sdk 33 -> 34 bump
# added dup2, and a config.h left over from before it said `#undef HAVE_DUP2`
# — bash quietly built its own dup2 replacement and every redirection failed
# with "cannot duplicate fd". Stamp what we configured against and start over
# when that changes.
TOOLCHAIN="$("$WASI_SDK/bin/clang" --version | head -1)"
if [ -f Makefile ] && [ "$(cat .wk-toolchain 2>/dev/null)" != "$TOOLCHAIN" ]; then
    echo "toolchain changed since configure; reconfiguring bash"
    env PATH="$BUILD_PATH" make distclean >/dev/null 2>&1 || true
    rm -f .wk-toolchain
fi
if [ ! -f Makefile ]; then
    CC="$WASI_SDK/bin/clang" \
    AR="$WASI_SDK/bin/llvm-ar" \
    RANLIB="$WASI_SDK/bin/llvm-ranlib" \
    CC_FOR_BUILD=/usr/bin/cc CFLAGS_FOR_BUILD="-O1" \
    CFLAGS="$CFLAGS" LDFLAGS="$LDFLAGS" \
    ./configure --host=wasm32-wasi \
        --disable-job-control --disable-readline --disable-history \
        --disable-nls --without-bash-malloc \
        ac_cv_have_sig_atomic_t=yes bash_cv_signal_vintage=posix \
        ac_cv_func_getcwd=yes bash_cv_getcwd_malloc=yes
    printf '%s' "$TOOLCHAIN" > .wk-toolchain
fi

# The shim objects reach the link through LDFLAGS, so make has no idea they are
# inputs and will happily leave `bash` alone after one of them changes. Drop it
# and let the link run every time; it is a second.
rm -f bash

# SIGNAMES_O= : trap.o already carries signal_names (see the header).
# LIBS without -ldl : no dynamic linker on WASI.
env PATH="$BUILD_PATH" make \
    CFLAGS="$CFLAGS" LDFLAGS="$LDFLAGS" \
    LIBS='$(BUILTINS_LIB) $(LIBRARIES)' SIGNAMES_O= \
    CC_FOR_BUILD=/usr/bin/cc CFLAGS_FOR_BUILD="-O1" \
    -j"$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)"

# bash's own getcwd replacement lands in libsh even when unused; drop it so it
# can't collide with wasi-libc's at link time.
"$WASI_SDK/bin/llvm-ar" d lib/sh/libsh.a getcwd.o 2>/dev/null || true

cd ..
# wasip2 links a component directly — no wasm-tools adapter step.
cp "$SRC/bash" bash.wasm

# The coreutils binary plus its command names as real symlinks, staged for the
# wk-shell image — the same one-binary-many-links layout coreutils installs.
cp ../coreutils/coreutils.wasm coreutils.wasm 2>/dev/null || \
    echo "note: build plugins/coreutils first for a shell that can run commands"
rm -rf bin && mkdir -p bin
for a in "[" b2sum base32 base64 basename basenc cat chcon chgrp chmod chown \
         cksum comm cp csplit cut date dir dircolors dirname du echo expand \
         expr factor false fmt fold groups head hostid id join link ln logname \
         ls md5sum mkdir mkfifo mknod mktemp mv nl nproc numfmt od paste \
         pathchk pr printenv printf ptx pwd readlink realpath rm rmdir seq \
         sha1sum sha224sum sha256sum sha384sum sha512sum shred shuf sleep sort \
         split stat sum sync tac tail tee test touch tr true truncate tsort \
         tty uname unexpand uniq unlink vdir wc whoami yes; do
    ln -sf coreutils.wasm "bin/$a"
done
# bash under its own name, plus `sh` (POSIX mode via argv[0]) — so anything that
# execs `/bin/sh -c ...` (a shell script, a `system()` call, node's
# child_process) finds a real shell on PATH, the same layout a Unix base image
# ships.
ln -sf bash.wasm bin/bash
ln -sf bash.wasm bin/sh
echo "built plugins/bash/bash.wasm (GNU bash $BASH_VER, wasm32-wasip2 component)"
echo "package it with: wk images build plugins/bash/Dockerfile --tag wk-shell"
