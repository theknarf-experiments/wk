#!/bin/bash
N="$BUN_NATIVE"; WASI_SDK="$WASI_SDK"; P="$BUN_PLUGIN"; B="$BUN"
CC="$WASI_SDK/bin/clang"; CXX="$WASI_SDK/bin/clang++"; AR="$WASI_SDK/bin/llvm-ar"
cd "$B"
FLAGS="-O2 -DLIBUS_USE_EPOLL -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_MMAN -D_WASI_EMULATED_GETPID -DLIBUS_USE_OPENSSL -I$BUN_NATIVE/boringssl/include -include $P/wasi-compat/sys/socket_compat.h -include $P/wasi-compat/wasi_signal_compat.h -I packages/bun-usockets/src -I $N/mimalloc/include -idirafter $P/wasi-compat"
mkdir -p /tmp/vlib/us; objs=(); i=0; fails=0
for c in packages/bun-usockets/src/loop.c packages/bun-usockets/src/socket.c packages/bun-usockets/src/context.c packages/bun-usockets/src/bsd.c packages/bun-usockets/src/udp.c packages/bun-usockets/src/crypto/openssl.c packages/bun-usockets/src/eventing/epoll_kqueue.c; do
  o="/tmp/vlib/us/$i.o"
  if $CC --target=wasm32-wasip2 $FLAGS -c "$c" -o "$o" 2>"/tmp/vlib/us/$i.err"; then objs+=("$o"); else fails=$((fails+1)); echo "FAIL $(basename $c): $(grep -m1 -oE 'error: .{0,50}|fatal error: .{0,40}' /tmp/vlib/us/$i.err|head -1)"; fi
  i=$((i+1))
done
# The TLS server's SNI hostname tree is C++ (sni_tree.cpp) — the ONLY C++ TU in
# uSockets. Without it sni_new/sni_add/sni_find/sni_remove/sni_free are undefined
# and the old blind `void sni_*(void){}` stubs (quic_syscall_stubs.c) turned any
# TLS handshake into a signature_mismatch trap. Compile it (no exceptions/rtti,
# matching the C++ deps) and add it to the archive.
o="/tmp/vlib/us/sni_tree.o"
if $CXX --target=wasm32-wasip2 -fno-exceptions -fno-rtti -w $FLAGS -c packages/bun-usockets/src/crypto/sni_tree.cpp -o "$o" 2>"/tmp/vlib/us/sni.err"; then objs+=("$o"); else fails=$((fails+1)); echo "FAIL sni_tree.cpp: $(grep -m1 -oE 'error: .{0,50}|fatal error: .{0,40}' /tmp/vlib/us/sni.err|head -1)"; fi
$AR rcs /tmp/vlib/libusockets.a "${objs[@]}"
echo "=== usockets: ${#objs[@]} objs, $fails fails, $("$WASI_SDK/bin/llvm-nm" /tmp/vlib/libusockets.a 2>/dev/null|grep -c ' T ') T syms"
