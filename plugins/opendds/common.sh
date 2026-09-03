#!/usr/bin/env bash
# Shared settings for the OpenDDS port. Sourced by every build-*.sh here.
#
# Layout (everything but the scripts, patches, shim and demo sources is
# gitignored — see ./.gitignore):
#
#   src/OpenDDS      upstream OpenDDS checkout, pinned to $OPENDDS_TAG
#   src/OpenDDS/ACE_TAO
#                    ACE+TAO, downloaded by OpenDDS's own configure script
#   host/            the NATIVE build: ACE, TAO and the two IDL compilers
#                    (tao_idl, opendds_idl) that the cross build has to run
#   build-target/    the wasm32-wasip2 build
#   logs/            one log per stage
#
# The version pin lives here and nowhere else.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# OpenDDS release. A tag, never a branch: this port patches upstream, and a
# moving base would silently invalidate patches/.
OPENDDS_TAG="v3.34.0"
OPENDDS_REPO="https://github.com/OpenDDS/OpenDDS.git"

# Which ACE/TAO configure should fetch, by acetao.ini section name. ACE 8 /
# TAO 4 is the newest and the one that wants C++17 — the fewest legacy
# platform assumptions to unpick for a target that is neither POSIX nor
# Windows. (`--doc-group` would give ACE 6/TAO 2, the OpenDDS default.)
ACE_TAO_SET="ace8tao4"

SRC="$HERE/src/OpenDDS"
HOST="$HERE/host"
TARGET="$HERE/build-target"
LOGS="$HERE/logs"

mkdir -p "$LOGS"

# wasi-sdk, resolved by mise in mise.toml; the fallback is for running a
# script directly outside mise.
WASI_SDK="${WASI_SDK:-$HOME/wasi-sdk}"

# Every plugin in this repo builds against ONE pinned wasi-sdk (root
# mise.toml). Guard rather than half-build: a different sysroot changes which
# libc gaps this port has to shim, and the failures land far from the cause.
require_wasi_sdk() {
  if [ ! -x "$WASI_SDK/bin/clang" ]; then
    echo "opendds: no wasi-sdk at $WASI_SDK (set WASI_SDK, or run via 'mise run ...')" >&2
    exit 1
  fi
}

log() { printf '\n== %s\n' "$*"; }
