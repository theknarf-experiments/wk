#!/usr/bin/env bash
# Stage 1 — the NATIVE build, into ./host/OpenDDS.
#
# OpenDDS cannot cross-compile without a host build of itself, for the same
# reason Qt cannot (plugins/qt/build-host.sh): the target build runs two code
# generators — TAO's `tao_idl` and OpenDDS's `opendds_idl` — over every .idl
# file, and a wasm32 binary is not something the build machine can execute.
# Upstream's own instructions for every cross target say to do this in a
# SEPARATE copy of the tree (docs/devguide/building/android.rst:106), because
# a configure run rewrites the tree in place: one checkout cannot hold both a
# macOS and a wasm configuration.
#
# So ./host/OpenDDS is a second checkout, cloned from ./src/OpenDDS (local, so
# no second trip to GitHub) and never patched — it is host code, and the
# wasi-libc gaps this port closes do not apply to it.
#
# Long: a full ACE + TAO + tao_idl + opendds_idl build. Run it detached and
# tail logs/host.log.
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

[ -d "$SRC/.git" ] || { echo "opendds: run ./fetch.sh first" >&2; exit 1; }

HOST_DDS="$HOST/OpenDDS"

if [ ! -d "$HOST_DDS/.git" ]; then
  log "cloning the host copy from src/OpenDDS"
  mkdir -p "$HOST"
  git clone --shared "$SRC" "$HOST_DDS"
  # A --shared clone borrows src/'s object store, so this costs a checkout and
  # not a second download. The host copy is disposable; nothing is committed
  # to it.
  git -C "$HOST_DDS" checkout --quiet "$OPENDDS_TAG"
fi

cd "$HOST_DDS"

# THE NATIVE PATH, and it is load-bearing. mise installs wasi-sdk as a global
# tool, so on this machine a bare `clang++` on PATH is the wasm32 cross
# compiler — `which clang++` answers .../wasi-sdk-34-rc.2/bin/clang++. OpenDDS's
# configure auto-detects the compiler by searching PATH, so the host build
# silently configured itself with wasi-sdk's clang and every ACE file died on
# `'Availability.h' file not found` (a macOS SDK header the wasi sysroot of
# course does not have). Narrow the PATH to the system's own tools and name the
# compiler explicitly, so neither the search nor an inherited CC can wander.
export PATH="/usr/bin:/bin:/usr/sbin:/sbin"
unset CC CXX
NATIVE_CXX="/usr/bin/clang++"
[ -x "$NATIVE_CXX" ] || { echo "opendds: no $NATIVE_CXX — install the Xcode command line tools" >&2; exit 1; }

if [ ! -f "$HOST_DDS/setenv.sh" ]; then
  log "configure (host tools only, $ACE_TAO_SET)"
  # --host-tools-only stops after the generators, so this does not build the
  #   DCPS libraries natively — those are only wanted for the target.
  # --ace-tao pins ACE/TAO to the same pairing the target build will use;
  #   tao_idl's output has to match the TAO headers it is compiled against.
  # --no-debug --optimize because nothing here is ever debugged: these are
  #   build tools, and the cross build runs them thousands of times.
  ./configure \
    --host-tools-only \
    --ace-tao="$ACE_TAO_SET" \
    --compiler="$NATIVE_CXX" \
    --no-debug \
    --optimize \
    2>&1 | tee "$LOGS/host-configure.log"
fi

log "make host tools"
# ACE's makefiles are recursive and highly parallel-safe; -j the machine.
make -j"$(sysctl -n hw.ncpu 2>/dev/null || nproc)" 2>&1 | tee "$LOGS/host.log"

log "host tools built"
# The two generators the cross build has to be able to run. `make` here exits 0
# even when a sub-make failed (ACE's makefiles run several targets with errors
# ignored), so check for the artifacts rather than trusting the exit status.
missing=0
for tool in "$HOST_DDS/ACE_wrappers/bin/tao_idl" "$HOST_DDS/bin/opendds_idl"; do
  if [ -x "$tool" ]; then
    echo "  $tool"
  else
    echo "  MISSING: $tool" >&2
    missing=1
  fi
done
[ "$missing" = 0 ] || { echo "opendds: host tools incomplete; see $LOGS/host.log" >&2; exit 1; }
