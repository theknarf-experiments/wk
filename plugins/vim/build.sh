#!/usr/bin/env bash
# Build UNMODIFIED upstream Vim into a wasi:cli command component that wk runs in
# a terminal node. The recipe follows kilo (real C, no source edits) + lua (the
# setjmp/exnref exception-handling flags), with a hand-generated config for
# cross-compiling (Vim's autoconf can't run wasm test programs).
#
# Non-stock pieces are all in compat/ (kilo principle — supply via the runtime,
# don't patch the app): a <termios.h>/<sys/ioctl.h> that bridge raw mode to wk's
# terminal, a no-op termcap library (headless terminal → Vim's builtin termcaps),
# and stubs for the process/tty syscalls WASI lacks (fork/exec/select/...).
#
# Requires wasi-sdk (WASI_SDK, default ~/wasi-sdk) and wasm-tools. Vim source is
# fetched (and cached) under vim-src/ on first run; auto/config.h etc. are
# generated once by configure (also cached).
set -euo pipefail
cd "$(dirname "$0")"

WASI_SDK="${WASI_SDK:-$HOME/wasi-sdk}"
VIM_VER=9.1.0000
SRC=vim-src/src
COMPAT="$(pwd)/compat"

# WASIp1→component adapter, pinned to our wasmtime (46). Prefer an explicit
# override or a copy already in the cargo registry; otherwise fetch the release
# artifact once and cache it. Applied with the `wasi_snapshot_preview1=` name so
# wasm-tools binds it to Vim's preview1 imports regardless of the file's stem.
WASMTIME_VER=46.0.1
ADAPTER="${WASI_ADAPTER:-$(find "$HOME/.cargo/registry/src" -name 'wasi_snapshot_preview1.command.wasm' 2>/dev/null | head -1)}"
if [ -z "$ADAPTER" ] || [ ! -f "$ADAPTER" ]; then
    ADAPTER="$COMPAT/wasi_snapshot_preview1.command.wasm"
    if [ ! -f "$ADAPTER" ]; then
        echo "fetching WASI command adapter $WASMTIME_VER..."
        curl -fsSL "https://github.com/bytecodealliance/wasmtime/releases/download/v$WASMTIME_VER/wasi_snapshot_preview1.command.wasm" -o "$ADAPTER"
    fi
fi

# wasm-opt (wasi-sdk's optional post-link step) can't parse the new exnref EH we
# emit; run clang with a PATH that omits it. wasm-tools runs under normal PATH.
CLANG_PATH="$WASI_SDK/bin:/usr/bin:/bin"
CLANG="$WASI_SDK/bin/clang"

if [ ! -d vim-src ]; then
    echo "fetching Vim $VIM_VER..."
    curl -fsSL "https://github.com/vim/vim/archive/refs/tags/v$VIM_VER.tar.gz" -o vim.tar.gz
    tar xzf vim.tar.gz
    mv "vim-$VIM_VER" vim-src
    rm -f vim.tar.gz
fi

# Exception-handling flags (setjmp → exnref) + the WASI emulated features Vim's
# libc calls need. -Icompat first so our <termios.h>/<sys/ioctl.h> win.
EH="-mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false"
DEFS="-DHAVE_CONFIG_H -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_PROCESS_CLOCKS -D_WASI_EMULATED_MMAN -D_WASI_EMULATED_GETPID"
CFLAGS="--target=wasm32-wasip1 -O2 $EH $DEFS -Wno-deprecated-declarations -Wno-implicit-function-declaration -I$COMPAT -I$SRC -I$SRC/proto"

# --- one-time config generation (cross-compile: preset the AC_TRY_RUN caches) ---
if [ ! -f "$SRC/auto/config.h" ]; then
    echo "configuring Vim for wasm32-wasi..."
    # A no-op termcap lib so configure's tgetent check passes; Vim then uses its
    # builtin termcaps at runtime.
    "$CLANG" --target=wasm32-wasip1 -O2 -c "$COMPAT/tcap_stub.c" -o "$COMPAT/tcap_stub.o"
    "$WASI_SDK/bin/llvm-ar" rcs "$COMPAT/libwktcap.a" "$COMPAT/tcap_stub.o"
    (
        cd "$SRC"
        export CC="$CLANG --target=wasm32-wasip1"
        export CFLAGS="-O2 $DEFS -I$COMPAT"
        export LDFLAGS="-L$COMPAT"
        export vim_cv_toupper_broken=no vim_cv_terminfo=no vim_cv_tgetent=zero \
            vim_cv_getcwd_broken=no vim_cv_stat_ignores_slash=no \
            vim_cv_memmove_handles_overlap=yes vim_cv_bcopy_handles_overlap=yes \
            vim_cv_memcpy_handles_overlap=yes vim_cv_timer_create=no \
            vim_cv_uname_output=Linux vim_cv_uname_r_output= vim_cv_uname_m_output=wasm32 \
            ac_cv_sizeof_int=4 ac_cv_sizeof_long=4 ac_cv_sizeof_time_t=8 ac_cv_sizeof_off_t=8
        ./configure --host=wasm32-wasi --build=x86_64-apple-darwin \
            --with-features=tiny --enable-gui=no --without-x --with-tlib=wktcap \
            --disable-netbeans --disable-channel --disable-terminal \
            --disable-nls --disable-selinux --disable-smack --disable-acl \
            --disable-canberra --disable-libsodium >/dev/null
        # Fix wasi-wrong config the AC_TRY_RUN/link probes guessed for the host.
        sed -i.bak \
            -e 's|#define HAVE_DLOPEN 1|/* #undef HAVE_DLOPEN */|' \
            -e 's|/\* #undef HAVE_SETJMP_H \*/|#define HAVE_SETJMP_H 1|' \
            -e 's|/\* #undef HAVE_TERMIOS_H \*/|#define HAVE_TERMIOS_H 1|' \
            auto/config.h
        # osdef.h (prototype extraction) needs the EH flag so <setjmp.h> parses.
        CC="$CLANG --target=wasm32-wasip1 $EH $DEFS -I$COMPAT" srcdir=. sh osdef.sh
    )
    # pathdef.c: compiled-in paths (normally Makefile-generated).
    cat > "$SRC/auto/pathdef.c" <<'PATHDEF'
#include "vim.h"
char_u *default_vim_dir = (char_u *)"/usr/share/vim";
char_u *default_vimruntime_dir = (char_u *)"/usr/share/vim/runtime";
char_u *all_cflags = (char_u *)"wasi-sdk clang -O2";
char_u *all_lflags = (char_u *)"wasi-sdk clang";
char_u *compiled_user = (char_u *)"wk";
char_u *compiled_sys = (char_u *)"wk";
PATHDEF
fi

# --- compile every core source ---
FILES=$(awk '/^BASIC_SRC = /{f=1} f{print} /^$/{if(f)exit}' "$SRC/Makefile" | grep -oE "[a-z_0-9]+\.c")
mkdir -p wkobj
OBJS=""
for f in $FILES; do
    src="$SRC/$f"; [ -f "$src" ] || src="$SRC/auto/$f"
    obj="wkobj/${f%.c}.o"
    env PATH="$CLANG_PATH" "$CLANG" $CFLAGS -c "$src" -o "$obj"
    OBJS="$OBJS $obj"
done

# --- link + componentize ---
env PATH="$CLANG_PATH" "$CLANG" --target=wasm32-wasip1 $EH -I"$COMPAT" \
    $OBJS "$COMPAT/wkos.c" "$COMPAT/libwktcap.a" \
    -lsetjmp -lwasi-emulated-signal -lwasi-emulated-process-clocks \
    -lwasi-emulated-mman -lwasi-emulated-getpid \
    -o vim.core.wasm

wasm-tools component new vim.core.wasm --adapt "wasi_snapshot_preview1=$ADAPTER" -o vim.wasm
rm -f vim.core.wasm
echo "built plugins/vim/vim.wasm"
