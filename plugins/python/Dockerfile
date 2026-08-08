# CPython 3.12 as a wasm container: the unmodified upstream wasm32-wasi
# interpreter (componentized by `mise run build`) plus its full standard library
# (python312.zip). Run `mise run build` first — it fetches the runtime and
# produces python.wasm and the usr/ tree this COPYs.
#
# PYTHONHOME=/usr/local is the prefix under which CPython finds lib/python312.zip
# (the zipped stdlib) and lib/python3.12/ (os.py — the landmark that anchors the
# home, plus lib-dynload). No shell, no site-packages: a self-contained runtime.
# HOME lets ~-expansion resolve internally.
FROM scratch
COPY python.wasm /python.wasm
COPY usr/local/lib /usr/local/lib
ENV PYTHONHOME=/usr/local
ENV PYTHONDONTWRITEBYTECODE=1
ENV HOME=/root
ENTRYPOINT ["/python.wasm"]
