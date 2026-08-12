# wk-base: the shell base image. One busybox-style binary (shell + coreutils +
# curl + a `wk` API client as in-process builtins — WASI has no fork/exec, so
# every "external command" is a builtin) at a stable path other images can
# build on:
#
#   wk images build plugins/wsh/Dockerfile --tag wk-base
#
#   # a child image, with shell RUN steps:
#   FROM wk-base
#   RUN ["/bin/wsh.wasm", "-c", "mkdir -p /etc && echo hi > /etc/motd"]
#
# Build wsh.wasm first (`mise run build` here) — the COPY takes the artifact.
FROM scratch
COPY wsh.wasm /bin/wsh.wasm
ENV PATH=/bin
ENTRYPOINT ["/bin/wsh.wasm"]
