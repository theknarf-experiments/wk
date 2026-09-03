#!/usr/bin/env bash
# Stage 2 — the wasm32-wasip2 cross build, in ./src/OpenDDS.
#
#   ./build-target.sh            configure (once) and build everything
#   ./build-target.sh ace        configure (once) and build ACE only
#
# START WITH `ace`. It is a few minutes rather than an hour, and every
# wasi-libc gap this port has to close lives in ACE — finding them there costs
# minutes instead of at the end of a long TAO build. Same advice, and the same
# reason, as plugins/libreoffice's "./build-lo.sh sal".
#
# The three files that make WASI a platform ACE knows about:
#
#   ./ace/config-wasi.h        installed into ACE_ROOT/ace/
#   ./ace/platform_wasi.GNU    installed into ACE_ROOT/include/makeinclude/
#   patches/opendds-0001-configure-wasi-target.patch
#                              teaches OpenDDS's configure that `wasi` is a
#                              cross target whose ACE config and platform file
#                              are the two above
#
# The first two are OURS and live in git as ordinary files, not as patches:
# config-<platform>.h and platform_<platform>.GNU are ACE's own extension
# points, so adding a platform is not a modification of upstream. Only the
# configure entry is a genuine diff against OpenDDS, and it is 12 lines.
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

STAGE="${1:-all}"
HOST_DDS="$HOST/OpenDDS"
ACE_ROOT="$SRC/ACE_wrappers"

require_wasi_sdk
[ -d "$SRC/.git" ] || { echo "opendds: run ./fetch.sh first" >&2; exit 1; }
[ -x "$HOST_DDS/bin/opendds_idl" ] || {
  echo "opendds: no host tools — run ./build-host.sh first" >&2; exit 1; }

# --- patches ---------------------------------------------------------------
#
# In two rounds, because the trees arrive at different times: OpenDDS's own
# files are here from ./fetch.sh, but ACE_wrappers/ does not exist until
# configure has downloaded it. Hence opendds-*.patch now and ace-*.patch after
# the configure step below.
#
# Idempotent: a patch that is already applied is skipped rather than re-applied
# or treated as an error, so this script can be re-run after any edit. (git
# apply works on the whole worktree, so it patches ACE_wrappers/ fine even
# though OpenDDS does not track it.)
apply_patches() {
  local what="$1" glob="$2" p
  log "patches: $what"
  for p in $glob; do
    [ -e "$p" ] || continue
    if git -C "$SRC" apply --reverse --check "$p" >/dev/null 2>&1; then
      echo "  already applied: $(basename "$p")"
    elif git -C "$SRC" apply "$p"; then
      echo "  applied: $(basename "$p")"
    else
      echo "opendds: patch failed: $p" >&2
      exit 1
    fi
  done
}
apply_patches OpenDDS "$HERE/patches/opendds-*.patch"

# --- configure -------------------------------------------------------------
if [ ! -f "$SRC/setenv.sh" ]; then
  log "configure (target=wasi, $ACE_TAO_SET)"
  # As in build-host.sh, narrow the PATH: mise puts wasi-sdk's clang on PATH
  # globally, and configure searches PATH for a compiler. Here we WANT the wasm
  # compiler — but we want it named, not found, because configure also runs
  # host-side perl and make from this same PATH.
  #
  # --host-tools points at the native build from stage 1; that is what supplies
  #   tao_idl and opendds_idl to a build that cannot run its own.
  # --macros=WASI_SDK=... is read by ./ace/platform_wasi.GNU, which errors out
  #   without it rather than silently building with the host compiler.
  # --static because wasm has no shared objects at all.
  ( cd "$SRC" && env PATH="/usr/bin:/bin:/usr/sbin:/sbin" \
      ./configure \
        --target=wasi \
        --target-compiler="$WASI_SDK/bin/clang++" \
        --host-tools="$HOST_DDS" \
        --ace-tao="$ACE_TAO_SET" \
        --macros="WASI_SDK=$WASI_SDK" \
        --macros="WK_OPENDDS_SHIM=$HERE/shim" \
        --std=c++17 \
        --static \
        --no-debug \
        --optimize \
        --no-tests \
      2>&1 | tee "$LOGS/target-configure.log" )
fi

# --- install the WASI platform files ---------------------------------------
#
# AFTER configure, because configure is what downloads ACE_wrappers — and
# BEFORE make, which is the first thing that reads them. configure only writes
# `#include "ace/config-wasi.h"` into ace/config.h, so it does not need the
# file to exist while it runs.
[ -d "$ACE_ROOT/ace" ] || { echo "opendds: configure did not produce $ACE_ROOT" >&2; exit 1; }

apply_patches ACE "$HERE/patches/ace-*.patch"

log "installing the WASI platform files"
platform_changed=0
for pair in "ace/config-wasi.h:$ACE_ROOT/ace/config-wasi.h" \
            "ace/platform_wasi.GNU:$ACE_ROOT/include/makeinclude/platform_wasi.GNU"; do
  from="$HERE/${pair%%:*}" to="${pair#*:}"
  if ! cmp -s "$from" "$to"; then
    cp "$from" "$to"
    platform_changed=1
    echo "  ${pair%%:*}  (changed)"
  else
    echo "  ${pair%%:*}"
  fi
done

# ACE's makefiles do NOT depend on ace/config.h — they have no generated
# dependency file unless `make depend` has been run — so an edit to
# config-wasi.h leaves every previously-built object in place, compiled against
# the old configuration. The symptom is not a compile error but a LINK error in
# something unrelated: turning ACE_LACKS_UNIX_SYSLOG on and rebuilding produced
# a clean ACE whose Log_Msg.o, untouched since before the change, still
# referenced `vtable for ACE_Log_Msg_UNIX_Syslog` from a translation unit that
# no longer compiles anything.
#
# So: a changed platform file discards the objects. ACE is a few minutes; a
# silently half-configured library is not worth the minutes saved.
#
# shim/include/*.h is deliberately NOT in this check. Those headers only ADD
# declarations wasi-libc withholds, so a file that compiled without them is
# unaffected by their arrival, and including them here would turn every added
# constant into a full ACE+TAO rebuild. If you ever CHANGE or remove something
# there, clean by hand.
if [ "$platform_changed" = 1 ] && [ "${WK_OPENDDS_KEEP_OBJECTS:-0}" = 1 ]; then
  log "platform files changed, but WK_OPENDDS_KEEP_OBJECTS=1 — keeping objects"
  # The escape hatch, for a change that provably cannot affect any object
  # already compiled: a pure LDFLAGS edit, or a new #define that nothing built
  # so far referenced. Set it deliberately and only then — the default is to
  # throw the objects away, because the failure mode it prevents (a library
  # half-configured across two versions of config-wasi.h) presents as a link
  # error in an unrelated file and costs far more than the rebuild.
elif [ "$platform_changed" = 1 ]; then
  log "platform files changed — discarding ACE/TAO objects built against the old ones"
  find "$ACE_ROOT" -type d -name .obj -prune -exec rm -rf {} + 2>/dev/null || true
  find "$ACE_ROOT" -type d -name .shobj -prune -exec rm -rf {} + 2>/dev/null || true
  rm -f "$ACE_ROOT"/lib/*.a "$ACE_ROOT"/ace/*.a 2>/dev/null || true
fi

# An ACE_LACKS_* that ACE has never heard of is SILENTLY IGNORED — the config
# looks right and configures nothing, and the symptom is a link error in an
# unrelated file an hour later. ACE has also retired macros over the years
# (ACE_LACKS_AUTO_PTR, ACE_LACKS_RPC_H and friends survive only in ChangeLogs),
# so "it was valid once" is not good enough. Check every name against ACE's
# actual sources, every time.
log "checking every ACE_LACKS_/ACE_HAS_ name against ACE's sources"
unknown=0
# Strip comments first: this file explains itself at length and mentions macro
# names in prose.
names=$(sed 's://.*::' "$HERE/ace/config-wasi.h" | grep -oE '\bACE_(LACKS|HAS)_[A-Z0-9_]+' | sort -u)
for m in $names; do
  if ! grep -rqE "\b$m\b" "$ACE_ROOT/ace" "$ACE_ROOT/TAO/tao" 2>/dev/null; then
    echo "  UNKNOWN to ACE: $m" >&2
    unknown=1
  fi
done
[ "$unknown" = 0 ] || {
  echo "opendds: ace/config-wasi.h names macros ACE does not use; fix them before building" >&2
  exit 1; }
echo "  all names known"

# --- build -----------------------------------------------------------------
JOBS="$(sysctl -n hw.ncpu 2>/dev/null || nproc)"
export PATH="/usr/bin:/bin:/usr/sbin:/sbin"

# ACE's makefiles are driven entirely by ACE_ROOT/TAO_ROOT/DDS_ROOT — the very
# first line of every GNUmakefile is `include $(ACE_ROOT)/include/makeinclude/
# macros.GNU`, which without them reads as `/include/...` and fails with a
# misleading "No rule to make target". configure writes setenv.sh for exactly
# this; source it rather than restating the paths, so the build always agrees
# with what was configured.
# setenv.sh expands ${LD_LIBRARY_PATH} unguarded, which is fatal under the
# `set -u` this port builds with, so drop -u across the source and no further.
# shellcheck source=/dev/null
set +u; source "$SRC/setenv.sh"; set -u
# setenv.sh appends the target tree's bin dirs to PATH. Put the HOST tools
# ahead of them: the generators the cross build runs must be the arm64 ones
# from stage 1, never wasm binaries out of this tree.
export PATH="$HOST_DDS/ACE_wrappers/bin:$HOST_DDS/bin:$PATH"
# Both are read by ace/platform_wasi.GNU, which errors out without them rather
# than silently building with the host compiler or without the port's headers.
# configure also writes them into platform_macros.GNU (--macros above); they
# are exported as well so that an ALREADY-configured tree picks up a change
# here without a reconfigure — GNU make imports the environment as variables,
# and both are declared with `?=`.
export WASI_SDK
export WK_OPENDDS_SHIM="$HERE/shim"

# ace/platform_wasi.GNU puts the shim archive on every executable link line, so
# it has to exist before make reaches the first one. A second or two.
"$HERE/build-shim.sh"

case "$STAGE" in
  ace)
    log "make ACE only (the short, high-yield rung)"
    ( cd "$ACE_ROOT/ace" && make -j"$JOBS" ) 2>&1 | tee "$LOGS/target-ace.log"
    ;;
  all)
    log "make everything (ACE, TAO, OpenDDS) — long"
    ( cd "$SRC" && make -j"$JOBS" ) 2>&1 | tee "$LOGS/target.log"
    ;;
  *)
    echo "opendds: unknown stage '$STAGE' (want: ace, all)" >&2
    exit 1
    ;;
esac
