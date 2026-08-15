# The full Bun runtime (post-Rust-rewrite, engine=JavaScriptCore) as a wk
# container — real Bun on wasm32-wasip2, not the JSC-free transpiler slice that
# plugins/bun/Dockerfile ships.
#
# FROM wk-shell layers Bun over the bash base (plugins/bash), so the container
# has a real /bin/sh (bash), /bin/bash and the GNU coreutils applets on PATH.
# That is what makes node's child_process work: `exec`/`execSync`/`spawnSync`
# run `/bin/sh -c ...` through wk:exec (bun -> bash -> coreutils, nested), with
# real exit codes (see wasi:cli/exit.exit-with-code in the bash/coreutils
# builds). Build the base first:
#
#   wk images build plugins/bash/Dockerfile --tag wk-shell
#
# bun-run.wasm is the linked runtime (gitignored, ~180 MB). Produce it with the
# JSC prebuild + the Rust/link pipeline: ./build-jsc.sh (one-time), then the
# cargo build of bun_bin + link/link_all.sh. Wire the node to a Network + Port
# to serve HTTP over the fabric; give it a Volume/BindMount to run mounted .js.
FROM wk-shell
COPY bun-run.wasm /bin/bun.wasm
ENV PATH=/bin
ENV HOME=/root
ENTRYPOINT ["/bin/bun.wasm"]
