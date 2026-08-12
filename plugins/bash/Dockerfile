# wk-shell: real GNU bash that can actually run commands.
#
# bash is patched (patches/) so that, on a platform without fork(), an external
# command runs to completion through wk's `wk:exec` capability instead of
# fork+exec. It ships with the GNU coreutils multicall binary and a table
# mapping each applet to it — wk's filesystem has no symlinks, which is how
# such a binary normally provides its hundred names.
FROM scratch
COPY bash.wasm /bin/bash.wasm
COPY coreutils.wasm /bin/coreutils.wasm
COPY wk-multicall /etc/wk-multicall
ENV PATH=/bin
ENTRYPOINT ["/bin/bash.wasm"]
