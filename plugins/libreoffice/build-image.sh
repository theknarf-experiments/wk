#!/usr/bin/env bash
# Stage 4: the install tree as an OCI image, so `libreoffice` is a node type you
# can drop on a canvas rather than one you have to wire a 644 MB directory into.
#
# It exists because the alternative shipped once and was wrong: registering the
# node type without an image gives you, from the Cmd+K palette, a node that
# throws
#
#     Cannot open uno ini file:///instdir/program/unorc
#     soffice: unhandled UNO exception: null process service factory
#
# and reaches the host as a bare "thrown Wasm exception" in soffice_main. A node
# type that cannot run without a hand-wired BindMount is a trap, and the comment
# explaining the wire was in an example file nobody had opened.
#
# TRIMMING. build/instdir is 644 MB and 353 MB of that is `.a` files -- the
# static libraries that were already linked INTO soffice.bin. They are build
# residue, and nothing at run time opens one. What is left is ~290 MB:
#
#   program/soffice.bin   182 MB, the office
#   program/*rc, *.rdb    the bootstrap and the registries
#   program/services/     the office's own services.rdb, found by listing
#   program/resource/     translations
#   share/fonts           53 MB -- LibreOffice's own Liberation/DejaVu set;
#                         there is no system font path inside a component
#   share/config          21 MB -- soffice.cfg (menus, toolbars) and the icons
#   share/registry        the .xcd configuration layers
#   share/fontconfig      fonts.conf, which points at share/fonts
#
# Everything is copied to /instdir and NOT to a prefix of our choosing, because
# nothing at run time can discover where it is: wasm has no dladdr, wasi-libc's
# realpath is a stub, and a guest gets a bare name in argv[0]. cppuhelper, sal
# and fontconfig all have that path compiled in. Moving it is a source change.
#
# wk's Dockerfile subset has no RUN-on-the-host and no globs, so the trimmed
# tree is assembled here and COPYed wholesale.
set -uo pipefail
cd "$(dirname "$0")"
LO_STAGE=image
# shellcheck source=common.sh
. ./common.sh

INSTDIR="$LO_BUILD/instdir"
STAGE="$LO_ROOT/image/instdir"
TAG="libreoffice"

[ -f "$INSTDIR/program/soffice.bin" ] || lo_die \
    "$INSTDIR/program/soffice.bin missing — run ./build-lo.sh first"

echo "=== staging the runtime tree (excluding build residue)"
rm -rf "$LO_ROOT/image"
mkdir -p "$STAGE"

# rsync rather than cp + find: one pass, and the exclude list is the whole
# statement of what "runtime" means here.
rsync -a --delete \
    --exclude='*.a' \
    --exclude='*-gdb.py' \
    --exclude='sdk/' \
    --exclude='readmes/' \
    --exclude='share/gallery/' \
    "$INSTDIR/" "$STAGE/" || lo_die "rsync failed"

# The fontconfig cache directory has to EXIST for fontconfig to stop warning on
# every start, and an image layer cannot carry an empty directory, so it gets a
# file. (A component has nowhere to write a cache anyway; this only quiets it.)
mkdir -p "$STAGE/share/fontconfig/cache"
printf 'fontconfig writes nothing here: a wk node has no writable install tree.\n' \
    > "$STAGE/share/fontconfig/cache/README"

echo "    staged $(du -sh "$STAGE" | cut -f1) (from $(du -sh "$INSTDIR" | cut -f1))"

echo "=== wk images build"
WK="${WK_BIN:-$LO_ROOT/../../target/release/wk}"
[ -x "$WK" ] || lo_die "$WK not built — cargo build --release --bin wk"
"$WK" images build "$LO_ROOT/Dockerfile" --tag "$TAG" || lo_die "image build failed"

echo
echo "built image://$TAG — `wk run ./example/impress.wk`, or add a libreoffice"
echo "node from the palette; it needs no wires."
