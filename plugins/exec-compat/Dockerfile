# The wk:exec demo: a program that runs other real programs. Ships the GNU
# coreutils multicall binary at a stable path so the demo can exec it.
FROM scratch
COPY execdemo.wasm /bin/execdemo.wasm
COPY coreutils.wasm /bin/coreutils.wasm
ENTRYPOINT ["/bin/execdemo.wasm"]
