# CPython 3.14 as a wasm container — the unmodified upstream interpreter built
# from source for wasm32-wasip2, so its socket module works over wk's network
# fabric (a real webserver, not just scripts). `mise run build` produces
# python.wasm (a component, no adapter needed — wasip2 links one directly) and
# the trimmed stdlib tree this COPYs.
#
# PYTHONHOME=/usr/local is the prefix under which CPython finds lib/python3.14/
# (the standard library). No shell, no site-packages: a self-contained runtime.
# HOME lets ~-expansion resolve internally. Wire the node to a Network + Port to
# serve HTTP on the fabric; wire a Volume/BindMount to run mounted .py files.
FROM scratch
COPY python.wasm /python.wasm
COPY lib/python3.14 /usr/local/lib/python3.14
ENV PYTHONHOME=/usr/local
ENV PYTHONDONTWRITEBYTECODE=1
ENV HOME=/root
ENTRYPOINT ["/python.wasm"]
