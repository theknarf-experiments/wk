#!/usr/bin/env bash
# Build UNMODIFIED upstream GNU bash into a wasm component that wk runs in a
# terminal node — the real bash(1), not a shell-alike.
#
# What works: the shell *language*. Variables and expansions, arithmetic,
# conditionals ([[ ]] and test), for/while/case, functions, aliases, arrays,
# and every builtin — verified in a node with
# `echo "hello from real GNU bash $BASH_VERSION"` reporting
# 5.2.37(1)-release (wasm32-unknown-wasi).
#
# What cannot work, and why: WASI has no process creation and no file
# descriptor duplication.
#   * pipelines and external commands need fork/exec — there is no such call;
#     the build stubs them to ENOSYS (`--disable-job-control`, nojobs.c).
#   * redirection (`> file`) needs dup()/dup2() to save and restore stdout.
#     WASI preview1 offers `fd_renumber`, which *moves* a descriptor rather
#     than copying it, so the save/restore dance bash performs is impossible;
#     redirections fail with "cannot duplicate fd". This is a WASI gap, not a
#     port bug — it disappears if WASI ever grows a real dup.
# So this is a script-language engine, not (yet) a command driver. For running
# actual commands in wk, give a node the coreutils multicall binary
# (plugins/coreutils) — one node, one program, which is the shape WASI allows.
#
# Targets wasm32-wasip1 + the component adapter (bash needs no sockets), which
# lets it reuse ../tty-compat — the same real termios shim vim uses, mapping
# terminal control onto wk's `wk:tty/control` capability.
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
#   * sockets — configure mis-detects netdb.h/sys/socket.h, so bash builds
#     /dev/tcp support against headers wasip1 lacks. config.h is corrected
#     after configure (a generated file, not source).
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

BUILD_PATH="$WASI_SDK/bin:/usr/bin:/bin"
WASMTIME_VER=46.0.1
ADAPTER="${WASI_ADAPTER:-$(find "$HOME/.cargo/registry/src" -name 'wasi_snapshot_preview1.command.wasm' 2>/dev/null | head -1)}"
if [ -z "$ADAPTER" ] || [ ! -f "$ADAPTER" ]; then
    ADAPTER="$COMPAT/wasi_snapshot_preview1.command.wasm"
    [ -f "$ADAPTER" ] || curl -fsSL \
        "https://github.com/bytecodealliance/wasmtime/releases/download/v$WASMTIME_VER/wasi_snapshot_preview1.command.wasm" \
        -o "$ADAPTER"
fi

if [ ! -d "$SRC" ]; then
    echo "fetching GNU bash $BASH_VER..."
    curl -fsSL "https://ftp.gnu.org/gnu/bash/$SRC.tar.gz" -o "$SRC.tar.gz"
    tar xzf "$SRC.tar.gz"
    rm -f "$SRC.tar.gz"
fi

# The shared termios shim + its wk:tty/control bindings (same as vim's build).
mkdir -p "$TTYGEN"
wit-bindgen c --world terminal "$TTY/wit/tty.wit" --out-dir "$TTYGEN"

CFLAGS="--target=wasm32-wasip1 -O2 -I$COMPAT -I$TTY -I$TTYGEN \
    -DF_DUPFD=0 -DHAVE_TERMIOS_H=1 -DHAVE_TCGETATTR=1 \
    -Wno-implicit-function-declaration -Wno-deprecated-non-prototype \
    -mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false \
    -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_PROCESS_CLOCKS -D_WASI_EMULATED_GETPID"

for src in "$COMPAT/compat.c" "$TTY/termios.c" "$TTYGEN/terminal.c"; do
    obj="$COMPAT/$(basename "${src%.c}").o"
    "$WASI_SDK/bin/clang" --target=wasm32-wasip1 -O2 \
        -I"$COMPAT" -I"$TTY" -I"$TTYGEN" \
        -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_PROCESS_CLOCKS -D_WASI_EMULATED_GETPID \
        -c "$src" -o "$obj"
done

LDFLAGS="$COMPAT/compat.o $COMPAT/termios.o $COMPAT/terminal.o \
    $TTYGEN/terminal_component_type.o -lsetjmp \
    -lwasi-emulated-signal -lwasi-emulated-process-clocks -lwasi-emulated-getpid"

cd "$SRC"
if [ ! -f Makefile ]; then
    CC="$WASI_SDK/bin/clang" \
    AR="$WASI_SDK/bin/llvm-ar" \
    RANLIB="$WASI_SDK/bin/llvm-ranlib" \
    CC_FOR_BUILD=/usr/bin/cc CFLAGS_FOR_BUILD="-O1" \
    CFLAGS="$CFLAGS" LDFLAGS="$LDFLAGS" \
    ./configure --host=wasm32-wasi \
        --disable-job-control --disable-readline --disable-history \
        --disable-nls --disable-net-redirections --without-bash-malloc \
        ac_cv_have_sig_atomic_t=yes bash_cv_signal_vintage=posix \
        ac_cv_func_getcwd=yes bash_cv_getcwd_malloc=yes

    # configure mis-detects sockets; wasip1 has none (see the header).
    sed -i '' \
        -e 's|^#define HAVE_NETDB_H 1|/* wasip1 has no sockets */|' \
        -e 's|^#define HAVE_SYS_SOCKET_H 1|/* wasip1 has no sockets */|' \
        -e 's|^#define HAVE_NETINET_IN_H 1|/* wasip1 has no sockets */|' \
        -e 's|^#define HAVE_GETPEERNAME 1|/* wasip1 has no sockets */|' \
        config.h
fi

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
wasm-tools component new "$SRC/bash" --adapt "wasi_snapshot_preview1=$ADAPTER" -o bash.wasm
echo "built plugins/bash/bash.wasm (GNU bash $BASH_VER, wasm32-wasip1 component)"
