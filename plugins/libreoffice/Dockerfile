# LibreOffice Impress as a wk node: the office and its whole install tree in one
# image, so the node type needs no wires to start.
#
# The tree is assembled by build-image.sh, which strips the 353 MB of `.a` files
# that were already linked into soffice.bin. Read that script for what "runtime"
# means here and why the destination is /instdir and not a nicer prefix --
# briefly: nothing at run time can discover where the install is (no dladdr, no
# realpath, a bare name in argv[0]), so cppuhelper, sal and fontconfig all have
# that exact path compiled in.
#
# soffice.bin is BOTH the entrypoint and a file inside the tree, because
# LibreOffice locates everything else relative to it -- which here means
# relative to the path it has agreed to be at, not to where it actually is.
FROM scratch
COPY image/instdir /instdir

# The profile: a component has no home directory and no user, and /instdir is a
# read-only image layer, so the default UserInstallation of $ORIGIN/.. cannot
# work. /profile lives in the node's own writable filesystem. It does not
# survive a restart; wire a Volume there when it should.
ENV HOME=/profile
ENV TMPDIR=/tmp

ENTRYPOINT ["/instdir/program/soffice.bin", "-env:UserInstallation=file:///profile"]
