# NetSurf as a wasm container: the real NetSurf 3.11 framebuffer browser
# (no JS, HTTP-only — the curl build has no TLS backend) drawing on a
# wasi-gfx surface through libnsfb's wk backend, fetching over the fabric.
# Run `mise run build` first: it produces netsurf.wasm and stages res/.
#
# The resources land on the compiled-in search path (PREFIX=/usr at build →
# /usr/share/netsurf): Messages (gzipped, read via zlib), the core css,
# and the welcome/credits/licence pages the about: fetcher redirects to —
# so the default about:welcome home page paints without any network. HOME
# gives the ~/.netsurf Choices/Cookies paths somewhere to point.
FROM scratch
COPY netsurf.wasm /netsurf.wasm
COPY res /usr/share/netsurf
ENV HOME=/root
ENTRYPOINT ["/netsurf.wasm"]
