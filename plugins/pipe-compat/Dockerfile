# The pipe() self-test: a plain C program using pipe(2), read, write, fstat and
# dup, running on wk's pipe. The RUN step is the test — it prints its results
# during the build.
FROM scratch
COPY pipetest.wasm /bin/pipetest.wasm
RUN ["/bin/pipetest.wasm"]
ENTRYPOINT ["/bin/pipetest.wasm"]
