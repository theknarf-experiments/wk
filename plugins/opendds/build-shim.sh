#!/usr/bin/env bash
# Build the two link-line shims into shim/libwkopendds.a (~1s).
#
#   wk-opendds-threads.c   the threading policy: no-op mutexes, and a condition
#                          wait that PUMPS the reactor and returns as a spurious
#                          wakeup. See PORTING.md, "One thread, and a condition
#                          variable that pumps".
#   wk-opendds-net.c       sendmsg/recvmsg over sendto/recvfrom, which
#                          wasi-libc declares nowhere and OpenDDS's rtps_udp
#                          transport is built on.
#   wk-opendds-inline.c    the inline-runnable registry: what a thread becomes
#                          when there are none. OpenDDS's DispatchService and
#                          ReactorTask register one pass of their event loop
#                          here (patches/opendds-0002) and the pump runs them.
#   wk-opendds-mcast.c     the multicast socket options, which succeed because
#                          the fabric delivers a group to every member of a
#                          Network without being asked. Needs -Wl,--wrap, and
#                          the link lines below and in ace/platform_wasi.GNU
#                          pass it.
#
# Every OpenDDS node links this archive with
#   -Wl,--whole-archive shim/libwkopendds.a -Wl,--no-whole-archive
# and the --whole-archive is not optional: lld registers archive members as
# lazy symbols and fetches the FIRST archive that offers a name, so being the
# strong definition does not help if libc offered a weak one first.
# --whole-archive force-includes these objects before any reference is
# resolved, so the overrides cannot lose that race wherever they land on the
# link line. plugins/libreoffice/shim/wk-wasi-threads.c documents the trap in
# full, including verification both ways.
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

require_wasi_sdk

cd "$HERE/shim"

# The same flag set as everything else this port builds — see
# ace/platform_wasi.GNU for why each one is here. These are C files that never
# throw, but -fwasm-exceptions still matters: it is what selects wasi-sdk 34's
# eh/ variant of the runtime libraries, and a translation unit built against
# the other one is an ABI mismatch waiting to happen at link.
CFLAGS=(
  --target=wasm32-wasip2
  -O2
  -Wall -Wextra
  -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_MMAN
  -D_WASI_EMULATED_PROCESS_CLOCKS -D_WASI_EMULATED_GETPID
  -fwasm-exceptions -mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false
  -I"$HERE/shim/include"
)

log "building the shim archive"
for src in wk-opendds-threads.c wk-opendds-net.c wk-opendds-inline.c wk-opendds-mcast.c; do
  "$WASI_SDK/bin/clang" "${CFLAGS[@]}" -c "$src" -o "${src%.c}.o"
  echo "  $src"
done

rm -f libwkopendds.a
"$WASI_SDK/bin/llvm-ar" rcs libwkopendds.a wk-opendds-threads.o wk-opendds-net.o wk-opendds-inline.o wk-opendds-mcast.o
echo "built plugins/opendds/shim/libwkopendds.a"
