#!/usr/bin/env bash
# Build the UNMODIFIED upstream PHP 8.2.6 interpreter from source, targeting
# wasm32-wasip2 so its built-in webserver (`php -S`) binds real sockets over wk's
# network fabric — a real PHP app server, not just a script runner.
#
# PHP has no wasi.py-style helper, so this leans on VMware Wasm Labs' proven
# wasip1 port patches (fibers, syslog, fork, gd/setjmp — the "make PHP compile on
# WASI" set) fetched from their archived repo, then does the wk-specific work:
#   * target wasm32-wasip2 (their build was wasip1, no sockets);
#   * define WASM_WASI so those patches stub the POSIX bits WASI omits, but leave
#     the CLI server's own socket()/bind()/listen()/accept() calls untouched —
#     they resolve to wasi-libc's wasip2 sockets → the fabric;
#   * un-stub php_network_getaddresses so the server can resolve its bind host
#     (patches/wk-0001-wasip2-real-sockets.patch);
#   * compile setjmp/longjmp (Zend's zend_bailout) via the WebAssembly exception
#     proposal, like the lua/sqlite plugins (host enables Config::wasm_exceptions).
# The wasip2 link step emits a component directly — no wasm-tools adapter.
#
# Requires wasi-sdk (WASI_SDK, default ~/wasi-sdk) with a wasm32-wasip2 sysroot,
# plus autoconf, bison, re2c (for buildconf). The source + upstream patches are
# fetched (and cached) under php-8.2.6/ on first run.
set -euo pipefail
cd "$(dirname "$0")"

PHP_VER=8.2.6
SRC="php-$PHP_VER"
WASI_SDK="${WASI_SDK:-$HOME/wasi-sdk}"
WASI_SYSROOT="$WASI_SDK/share/wasi-sysroot"
# VMware Wasm Labs' base WASI port patches (the compile-on-WASI set; we skip the
# WasmEdge-socket ones — wk uses wasi-libc's own wasip2 sockets instead).
WLR=https://raw.githubusercontent.com/vmware-labs/webassembly-language-runtimes/main/php/v8.2.6/patches
WLR_PATCHES=(
    0001-Initial-port-of-the-php-7.4.32-patch-for-php-8.2.6.patch
    0002-feat-Incapacitate-fibers-when-compiling-for-WASI.patch
    0003-fix-Add-more-ifdefs-for-php-8.2.0.patch
    0006-fix-Avoid-include-syslog.h-since-it-is-not-part-of-w.patch
    0014-fix-implicit-function-declaration-in-syslog.patch
    0017-fix-Patch-for-is_readable-and-is_writable-to-bypass-.patch
    0018-fix-random_bytes-failing-on-Windows.patch
)

# wasi-sdk's clang runs an optional wasm-opt post-link step; the wasm-opt on PATH
# can't parse the new exnref EH we emit, so run the build with a PATH that omits
# it (kept consistent with the lua/sqlite plugins).
BUILD_PATH="$WASI_SDK/bin:/usr/bin:/bin"

if [ ! -d "$SRC" ]; then
    echo "fetching PHP $PHP_VER..."
    curl -fsSL "https://www.php.net/distributions/$SRC.tar.gz" -o "$SRC.tar.gz"
    tar xzf "$SRC.tar.gz"
    rm -f "$SRC.tar.gz"

    echo "fetching + applying upstream WASI port patches..."
    mkdir -p .patches
    ( cd "$SRC"
      for p in "${WLR_PATCHES[@]}"; do
          curl -fsSL "$WLR/$p" -o "../.patches/$p"
          patch -p1 --forward --silent < "../.patches/$p"
      done
      # wk: keep real sockets under WASM_WASI (see header).
      patch -p1 --forward < ../patches/wk-0001-wasip2-real-sockets.patch
    )
fi

# --- libsqlite3.a for wasm32-wasip2: pdo_sqlite/sqlite3 link against it, so
# PHP (and WordPress via its SQLite Database Integration plugin) has a database.
SQLITE_VER=3530300
SQLITE_YEAR=2026
DEPS="$PWD/deps"
if [ ! -f "$DEPS/lib/libsqlite3.a" ]; then
    if [ ! -d "sqlite-amalgamation-$SQLITE_VER" ]; then
        curl -fsSL "https://www.sqlite.org/$SQLITE_YEAR/sqlite-amalgamation-$SQLITE_VER.zip" -o sqlite-amalg.zip
        unzip -oq sqlite-amalg.zip && rm -f sqlite-amalg.zip
    fi
    mkdir -p "$DEPS/lib/pkgconfig" "$DEPS/include"
    PATH="$BUILD_PATH" clang --target=wasm32-wasip2 -O2 \
        -DSQLITE_THREADSAFE=0 -DSQLITE_OMIT_LOAD_EXTENSION -DSQLITE_OMIT_WAL -DSQLITE_DISABLE_LFS \
        -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_PROCESS_CLOCKS -D_WASI_EMULATED_MMAN -D_WASI_EMULATED_GETPID \
        -c "sqlite-amalgamation-$SQLITE_VER/sqlite3.c" -o "$DEPS/sqlite3.o"
    "$WASI_SDK/bin/llvm-ar" rcs "$DEPS/lib/libsqlite3.a" "$DEPS/sqlite3.o"
    cp "sqlite-amalgamation-$SQLITE_VER/sqlite3.h" "$DEPS/include/"
    cat > "$DEPS/lib/pkgconfig/sqlite3.pc" <<PC
prefix=$DEPS
libdir=\${prefix}/lib
includedir=\${prefix}/include
Name: SQLite
Description: SQL database engine
Version: 3.53.3
Libs: -L\${libdir} -lsqlite3
Cflags: -I\${includedir}
PC
fi
# pkg-config (from the host) locates our sqlite3.pc; keep its dir on the configure
# PATH and point PKG_CONFIG_PATH at deps.
PKG_CONFIG_BIN="$(command -v pkg-config || true)"
[ -n "$PKG_CONFIG_BIN" ] || { echo "pkg-config not found (needed for --with-sqlite3)" >&2; exit 1; }
export PKG_CONFIG_PATH="$DEPS/lib/pkgconfig"
CONFIGURE_PATH="$BUILD_PATH:$(dirname "$PKG_CONFIG_BIN")"

cd "$SRC"

# The patches touch configure.ac, so regenerate configure (release tarballs ship
# a pre-generated one).
if [ ! -f .buildconf-done ]; then
    PATH="$BUILD_PATH:/opt/homebrew/bin" ./buildconf --force
    touch .buildconf-done
fi

EH="-mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false"
export CC="clang --target=wasm32-wasip2"
export CXX="clang++ --target=wasm32-wasip2"
export CFLAGS="-O2 $EH -DWASM_WASI -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_GETPID -D_WASI_EMULATED_PROCESS_CLOCKS -D_GNU_SOURCE=1 -Wno-unused-command-line-argument -Wno-implicit-function-declaration -Wno-incompatible-function-pointer-types -lwasi-emulated-signal -lwasi-emulated-getpid -lwasi-emulated-process-clocks"
export CXXFLAGS="$CFLAGS"
export LDFLAGS="$EH -lwasi-emulated-signal -lwasi-emulated-getpid -lwasi-emulated-process-clocks"

if [ ! -f Makefile ]; then
    # Extensions: pdo_sqlite/sqlite3 for the database; the rest is the set a
    # stock WordPress needs that has no external-library dependency.
    PATH="$CONFIGURE_PATH" ./configure \
        --host=wasm32-wasip2 host_alias=wasm32-wasi \
        --target=wasm32-wasip2 target_alias=wasm32-wasi \
        --disable-all --enable-cli \
        --enable-pdo --with-pdo-sqlite --with-sqlite3 \
        --enable-filter --enable-ctype --enable-tokenizer --enable-session \
        --enable-fileinfo --enable-exif --enable-calendar \
        --without-pcre-jit --disable-fiber-asm --disable-zend-signals \
        --without-pear --disable-phar --without-iconv --without-openssl --disable-opcache \
        --config-cache
fi

PATH="$BUILD_PATH" make -j"$(sysctl -n hw.ncpu 2>/dev/null || nproc)" cli

cp sapi/cli/php ../php.wasm
echo "built plugins/php/php.wasm ($(du -h ../php.wasm | cut -f1))"
