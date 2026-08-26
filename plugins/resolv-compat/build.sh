#!/usr/bin/env bash
# Build libresolv.a for wasm32-wasip2 into ./sysroot, for anything that needs
# to speak DNS *records* rather than just resolve names. See resolv.h for what
# this shim is and why wasi-libc's lack of a resolver makes it necessary.
#
# Consumers point CMake at ./sysroot: `find_library(resolv)` finds
# sysroot/lib/libresolv.a and `#include <resolv.h>` finds sysroot/include.
# That is exactly what Qt's cmake/FindWrapResolv.cmake probes for, so
# plugins/qt/build-qtbase.sh adds this directory to CMAKE_FIND_ROOT_PATH and
# QT_FEATURE_libresolv then autodetects ON — which is what pulls Qt's own
# qdnslookup_unix.cpp into libQt6Network.a instead of the do-nothing
# qdnslookup_dummy.cpp.
#
# There is deliberately NO componentization step here: this is a static
# library that other plugins link, not a node.
#
# Portability is a feature, not an accident. Nothing in resolv.c is
# wasm-specific — it is socket/sendto/poll/recvfrom — so it builds natively
# too, which is how the DNS logic was tested against a real resolver before
# ever being pointed at the fabric. `./build.sh --native` rebuilds that
# self-test; keep it working.
#
# Requires wasi-sdk (set WASI_SDK; defaults to the mise-pinned install).
set -euo pipefail
cd "$(dirname "$0")"

# Default to the mise-pinned toolchain when present; ~/wasi-sdk may be stale.
MISE_SDK="$HOME/.local/share/mise/installs/github-web-assembly-wasi-sdk/wasi-sdk-34-rc.2"
WASI_SDK="${WASI_SDK:-$([ -d "$MISE_SDK" ] && echo "$MISE_SDK" || echo "$HOME/wasi-sdk")}"
# The same guard every other C plugin here carries: these are built and tested
# against exactly this SDK, and a silent mismatch wastes an afternoon.
EXPECT="wasi-sdk-34-rc.2"
case "$WASI_SDK" in
    *"$EXPECT"*) ;;
    *)
        echo "resolv-compat: expected $EXPECT (set WASI_SDK), got: $WASI_SDK" >&2
        exit 1
        ;;
esac

if [ "${1:-}" = "--native" ]; then
    # The host build exists so the DNS logic can be exercised against a real
    # resolver; it is not installed anywhere.
    mkdir -p obj
    cc -I. -O2 -Wall -Wextra -c resolv.c -o obj/resolv-native.o
    echo "built obj/resolv-native.o (host)"
    exit 0
fi

CLANG="$WASI_SDK/bin/clang"
SYSROOT="$PWD/sysroot"
mkdir -p obj "$SYSROOT/lib" "$SYSROOT/include"

"$CLANG" --target=wasm32-wasip2 -O2 -Wall -Wextra \
    -c resolv.c -o obj/resolv.o

"$WASI_SDK/bin/llvm-ar" rcs "$SYSROOT/lib/libresolv.a" obj/resolv.o
cp -f resolv.h "$SYSROOT/include/resolv.h"

echo "built plugins/resolv-compat/sysroot/lib/libresolv.a"
