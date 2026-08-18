# NetSurf as a wasm container: the real NetSurf 3.11 framebuffer browser
# (no JS; HTTP and HTTPS — TLS lives inside libcurl's wolfSSL backend)
# drawing on a wasi-gfx surface through libnsfb's wk backend, fetching over
# the fabric. Run `mise run build` first: it produces netsurf.wasm and
# stages res/ + cacert.pem.
#
# The resources land on the compiled-in search path (PREFIX=/usr at build →
# /usr/share/netsurf): Messages (gzipped, read via zlib), the core css,
# and the welcome/credits/licence pages the about: fetcher redirects to —
# so the default about:welcome home page paints without any network. HOME
# gives the ~/.netsurf Choices/Cookies paths somewhere to point.
FROM scratch
COPY netsurf.wasm /netsurf.wasm
COPY res /usr/share/netsurf
# Mozilla's CA bundle (the dated snapshot plugins/curl pins) at the path
# baked into libcurl via --with-ca-bundle: https:// verifies out of the box.
COPY cacert.pem /etc/ssl/cacert.pem
ENV HOME=/root
ENTRYPOINT ["/netsurf.wasm"]
