#!/bin/bash
# The C++ side of the bun-run link, compiled with the committed clang
# response file (../cxx-flags.rsp, $BUN_NATIVE/$BUN_CODEGEN/$BUN_PLUGIN
# placeholders → expanded to $WORK/bunflags.rsp here):
#
#   * bunobj/  — the sweep of src/jsc/bindings + src/runtime/napi (minus
#     windows/, the v8 shim dir, and the highway json/xml helpers) PLUS the
#     handful of TUs the sweep's name filters would wrongly drop but the
#     link REQUIRES (the *_testing.cpp / *ForTesting.cpp family backing
#     bun:internal-for-testing, and Bake's source provider). FAIL-TOLERANT
#     by design: the symbols of TUs that don't compile on wasi are covered
#     by link/trap_stubs*.c — the final link resolving with zero undefined
#     is the gate. A TU that already has a .o is skipped — FORCE_CXX=1
#     recompiles everything; .o files NOT on the candidate list are pruned
#     (link_all.sh globs bunobj/*.o).
#   * gen_*.o  — nine codegen TUs (REQUIRED; hard error on failure).
#   * mod_*.o  — src/jsc/modules/*.cpp (fail-tolerant, like the sweep).
#   * libuwsockets.o, us_root_certs.o, bun_simdutf.o — single required TUs.
#   * imrc.o   — InternalModuleRegistryConstants.bin wrapped by the .wasm.S.
#     The committed .S carries a HARDCODED .size; it is COMPUTED here (sed
#     into a work-dir copy — the committed file is never trusted for size)
#     and assembled from codegen/ so .incbin finds the .bin.
#
# Recovered from the original porting session's one-off commands; flags are
# byte-identical, only output paths moved from /tmp + session scratchpad to
# $WORK (native/runtime-build).
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
P="${BUN_PLUGIN:-$(cd "$HERE/.." && pwd)}"
N="${BUN_NATIVE:-$P/native}"
B="${BUN:-$P/bun}"
WORK="${WORK:-$N/runtime-build}"
OBJ="${OBJ:-$WORK/obj}"
WASI_SDK="${WASI_SDK:?set WASI_SDK (wasi-sdk-34-rc.2)}"
CXX="$WASI_SDK/bin/clang++"
mkdir -p "$WORK/bunobj" "$WORK/lists" "$WORK/logs" "$OBJ"

# ── response file: expand the committed placeholders ──────────────────────
RSP="$WORK/bunflags.rsp"
sed -e "s|\$BUN_NATIVE|$N|g" -e "s|\$BUN_CODEGEN|$P/codegen|g" -e "s|\$BUN_PLUGIN|$P|g" \
    "$P/cxx-flags.rsp" > "$RSP"

cd "$B"   # the rsp's -I paths are relative to the bun checkout

# ── the bindings sweep ────────────────────────────────────────────────────
LIST="$WORK/lists/cxx_list.txt"
{
  find src/jsc/bindings src/runtime/napi -name "*.cpp" \
    | grep -vE "windows/|test|Test|highway_json|highway_xml" \
    | grep -v "bindings/v8/"
  # The filters above are name-based and would drop these link-REQUIRED TUs:
  # bun:internal-for-testing's native half (JS2Native references them) and
  # Bake's source provider (BakeLoadInitialServerCode & co).
  echo src/jsc/bindings/xxhash3_testing.cpp
  echo src/jsc/bindings/highway_strings_testing.cpp
  echo src/jsc/bindings/InternalForTesting.cpp
  echo src/jsc/bindings/NoOpForTesting.cpp
  echo src/jsc/bindings/JSCTestingHelpers.cpp
  echo src/runtime/bake/BakeSourceProvider.cpp
  echo src/runtime/bake/DevServerSourceProvider.cpp
} | sort -u > "$LIST"
if [ "${FORCE_CXX:-0}" = 1 ]; then rm -f "$WORK/bunobj"/*.o; fi
# Prune objects that are not on the candidate list (link_all.sh globs
# bunobj/*.o — a stale object from an edited list would still be linked).
tr '/' '_' < "$LIST" > "$WORK/lists/cxx_list.mangled"
for o in "$WORK/bunobj"/*.o; do
  [ -e "$o" ] || break
  tag=$(basename "$o" .o)
  grep -qxF "$tag" "$WORK/lists/cxx_list.mangled" || rm -f "$o"
done
FAILS="$WORK/logs/cxx_fails.txt"; : > "$FAILS"
compile_one() {
  local f="$1"; local tag=$(echo "$f" | tr '/' '_')
  [ -f "$WORK/bunobj/$tag.o" ] && { echo "SKIP"; return; }
  if "$CXX" @"$RSP" -c "$f" -o "$WORK/bunobj/$tag.o" 2>"$WORK/bunobj/$tag.err"; then echo "OK"; else
    echo "$f | $(grep -m1 -oE "fatal error: '[^']*'|error: .{0,55}" "$WORK/bunobj/$tag.err" | head -1)" >> "$FAILS"
    echo "FAIL"
  fi
}
export -f compile_one; export CXX RSP WORK FAILS
NPROC=$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)
echo "== bindings sweep ($(wc -l < "$LIST" | tr -d ' ') TUs, -P$NPROC, log: $WORK/logs/cxx_run.log)"
xargs -P"$NPROC" -I{} bash -c 'compile_one "$@"' _ {} < "$LIST" > "$WORK/logs/cxx_run.log"
echo "   OK=$(grep -c '^OK$' "$WORK/logs/cxx_run.log") SKIP=$(grep -c '^SKIP$' "$WORK/logs/cxx_run.log") FAIL=$(wc -l < "$FAILS" | tr -d ' ') (fails tolerated — trap stubs cover them; the link is the gate)"

# ── codegen TUs (all required) ────────────────────────────────────────────
for f in GeneratedBindings JSSink WebCoreJSBuiltins ZigGeneratedClasses \
         GeneratedSSLConfig GeneratedSocketConfig GeneratedSocketConfigHandlers \
         GeneratedSocketConfigBinaryType GeneratedFakeTimersConfig; do
  "$CXX" @"$RSP" -c "../codegen/$f.cpp" -o "$OBJ/gen_$f.o" \
    || { echo "build_cxx_objects: REQUIRED codegen TU failed: $f" >&2; exit 1; }
done
echo "== gen_*.o: 9 OK"

# ── src/jsc/modules (fail-tolerant, like the sweep) ───────────────────────
rm -f "$OBJ"/mod_*.o
for f in $(find src/jsc/modules -name "*.cpp" | grep -vE "test|Test"); do
  tag=$(echo "$f" | tr '/' '_')
  "$CXX" @"$RSP" -c "$f" -o "$OBJ/mod_$tag.o" 2>"$WORK/logs/mod_$tag.err" \
    && echo "   mod OK $f" || { rm -f "$OBJ/mod_$tag.o"; echo "   mod FAIL $f (tolerated)"; }
done

# ── uWebSockets C API (bmalloc backend via root.h, epoll+openssl arms) ────
"$CXX" @"$RSP" \
  -DBUSE_SYSTEM_MALLOC=1 -DLIBUS_USE_EPOLL -DLIBUS_USE_OPENSSL \
  -Ipackages/bun-uws/src \
  -include src/jsc/bindings/root.h \
  -include "$P/wasi-compat/sys/socket_compat.h" \
  -include "$P/wasi-compat/wasi_signal_compat.h" \
  -c src/uws_sys/libuwsockets.cpp -o "$OBJ/libuwsockets.o"
echo "== libuwsockets.o OK"

# ── uSockets root certificate store ───────────────────────────────────────
"$CXX" --target=wasm32-wasip2 -O2 -fno-exceptions -fno-rtti \
  -DLIBUS_USE_EPOLL -DLIBUS_USE_OPENSSL \
  -include "$P/wasi-compat/sys/socket_compat.h" \
  -I packages/bun-usockets/src -I "$N/boringssl/include" -I "$N/mimalloc/include" \
  -idirafter "$P/wasi-compat" \
  -c packages/bun-usockets/src/crypto/root_certs.cpp -o "$OBJ/us_root_certs.o"
echo "== us_root_certs.o OK"

# ── simdutf (lives in WTF; bun's bridge TU, no separate fetch) ────────────
"$CXX" @"$RSP" -c src/simdutf_sys/bun-simdutf.cpp -o "$OBJ/bun_simdutf.o"
echo "== bun_simdutf.o OK"

# ── InternalModuleRegistryConstants → imrc.o ──────────────────────────────
# The .size in the committed .wasm.S must equal the actual .bin byte count;
# compute it and sed into a work-dir copy rather than trusting the file.
BIN="$P/codegen/InternalModuleRegistryConstants.bin"
[ -f "$BIN" ] || { echo "build_cxx_objects: $BIN missing (run gen-codegen)" >&2; exit 1; }
SZ=$(wc -c < "$BIN" | tr -d ' ')
sed "s/\.size bun_internal_modules_data,  *[0-9]*/.size bun_internal_modules_data,  $SZ/" \
    "$P/link/InternalModuleRegistryConstants.wasm.S" > "$WORK/imrc.wasm.S"
( cd "$P/codegen" && "$WASI_SDK/bin/clang" --target=wasm32-wasip2 -c "$WORK/imrc.wasm.S" -o "$OBJ/imrc.o" )
echo "== imrc.o OK (.size = $SZ)"

echo "DONE cxx objects"
