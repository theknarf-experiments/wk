#!/usr/bin/env bash
# Build the host tools macOS does not ship, into .hosttools, and expose them
# through .toolbin so the rest of the port finds them like any other tool.
#
# LibreOffice's configure has two HARD errors this machine trips on:
#
#   GNU Make >= 4.2   configure.ac:6907. macOS ships /usr/bin/make 3.81, and
#                     there is no gmake.
#   gperf >= 3.1      configure.ac:8201. macOS ships /usr/bin/gperf 3.0.3
#                     (Xcode's), which is 2007 vintage.
#
# Neither is installable through mise: there is no aqua or ubi build of either
# (both are GNU/savannah releases, not GitHub ones), and mise's asdf make plugin
# fails GPG verification because it wants GNU's signing key in your keyring.
# ccache IS available through mise and is pinned in mise.toml's [tools] instead
# of built here — see the comment there.
#
# So: pinned source tarballs, built once, ~2 minutes for both. Cheap enough that
# nothing about this needs caching beyond "is it already there".
#
# On the checksums: these are the SHA-256 of what ftp.gnu.org served us, not a
# figure copied from an independent record. That means they pin *reproducibility*
# — a silently changed tarball is caught — but they are not by themselves proof
# of provenance, since we cannot check GNU's signatures without their key. Said
# plainly because a checksum in a build script usually implies more than this
# one can deliver.
set -euo pipefail
cd "$(dirname "$0")"
. ./common.sh

MAKE_VER=4.4.1
MAKE_SHA=dd16fb1d67bfab79a72f5e8390735c49e3e8e70b4945a15ab1f81ddb78658fb3
GPERF_VER=3.1
GPERF_SHA=588546b945bba4b70b6a3a616e80b4ab466e3f33024a352fc2198112cdbb3ae2

PREFIX="$LO_ROOT/.hosttools"
mkdir -p "$PREFIX" "$LO_TARBALLS"

# Fetch and verify one tarball. Re-verifies an already-present file rather than
# trusting its name: a truncated download from an earlier interrupted run is
# otherwise indistinguishable from a good one.
fetch() {
    local url="$1" file="$2" want="$3" got
    if [ -f "$LO_TARBALLS/$file" ]; then
        got="$(shasum -a 256 "$LO_TARBALLS/$file" | cut -d' ' -f1)"
        [ "$got" = "$want" ] && return 0
        echo "  $file: checksum mismatch, refetching" >&2
        rm -f "$LO_TARBALLS/$file"
    fi
    echo "  fetching $file"
    curl -sSLo "$LO_TARBALLS/$file" "$url"
    got="$(shasum -a 256 "$LO_TARBALLS/$file" | cut -d' ' -f1)"
    if [ "$got" != "$want" ]; then
        echo "$file: sha256 $got, expected $want" >&2
        exit 1
    fi
}

# GNU Make. Built with its own build.sh bootstrap? No — configure works fine and
# we are not in the chicken-and-egg case: /usr/bin/make 3.81 is too old for
# LibreOffice but perfectly able to build make 4.4.1.
build_make() {
    if [ -x "$PREFIX/bin/make" ] && "$PREFIX/bin/make" --version | head -1 | grep -q "$MAKE_VER"; then
        echo "gmake $MAKE_VER already built"
        return 0
    fi
    echo "building GNU Make $MAKE_VER"
    fetch "https://ftp.gnu.org/gnu/make/make-$MAKE_VER.tar.gz" "make-$MAKE_VER.tar.gz" "$MAKE_SHA"
    local d="$LO_ROOT/.hosttools-build/make-$MAKE_VER"
    rm -rf "$d"
    mkdir -p "$(dirname "$d")"
    tar -C "$(dirname "$d")" -xzf "$LO_TARBALLS/make-$MAKE_VER.tar.gz"
    (
        cd "$d"
        ./configure --prefix="$PREFIX" --disable-dependency-tracking >/dev/null
        /usr/bin/make -j"$LO_JOBS" >/dev/null
        /usr/bin/make install >/dev/null
    )
}

# gperf. LibreOffice uses it to generate perfect hash functions for, among
# others, the HTML/CSS keyword tables.
build_gperf() {
    if [ -x "$PREFIX/bin/gperf" ] && "$PREFIX/bin/gperf" --version | head -1 | grep -q "$GPERF_VER"; then
        echo "gperf $GPERF_VER already built"
        return 0
    fi
    echo "building gperf $GPERF_VER"
    fetch "https://ftp.gnu.org/gnu/gperf/gperf-$GPERF_VER.tar.gz" "gperf-$GPERF_VER.tar.gz" "$GPERF_SHA"
    local d="$LO_ROOT/.hosttools-build/gperf-$GPERF_VER"
    rm -rf "$d"
    mkdir -p "$(dirname "$d")"
    tar -C "$(dirname "$d")" -xzf "$LO_TARBALLS/gperf-$GPERF_VER.tar.gz"
    (
        cd "$d"
        ./configure --prefix="$PREFIX" >/dev/null
        /usr/bin/make -j"$LO_JOBS" >/dev/null
        /usr/bin/make install >/dev/null
    )
}

build_make
build_gperf

# `gmake`, not `make`: configure.ac:635 searches "$MAKE" "$GNUMAKE" make gmake
# gnumake in that order, and lo_find_gmake looks for the same. Linking it under
# both names means neither search can pick up /usr/bin/make 3.81 first.
mkdir -p "$LO_TOOLBIN"
ln -sf "$PREFIX/bin/make" "$LO_TOOLBIN/gmake"
ln -sf "$PREFIX/bin/make" "$LO_TOOLBIN/make"
ln -sf "$PREFIX/bin/gperf" "$LO_TOOLBIN/gperf"

echo
echo "host tools ready in $PREFIX/bin:"
echo "  $("$PREFIX/bin/make" --version | head -1)"
echo "  $("$PREFIX/bin/gperf" --version | head -1)"
echo "ccache comes from mise ([tools] in mise.toml), not from here."
