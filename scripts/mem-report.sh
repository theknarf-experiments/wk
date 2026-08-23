#!/usr/bin/env bash
# Measure how much address space a running wk server reserves, at several guest
# memory-reservation sizes. wasmtime reserves per *linear memory*, so the number
# that matters is the slope: how much each running node adds.
#
# macOS only (reads `ps -o vsz`). Address space, not resident memory — the
# reserved pages are never touched, which is exactly why this is invisible on a
# desktop and fatal on a phone.
set -uo pipefail

WORKSPACE="${1:-example/live-coding.wk}"
WK="${WK:-target/debug/wk}"
SETTLE="${SETTLE:-12}"

[ -x "$WK" ] || { echo "no $WK — cargo build first"; exit 1; }
[ -f "$WORKSPACE" ] || { echo "no such workspace: $WORKSPACE"; exit 1; }

cleanup() { pkill -f "wk run $WORKSPACE" 2>/dev/null; }
trap cleanup EXIT

sample() { # $1 = label, $2 = env assignment
  cleanup; sleep 1
  env "$2" nohup "$WK" run "$WORKSPACE" --headless >/tmp/wk-mem-report.log 2>&1 &
  sleep "$SETTLE"
  local pid running
  pid="$(pgrep -f "wk run $WORKSPACE" | head -1)"
  if [ -z "$pid" ]; then printf '  %-22s (server did not start)\n' "$1"; return; fi
  running="$("$WK" -f "$WORKSPACE" ps 2>/dev/null | grep -c running)"
  ps -o vsz,rss -p "$pid" | tail -1 |
    awk -v l="$1" -v r="$running" \
      '{printf "  %-22s VSZ %8.1f GiB   RSS %5.0f MB   (%s nodes running)\n", l, $1/1048576, $2/1024, r}'
  cleanup; sleep 1
}

echo "address space by guest memory reservation — $WORKSPACE"
echo
sample "wasmtime default"  "WK_MEM_REPORT=1"
sample "64 MiB per memory" "WK_MEMORY_RESERVATION_MIB=64"
sample "0 (exact size)"    "WK_MEMORY_RESERVATION_MIB=0"
echo
echo "The default reserves 4 GiB of address space per linear memory; the gap"
echo "between the rows is what that costs at this node count."
