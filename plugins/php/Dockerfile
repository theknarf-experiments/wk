# PHP 8.2 as a wasm container: the unmodified upstream interpreter built from
# source for wasm32-wasip2 (`mise run build`), so its built-in webserver
# (`php -S`) binds real sockets over wk's network fabric. A self-contained
# component — no separate stdlib tree. Bind-mount a document root at /app and
# wire the node to a Network + Port to serve it.
FROM scratch
COPY php.wasm /php.wasm
ENV HOME=/root
ENTRYPOINT ["/php.wasm"]
