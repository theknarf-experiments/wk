#!/bin/bash
# uSockets (C core + the C++ sni_tree TU) for wasm32-wasip2 → $VLIB/libusockets.a.
# Recompile after touching packages/bun-usockets/** headers (e.g. the
# Bun__addrinfo_set / zig_mutex_t wasi arms).
#
# PATHS RE-ROOTED (2026-08): objects went to /tmp/vlib/us; now $VLIB/us.
# Flags byte-identical.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
P="${BUN_PLUGIN:-$(cd "$HERE/.." && pwd)}"
N="${BUN_NATIVE:-$P/native}"
B="${BUN:-$P/bun}"
WORK="${WORK:-$N/runtime-build}"
VLIB="${VLIB:-$WORK/vlib}"
WASI_SDK="${WASI_SDK:?set WASI_SDK (wasi-sdk-34-rc.2)}"
CC="$WASI_SDK/bin/clang"; CXX="$WASI_SDK/bin/clang++"; AR="$WASI_SDK/bin/llvm-ar"
cd "$B"
FLAGS="-O2 -DLIBUS_USE_EPOLL -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_MMAN -D_WASI_EMULATED_GETPID -DLIBUS_USE_OPENSSL -I$N/boringssl/include -include $P/wasi-compat/sys/socket_compat.h -include $P/wasi-compat/wasi_signal_compat.h -I packages/bun-usockets/src -I $N/mimalloc/include -idirafter $P/wasi-compat"
mkdir -p "$VLIB/us"; objs=(); i=0; fails=0
for c in packages/bun-usockets/src/loop.c packages/bun-usockets/src/socket.c packages/bun-usockets/src/context.c packages/bun-usockets/src/bsd.c packages/bun-usockets/src/udp.c packages/bun-usockets/src/crypto/openssl.c packages/bun-usockets/src/eventing/epoll_kqueue.c; do
  o="$VLIB/us/$i.o"
  if $CC --target=wasm32-wasip2 $FLAGS -c "$c" -o "$o" 2>"$VLIB/us/$i.err"; then objs+=("$o"); else fails=$((fails+1)); echo "FAIL $(basename $c): $(grep -m1 -oE 'error: .{0,50}|fatal error: .{0,40}' $VLIB/us/$i.err|head -1)"; fi
  i=$((i+1))
done
# The TLS server's SNI hostname tree is C++ (sni_tree.cpp) — the ONLY C++ TU in
# uSockets. Without it sni_new/sni_add/sni_find/sni_remove/sni_free are undefined
# and the old blind `void sni_*(void){}` stubs (quic_syscall_stubs.c) turned any
# TLS handshake into a signature_mismatch trap. Compile it (no exceptions/rtti,
# matching the C++ deps) and add it to the archive.
o="$VLIB/us/sni_tree.o"
if $CXX --target=wasm32-wasip2 -fno-exceptions -fno-rtti -w $FLAGS -c packages/bun-usockets/src/crypto/sni_tree.cpp -o "$o" 2>"$VLIB/us/sni.err"; then objs+=("$o"); else fails=$((fails+1)); echo "FAIL sni_tree.cpp: $(grep -m1 -oE 'error: .{0,50}|fatal error: .{0,40}' $VLIB/us/sni.err|head -1)"; fi
$AR rcs "$VLIB/libusockets.a" "${objs[@]}"
echo "=== usockets: ${#objs[@]} objs, $fails fails, $("$WASI_SDK/bin/llvm-nm" "$VLIB/libusockets.a" 2>/dev/null|grep -c ' T ') T syms"
