#!/usr/bin/env bash
# Build the two DDS wk nodes: dds-publisher.wasm and dds-subscriber.wasm.
#
#   ./build-nodes.sh          build both
#   ./build-nodes.sh run      build, then run the pair on loopback under
#                             wasmtime (a smoke test before wiring them in wk)
#
# Needs ./build-target.sh to have completed — this links against the OpenDDS,
# TAO and ACE static libraries it produces.
#
# THE IDL, AND WHY IT NEEDS THE HOST TOOLS
# ========================================
# A DDS type is written in IDL and compiled twice, by two generators that must
# RUN on the build machine and therefore cannot be the wasm build's own:
#
#   opendds_idl  WkMessage.idl -> WkMessageTypeSupport.idl + ...Impl.{h,cpp}
#   tao_idl      WkMessage.idl -> WkMessageC/S.{h,cpp}, .inl
#   tao_idl      WkMessageTypeSupport.idl -> ...TypeSupportC/S.{h,cpp}
#
# Both come from ./host/OpenDDS (stage 1). This is the whole reason this port
# needs a native build at all; see build-host.sh.
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

require_wasi_sdk

HOST_DDS="$HOST/OpenDDS"
ACE_ROOT="$SRC/ACE_wrappers"
NODES="$HERE/nodes"

[ -f "$ACE_ROOT/lib/libACE.a" ] || {
  echo "opendds: no target libraries — run ./build-target.sh first" >&2; exit 1; }
[ -x "$HOST_DDS/bin/opendds_idl" ] || {
  echo "opendds: no host tools — run ./build-host.sh first" >&2; exit 1; }

"$HERE/build-shim.sh"

# --- generate ---------------------------------------------------------------
log "generating type support from WkMessage.idl"
(
  cd "$NODES"
  export DDS_ROOT="$HOST_DDS"
  export ACE_ROOT="$HOST_DDS/ACE_wrappers"
  export TAO_ROOT="$HOST_DDS/ACE_wrappers/TAO"
  IDLFLAGS=(--idl-version 4 --unknown-annotations ignore -Sa -St -I"$DDS_ROOT")
  "$DDS_ROOT/bin/opendds_idl" "${IDLFLAGS[@]}" WkMessage.idl
  "$ACE_ROOT/bin/tao_idl" "${IDLFLAGS[@]}" -as WkMessage.idl
  "$ACE_ROOT/bin/tao_idl" "${IDLFLAGS[@]}" -as WkMessageTypeSupport.idl
)

# --- compile and link -------------------------------------------------------
#
# The flag set is ace/platform_wasi.GNU's, because every object in the
# libraries was built with it and a translation unit that disagrees about the
# exception encoding is rejected at INSTANTIATE time, not at link.
CXXFLAGS=(
  --target=wasm32-wasip2
  -std=c++17 -O2
  -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_MMAN
  -D_WASI_EMULATED_PROCESS_CLOCKS -D_WASI_EMULATED_GETPID
  -fwasm-exceptions -mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false
  -DACE_AS_STATIC_LIBS -DTAO_AS_STATIC_LIBS -DOPENDDS_AS_STATIC_LIBS
  -I"$ACE_ROOT" -I"$ACE_ROOT/TAO" -I"$SRC" -I"$HERE/shim/include" -I"$NODES"
  -Wno-unused-parameter
)

# Link order is bottom-up and it matters for static archives: each library may
# only reference symbols in the ones to its RIGHT. This is upstream's own order
# (DevGuideExamples/.../GNUmakefile.Messenger_Publisher), with the shim first
# and whole-archive so its overrides win — see shim/wk-opendds-threads.c.
LDLIBS=(
  -Wl,--whole-archive "$HERE/shim/libwkopendds.a" -Wl,--no-whole-archive
  # Lets the shim accept the multicast socket options and pass every other one
  # through to wasi-libc; see shim/wk-opendds-mcast.c.
  -Wl,--wrap=setsockopt
  -L"$SRC/lib" -L"$ACE_ROOT/lib"
  -lOpenDDS_Rtps_Udp -lOpenDDS_Rtps -lOpenDDS_Dcps
  -lTAO_BiDirGIOP -lTAO_PI -lTAO_CodecFactory -lTAO_Valuetype
  -lTAO_PortableServer -lTAO_AnyTypeCode -lTAO -lACE
  -lunwind -lsetjmp
  -lwasi-emulated-signal -lwasi-emulated-mman
  -lwasi-emulated-process-clocks -lwasi-emulated-getpid
  # 8 MB, as everything else in this port: CDR marshaling and the RTPS
  # parameter-list walk both recurse, and overflowing the 64 KB default shadow
  # stack presents as an unattributed trap.
  -Wl,-z,stack-size=8388608
)

GENERATED=(
  "$NODES/WkMessageC.cpp"
  "$NODES/WkMessageS.cpp"
  "$NODES/WkMessageTypeSupportC.cpp"
  "$NODES/WkMessageTypeSupportS.cpp"
  "$NODES/WkMessageTypeSupportImpl.cpp"
)

for node in publisher subscriber; do
  log "linking dds-$node.wasm"
  "$WASI_SDK/bin/clang++" "${CXXFLAGS[@]}" \
    "$NODES/$node.cpp" "${GENERATED[@]}" \
    "${LDLIBS[@]}" \
    -o "$HERE/dds-$node.wasm"
  echo "built plugins/opendds/dds-$node.wasm"
done

if [ "${1:-}" = "run" ]; then
  log "running the pair on loopback under wasmtime"
  # Both participants share 127.0.0.1 here, so OpenDDS walks participant ids
  # and the second binds SPDP two ports up (17910, then 17912). Each therefore
  # has to be told the other's port. Under wk this does not arise: every node
  # has an address of its own, every node is participant 0, and `--peer <node>`
  # is the entire configuration.
  echo "(loopback rehearsal; under wk it is just: --peer <node-name>)"
  ( wasmtime run -W exceptions -S inherit-network --dir=/tmp::/tmp \
      "$HERE/dds-subscriber.wasm" --peer 127.0.0.1:17912 --self 127.0.0.1 --count 5 \
      2>&1 | grep -v MulticastManager | sed 's/^/[sub] /' & )
  sleep 3
  wasmtime run -W exceptions -S inherit-network --dir=/tmp::/tmp \
    "$HERE/dds-publisher.wasm" --peer 127.0.0.1:17910 --self 127.0.0.1 --count 5 \
    2>&1 | grep -v MulticastManager | sed 's/^/[pub] /'
  sleep 5
fi
