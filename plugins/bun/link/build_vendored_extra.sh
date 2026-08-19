#!/bin/bash
# The rest of the vendored C libraries for the bun-run link, cross-built for
# wasm32-wasip2 into $VLIB (archives) and $OBJ (hdrhistogram's three .o):
#
#   c-ares, libarchive, zlib-ng (compat mode), libdeflate, sqlite3, llhttp,
#   libspng, libjpeg-turbo, hdrhistogram
#
# These recipes were recovered from the original porting session (they were
# run as one-off commands and never committed; link/README.md called the
# result "documented rather than hermetic"). Flags are byte-identical to the
# final working command for each library; only the output paths moved from
# /tmp/vlib to $VLIB. Each library is skipped when its archive already
# exists — delete the archive (or FORCE_VLIB=1 via build-runtime.sh) to
# rebuild.
#
# Source trees live under native/ (fetched by build-runtime.sh at pinned
# commits). The hand-written wasi configs live in link/configs/ and are
# installed into the source trees here.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
P="${BUN_PLUGIN:-$(cd "$HERE/.." && pwd)}"
N="${BUN_NATIVE:-$P/native}"
B="${BUN:-$P/bun}"
WORK="${WORK:-$N/runtime-build}"
VLIB="${VLIB:-$WORK/vlib}"
OBJ="${OBJ:-$WORK/obj}"
WASI_SDK="${WASI_SDK:?set WASI_SDK (wasi-sdk-34-rc.2)}"
CC="$WASI_SDK/bin/clang"; AR="$WASI_SDK/bin/llvm-ar"; NM="$WASI_SDK/bin/llvm-nm"
mkdir -p "$VLIB" "$OBJ"

# ── c-ares ─────────────────────────────────────────────────────────────────
# Hand-written wasi ares_config.h (configs/); AF_MAX/EHOSTDOWN and the
# strcmpi aliases paper over BSD-isms wasi-libc lacks.
if [ ! -f "$VLIB/libcares.a" ]; then
  [ -f "$N/cares/src/lib/ares_config.h" ] || cp "$P/link/configs/ares_config.h" "$N/cares/src/lib/ares_config.h"
  rm -rf "$VLIB/cares"; mkdir -p "$VLIB/cares"; objs=(); fails=0; i=0
  for c in $(find "$N/cares/src/lib" -name "*.c"); do
    o="$VLIB/cares/$i.o"
    if $CC --target=wasm32-wasip2 -O2 -D_GNU_SOURCE -DHAVE_CONFIG_H -DCARES_STATICLIB "-DAF_MAX=42" "-DEHOSTDOWN=112" "-Dstrncmpi=strncasecmp" "-Dstrcmpi=strcasecmp" -I "$N/cares/include" -I "$N/cares/src/lib" -I "$N/cares/src/lib/include" -c "$c" -o "$o" 2>"$VLIB/cares/$i.err"; then objs+=("$o"); else fails=$((fails+1)); fi
    i=$((i+1))
  done
  $AR rcs "$VLIB/libcares.a" "${objs[@]}"
  echo "== cares: ${#objs[@]} objs, $fails fails"
fi

# ── libarchive (tar + gzip subset) ────────────────────────────────────────
# Hand-written wasi config.h (configs/); the pwd/grp/process/fs compat
# headers shim getpwuid/fork-era calls the disk readers make.
if [ ! -f "$VLIB/libarchive.a" ]; then
  [ -f "$N/libarchive/config.h" ] || cp "$P/link/configs/libarchive_config.h" "$N/libarchive/config.h"
  rm -rf "$VLIB/la"; mkdir -p "$VLIB/la"; objs=(); fails=0; i=0
  for c in $(find "$N/libarchive/libarchive" -name "archive_*.c" | grep -vE "/test/|_test\.c"); do
    o="$VLIB/la/$i.o"
    if $CC --target=wasm32-wasip2 -O2 -D_GNU_SOURCE -D__LIBARCHIVE_BUILD=1 -DHAVE_CONFIG_H -DPLATFORM_CONFIG_H='"config.h"' -include ctype.h -include "$P/wasi-compat/pwd.h" -include "$P/wasi-compat/grp.h" -include "$P/wasi-compat/wasi_process_compat.h" -include "$P/wasi-compat/wasi_fs_compat.h" -I "$N/libarchive" -I "$N/libarchive/libarchive" -I "$N/zlib" -I "$N/zstd/lib" -c "$c" -o "$o" 2>"$VLIB/la/$i.err"; then objs+=("$o"); else fails=$((fails+1)); fi
    i=$((i+1))
  done
  $AR rcs "$VLIB/libarchive.a" "${objs[@]}"
  echo "== libarchive: ${#objs[@]} objs, $fails fails"
fi

# ── zlib-ng (compat mode) ─────────────────────────────────────────────────
# Generated headers first (zlib-ng ships .in/.empty templates; its cmake
# usually instantiates them): zlib.h from zlib.h.in (NOT zlib-ng.h.in — the
# punchlist gotcha), empty mangling headers, plain-copied zconf.
if [ ! -f "$VLIB/libz.a" ]; then
  Z="$N/zlib"
  [ -f "$Z/zlib.h" ] || sed -E 's/@ZLIB_SYMBOL_PREFIX@//g; s/@ZLIB_VERSION@/1.3.1.zlib-ng/g; s/@ZLIB_VER_MAJOR@/1/g; s/@ZLIB_VER_MINOR@/3/g; s/@ZLIB_VER_REVISION@/1/g; s/@ZLIB_VER_SUBREVISION@/0/g' "$Z/zlib.h.in" > "$Z/zlib.h"
  [ -f "$Z/zconf.h" ] || sed -E 's/@[A-Z_]+@//g' "$Z/zconf.h.in" > "$Z/zconf.h"
  [ -f "$Z/zconf-ng.h" ] || cp "$Z/zconf-ng.h.in" "$Z/zconf-ng.h"
  [ -f "$Z/zlib_name_mangling.h" ] || cp "$Z/zlib_name_mangling.h.empty" "$Z/zlib_name_mangling.h"
  [ -f "$Z/zlib_name_mangling-ng.h" ] || cp "$Z/zlib_name_mangling-ng.h.empty" "$Z/zlib_name_mangling-ng.h" 2>/dev/null || cp "$Z/zlib_name_mangling.h.empty" "$Z/zlib_name_mangling-ng.h"
  [ -f "$Z/gzread_mangle.h" ] || : > "$Z/gzread_mangle.h"
  rm -rf "$VLIB/z"; mkdir -p "$VLIB/z"; objs=(); nf=0; i=0
  for c in $(find "$Z" -maxdepth 1 -name '*.c'; find "$Z/arch/generic" -name '*.c'); do
    b=$(basename "$c"); case "$b" in *test*|example*|minigzip*|makefixed*|maketrees*) continue;; esac
    o="$VLIB/z/$i.o"
    if $CC --target=wasm32-wasip2 -O2 -DZLIB_COMPAT -DHAVE_UNISTD_H -DWITH_ALL_FALLBACKS -I"$Z" -c "$c" -o "$o" 2>"$VLIB/z/$i.err"; then objs+=("$o"); else nf=$((nf+1)); echo "FAIL $b: $(grep -m1 -oE 'error: .{0,50}' $VLIB/z/$i.err)"; fi
    i=$((i+1))
  done
  $AR rcs "$VLIB/libz.a" "${objs[@]}"
  echo "== zlib: ${#objs[@]} objs, $nf fails, $($NM "$VLIB/libz.a"|grep -c ' T ') T syms"
fi

# ── libdeflate ─────────────────────────────────────────────────────────────
if [ ! -f "$VLIB/libdeflate.a" ]; then
  rm -rf "$VLIB/ld"; mkdir -p "$VLIB/ld"; objs=(); nf=0; i=0
  for c in $(find "$N/libdeflate/lib" -name '*.c'); do
    o="$VLIB/ld/$i.o"
    if $CC --target=wasm32-wasip2 -O2 -DFREESTANDING -I "$N/libdeflate" -c "$c" -o "$o" 2>"$VLIB/ld/$i.err"; then objs+=("$o"); else nf=$((nf+1)); grep -m1 -oE 'error: .{0,50}' "$VLIB/ld/$i.err"; fi
    i=$((i+1))
  done
  $AR rcs "$VLIB/libdeflate.a" "${objs[@]}"
  echo "== libdeflate: ${#objs[@]} objs, $nf fails"
fi

# ── sqlite3 (bun's vendored amalgamation) ─────────────────────────────────
if [ ! -f "$VLIB/libsqlite3.a" ]; then
  rm -rf "$VLIB/sq"; mkdir -p "$VLIB/sq"
  if $CC --target=wasm32-wasip2 -O2 -DSQLITE_THREADSAFE=0 -DSQLITE_OMIT_LOAD_EXTENSION=1 -DSQLITE_ENABLE_COLUMN_METADATA=1 -DSQLITE_ENABLE_FTS3=1 -DSQLITE_ENABLE_FTS3_PARENTHESIS=1 -DSQLITE_ENABLE_FTS5=1 -DSQLITE_ENABLE_RTREE=1 -DSQLITE_ENABLE_SESSION=1 -DSQLITE_ENABLE_PREUPDATE_HOOK=1 -DSQLITE_ENABLE_DBSTAT_VTAB=1 -c "$B/src/jsc/bindings/sqlite/sqlite3.c" -o "$VLIB/sq/sqlite3.o" 2>"$VLIB/sq/err"; then
    $AR rcs "$VLIB/libsqlite3.a" "$VLIB/sq/sqlite3.o"; echo "== sqlite3: OK"
  else echo "sqlite3 FAIL:"; grep -m5 -oE 'error: .{0,60}' "$VLIB/sq/err"|sort -u; exit 1; fi
fi

# ── llhttp (bun's vendored copy) ──────────────────────────────────────────
if [ ! -f "$VLIB/libllhttp.a" ]; then
  rm -rf "$VLIB/lh"; mkdir -p "$VLIB/lh"; objs=(); i=0
  LD="$B/src/jsc/bindings/node/http/llhttp"
  for c in $(find "$LD" -name '*.c'); do
    o="$VLIB/lh/$i.o"
    if $CC --target=wasm32-wasip2 -O2 -I "$LD" -c "$c" -o "$o" 2>"$VLIB/lh/$i.err"; then objs+=("$o"); else echo "llhttp fail $c:"; grep -m2 -oE 'error: .{0,50}' "$VLIB/lh/$i.err"; fi
    i=$((i+1))
  done
  $AR rcs "$VLIB/libllhttp.a" "${objs[@]}"
  echo "== llhttp: ${#objs[@]} objs"
fi

# ── libspng ────────────────────────────────────────────────────────────────
if [ ! -f "$VLIB/libspng.a" ]; then
  rm -rf "$VLIB/sp"; mkdir -p "$VLIB/sp"
  if $CC --target=wasm32-wasip2 -O2 -DSPNG_STATIC=1 -I "$N/libspng/spng" -I "$N/zlib" -c "$N/libspng/spng/spng.c" -o "$VLIB/sp/spng.o" 2>"$VLIB/sp/err"; then
    $AR rcs "$VLIB/libspng.a" "$VLIB/sp/spng.o"; echo "== spng: OK"
  else echo "spng FAIL:"; grep -m5 -oE 'error: .{0,60}' "$VLIB/sp/err"|sort -u; exit 1; fi
fi

# ── libjpeg-turbo ─────────────────────────────────────────────────────────
# Hand-written jconfig/jconfigint/jversion (cmake normally instantiates
# them); sjlj because jpeg's error paths setjmp. The src/wrapper/*.c are
# the multi-precision (8/12/16-bit) TUs libjpeg-turbo 3.x ships.
J="$N/libjpeg-turbo"
if [ ! -f "$VLIB/libturbojpeg.a" ]; then
  if [ ! -f "$J/src/jconfig.h" ]; then
    cat > "$J/src/jconfig.h" <<'EOF'
#define JPEG_LIB_VERSION 80
#define LIBJPEG_TURBO_VERSION "3.1.4"
#define LIBJPEG_TURBO_VERSION_NUMBER 3001004
#define C_ARITH_CODING_SUPPORTED 1
#define D_ARITH_CODING_SUPPORTED 1
#define MEM_SRCDST_SUPPORTED 1
#ifndef BITS_IN_JSAMPLE
#define BITS_IN_JSAMPLE 8
#endif
EOF
  fi
  if [ ! -f "$J/src/jconfigint.h" ]; then
    cat > "$J/src/jconfigint.h" <<'EOF'
#define BUILD "wasi"
#define HIDDEN __attribute__((visibility("hidden")))
#define INLINE inline __attribute__((always_inline))
#define THREAD_LOCAL _Thread_local
#define PACKAGE_NAME "libjpeg-turbo"
#define VERSION "3.1.4"
#define SIZEOF_SIZE_T 4
#define HAVE_BUILTIN_CTZL
#if defined(__has_attribute)
#if __has_attribute(fallthrough)
#define FALLTHROUGH __attribute__((fallthrough));
#else
#define FALLTHROUGH
#endif
#else
#define FALLTHROUGH
#endif
EOF
  fi
  if [ ! -f "$J/src/jversion.h" ]; then
    cat > "$J/src/jversion.h" <<'EOF'
#define JVERSION "8b  16-May-2011"
#define JCOPYRIGHT "Copyright (C) 2009-2024 D. R. Commander\nCopyright (C) 1991-2020 Thomas G. Lane, Guido Vollbeding"
#define JCOPYRIGHT_SHORT "Copyright (C) 1991-2024 The libjpeg-turbo Project and many others"
EOF
  fi
  FLAGS=(--target=wasm32-wasip2 -O2 -mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false "-I$J/src" "-I$J")
  rm -rf "$VLIB/jt"; mkdir -p "$VLIB/jt"; objs=(); nf=0; i=0
  compile(){ local o="$VLIB/jt/$i.o"; if $CC "${FLAGS[@]}" -c "$1" -o "$o" 2>/dev/null; then objs+=("$o"); else nf=$((nf+1)); echo "FAIL $(basename $1)"; fi; i=$((i+1)); }
  for f in "$J/src/wrapper"/j*.c; do compile "$f"; done
  for f in "$J/src"/j*.c; do b=$(basename "$f" .c); ls "$J/src/wrapper/$b-8.c" >/dev/null 2>&1 && continue; case "$b" in *ext|*565|*mrgext|jstdhuff|jpegtran|jpegint|jcstest|jctest|*test) continue;; esac; compile "$f"; done
  for f in turbojpeg transupp jdatadst-tj jdatasrc-tj tjutil; do compile "$J/src/$f.c"; done
  $AR rcs "$VLIB/libturbojpeg.a" "${objs[@]}"
  echo "== jpeg: ${#objs[@]} objs, $nf fails"
fi

# ── hdrhistogram (three loose objects, not an archive) ────────────────────
if [ ! -f "$OBJ/hdr_hdr_histogram.o" ]; then
  for c in hdr_histogram hdr_encoding hdr_time; do
    $CC --target=wasm32-wasip2 -O2 -I "$N/hdrhistogram/include" -I "$N/hdrhistogram/src" -c "$N/hdrhistogram/src/$c.c" -o "$OBJ/hdr_$c.o"
  done
  echo "== hdrhistogram: 3 objs"
fi

echo "DONE vendored-extra"
