# wk-shell: real GNU bash that can actually run commands.
#
# bash is patched (patches/) so that, on a platform without fork(), an external
# command runs to completion through wk's `wk:exec` capability instead of
# fork+exec. Command names are ordinary symlinks onto the GNU coreutils
# multicall binary — the same install layout coreutils uses everywhere — which
# wk's filesystem now supports for real.
FROM scratch
COPY bash.wasm /bin/bash.wasm
COPY coreutils.wasm /bin/coreutils.wasm
COPY bin/ /bin/
ENV PATH=/bin
# A writable temp dir the way every base image ships one: tools that spill to a
# scratch file (tac, sort on large input, mktemp) fail without it. The node's
# root fs is writable at runtime, so an empty dir baked here is enough.
RUN ["/bin/bash.wasm", "-c", "mkdir -p /tmp /var/tmp"]
ENV TMPDIR=/tmp
ENTRYPOINT ["/bin/bash.wasm"]
