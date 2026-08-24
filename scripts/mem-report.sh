#!/usr/bin/env bash
# Measure what a running wk server costs in memory, at several guest
# memory-reservation sizes. wasmtime reserves per *linear memory*, so the
# number that moves is `VM_ALLOCATE`: 4GiB of address space per running node at
# the default.
#
# macOS only (reads `vmmap`). Deliberately NOT `ps -o vsz`: on Apple Silicon
# every process reports ~390GiB of address space — an idle `bash` does — so VSZ
# drowns the thing being measured. `vmmap` separates it:
#
#   VM_ALLOCATE        virtual reservations, where guest memories land
#   Physical footprint dirty pages, i.e. what a phone's jetsam actually counts
set -uo pipefail

WORKSPACE="${1:-example/live-coding.wk}"
WK="${WK:-target/debug/wk}"
SETTLE="${SETTLE:-12}"

[ -x "$WK" ] || { echo "no $WK — cargo build first"; exit 1; }
[ -f "$WORKSPACE" ] || { echo "no such workspace: $WORKSPACE"; exit 1; }
command -v vmmap >/dev/null || { echo "vmmap not found (macOS only)"; exit 1; }

cleanup() { pkill -f "wk run $WORKSPACE" 2>/dev/null; }
trap cleanup EXIT

sample() { # $1 = label, $2 = env assignment
  cleanup; sleep 1
  env "$2" nohup "$WK" run "$WORKSPACE" --headless >/tmp/wk-mem-report.log 2>&1 &
  sleep "$SETTLE"
  local pid running map footprint vmalloc
  pid="$(pgrep -f "wk run $WORKSPACE" | head -1)"
  if [ -z "$pid" ]; then printf '  %-22s (server did not start)\n' "$1"; return; fi
  running="$("$WK" -f "$WORKSPACE" ps 2>/dev/null | grep -c running)"
  map="$(vmmap -summary "$pid" 2>/dev/null)"
  footprint="$(awk '/^Physical footprint:/ {print $3; exit}' <<<"$map")"
  # The first VM_ALLOCATE row is the mapped total; a "(reserved)" row may follow.
  vmalloc="$(awk '/^VM_ALLOCATE / {print $2; exit}' <<<"$map")"
  printf '  %-22s guest memories %-9s dirty %-8s (%s nodes running)\n' \
    "$1" "${vmalloc:-?}" "${footprint:-?}" "$running"
  cleanup; sleep 1
}

echo "memory by guest reservation size — $WORKSPACE"
echo
sample "wasmtime default"  "WK_MEM_REPORT=1"
sample "64 MiB per memory" "WK_MEMORY_RESERVATION_MIB=64"
sample "0 (exact size)"    "WK_MEMORY_RESERVATION_MIB=0"
echo
echo "'guest memories' is vmmap's VM_ALLOCATE — the address space wasmtime"
echo "reserves per linear memory. 'dirty' is the physical footprint, which is"
echo "what a memory-constrained platform actually enforces."
