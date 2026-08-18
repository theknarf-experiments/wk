# A PDF reader as a wasm container: unmodified upstream MuPDF (the library —
# fitz + all its vendored thirdparty deps, base-14 fonts embedded) with our
# thin viewer main drawing pages on a wasi-gfx surface via the shared
# gfx-compat shim. Run `mise run build` first: it produces mupdf-view.wasm
# (mupdf sources fetched pinned; see build.sh).
#
# No PDF is baked in — the document comes from a bindmount/volume wire (that
# is the whole point): with no args the viewer scans / for the first *.pdf,
# which is exactly where a wired file lands.
#
# Controls: PgDn/PgUp or Left/Right turn pages, +/- zooms, scroll or Up/Down
# pans, Home/End jump to the first/last page.
FROM scratch
COPY mupdf-view.wasm /mupdf-view.wasm
ENTRYPOINT ["/mupdf-view.wasm"]
