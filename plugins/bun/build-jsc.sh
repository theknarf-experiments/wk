#!/usr/bin/env bash
# Build JavaScriptCore — bun's WebKit fork, cloop LLInt interpreter, no JIT —
# for wasm32-wasip2. Produces native/jsc-build/bin/jsc (a 57 MB wasm shell
# that runs real JavaScript: Intl/ICU, regex, BigInt, async all work under
# `wasmtime run -W exceptions`) plus the static libs bun's *_jsc bridge will
# link next: libJavaScriptCore.a, libWTF.a, libbmalloc.a.
#
# The port knowledge, each item a failure first:
#   * ICU cross-compiles in two stages (host build supplies the data tools).
#     Configure can't detect the platform → alias config/mh-linux to
#     mh-unknown; wasi has no tz API → -DU_HAVE_TZSET=0/TIMEZONE=0/TZNAME=0.
#   * WebKit patches live in patches/webkit-wasi-0001.patch: WASI as a
#     cmake/WTF OS (Unix-shaped), signal machinery compiled out (wasi's
#     emulated <signal.h> has no sigaction/siginfo_t), thread suspend/resume
#     stubbed (single thread — the GC never stops any other world),
#     getentropy as the entropy source (no /dev/urandom), stack bounds from
#     wasm-ld's __stack_low/__stack_high, wasm32 fixes where LP64 was
#     assumed (RawHex, Options parse<size_t>, double-conversion arch list),
#     and bit-rot repairs to bun-fork paths cloop never built (DOMJIT's
#     AbstractHeapKind hoist, USE_BUN_JSC_ADDITIONS=ON is the maintained
#     path).
#   * setjmp/longjmp (the cloop's VM-entry unwind) lowers to wasm exceptions:
#     -mllvm -wasm-enable-sjlj + -lsetjmp, and the RUNTIME needs the
#     exceptions proposal (wasmtime -W exceptions; wk's host already enables
#     Config::wasm_exceptions).
#   * wasm-ld's default shadow stack is 64 KB; JSC's reserved zone alone is
#     bigger, so every VM entry "overflowed" with an unprintable RangeError.
#     -Wl,-z,stack-size=8388608. LLIntOffsetsExtractor works unmodified —
#     offlineasm parses the wasm object file fine.
#
# Requires: cmake, ninja, ruby (offlineasm), wasi-sdk, wasmtime for testing.
set -euo pipefail
cd "$(dirname "$0")"

WASI_SDK="${WASI_SDK:-$HOME/wasi-sdk}"
WEBKIT_REV=caad865eb1a6e5ca4427f5ea1f066140b11953e7   # bun/scripts/build/deps/webkit.ts
ICU_VER=76-1
mkdir -p native

# ── ICU (host tools, then wasi cross) ─────────────────────────────────────
if [ ! -d native/icu ]; then
    curl -fsSL "https://github.com/unicode-org/icu/releases/download/release-$ICU_VER/icu4c-${ICU_VER/-/_}-src.tgz" | tar xz -C native
    cp native/icu/source/config/mh-linux native/icu/source/config/mh-unknown
fi
if [ ! -f native/icu-host/lib/libicuuc.dylib ] && [ ! -f native/icu-host/lib/libicuuc.so ]; then
    mkdir -p native/icu-host
    ( cd native/icu-host && ../icu/source/runConfigureICU "$(uname | sed 's/Darwin/MacOSX/')" \
        --disable-tests --disable-samples --disable-extras > configure.log 2>&1 && make -j8 > build.log 2>&1 )
fi
if [ ! -f native/icu-wasi/install/lib/libicuuc.a ]; then
    mkdir -p native/icu-wasi
    ( cd native/icu-wasi && \
      DEFS="-DU_HAVE_TZSET=0 -DU_HAVE_TIMEZONE=0 -DU_HAVE_TZNAME=0 -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_GETPID -D_WASI_EMULATED_MMAN" && \
      CC="$WASI_SDK/bin/clang" CXX="$WASI_SDK/bin/clang++" AR="$WASI_SDK/bin/llvm-ar" RANLIB="$WASI_SDK/bin/llvm-ranlib" \
      CFLAGS="--target=wasm32-wasip2 -O2 $DEFS" CXXFLAGS="--target=wasm32-wasip2 -O2 -fno-exceptions $DEFS" \
      LDFLAGS="--target=wasm32-wasip2 -lwasi-emulated-signal -lwasi-emulated-getpid -lwasi-emulated-mman" \
      ../icu/source/configure --host=wasm32-wasi --with-cross-build="$PWD/../icu-host" \
        --enable-static --disable-shared --disable-dyload --disable-tools --disable-tests \
        --disable-samples --disable-extras --with-data-packaging=static --prefix="$PWD/install" \
        > configure.log 2>&1 && make -j8 > build.log 2>&1 && make install > install.log 2>&1 )
fi

# ── WebKit (bun's fork at the repo pin) + the wasi patch ──────────────────
if [ ! -d native/webkit ]; then
    git init -q native/webkit
    ( cd native/webkit && git remote add origin https://github.com/oven-sh/WebKit.git && \
      git fetch --depth 1 origin "$WEBKIT_REV" && git checkout -q FETCH_HEAD && \
      git apply ../../patches/webkit-wasi-0001.patch )
fi

# ── JSC (cloop, static, system malloc, bun additions) ─────────────────────
FLAGS="-D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_GETPID -D_WASI_EMULATED_MMAN -D_WASI_EMULATED_PROCESS_CLOCKS -DU_STATIC_IMPLEMENTATION -DSIMDE_FLOAT16_API=1 -mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false"
cmake -S native/webkit -B native/jsc-build -G Ninja \
  -DCMAKE_TOOLCHAIN_FILE="$WASI_SDK/share/cmake/wasi-sdk-p2.cmake" -DWASI_SDK_PREFIX="$WASI_SDK" \
  -DPORT=JSCOnly -DCMAKE_BUILD_TYPE=Release \
  -DENABLE_JIT=OFF -DENABLE_C_LOOP=ON -DENABLE_DFG_JIT=OFF -DENABLE_FTL_JIT=OFF \
  -DENABLE_WEBASSEMBLY=OFF -DENABLE_SAMPLING_PROFILER=OFF -DENABLE_REMOTE_INSPECTOR=OFF \
  -DUSE_SYSTEM_MALLOC=ON -DENABLE_STATIC_JSC=ON -DCMAKE_POSITION_INDEPENDENT_CODE=OFF \
  -DUSE_BUN_JSC_ADDITIONS=ON \
  -DICU_ROOT="$PWD/native/icu-wasi/install" -DICU_INCLUDE_DIR="$PWD/native/icu-wasi/install/include" \
  -DCMAKE_C_FLAGS="$FLAGS" -DCMAKE_CXX_FLAGS="$FLAGS" \
  -DCMAKE_EXE_LINKER_FLAGS="-lsetjmp -lwasi-emulated-getpid -lwasi-emulated-signal -lwasi-emulated-mman -lwasi-emulated-process-clocks -Wl,-z,stack-size=8388608"
ninja -C native/jsc-build jsc
echo "built native/jsc-build/bin/jsc — try:"
echo "  wasmtime run -W exceptions native/jsc-build/bin/jsc -e 'print(6*7)'"
