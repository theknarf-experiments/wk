# bun-transpile: the real Bun transpiler (post-Rust-rewrite, JSC-free slice)
# as a wk terminal command. Feed it TS/TSX paths; transpiled JS on stdout.
FROM scratch
COPY bun-transpile.wasm /bin/bun-transpile.wasm
ENV PATH=/bin
ENTRYPOINT ["/bin/bun-transpile.wasm"]
