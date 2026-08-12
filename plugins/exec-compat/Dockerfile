# The wk:exec demo: a program that runs other real programs. Ships the GNU
# coreutils multicall binary at a stable path so the demos can exec it.
#
# Two demos. execdemo runs programs to completion (`run`). pipedemo puts two of
# them on one pipe at the same time (`spawn`) and shows the difference: it is
# `seq 1 200000 | head -1`, which stops early instead of generating 1.3 MB.
# Run that one with:
#     wk node add exec-demo -- /bin/pipedemo.wasm
FROM scratch
COPY execdemo.wasm /bin/execdemo.wasm
COPY pipedemo.wasm /bin/pipedemo.wasm
COPY coreutils.wasm /bin/coreutils.wasm
RUN ["/bin/pipedemo.wasm"]
ENTRYPOINT ["/bin/execdemo.wasm"]
