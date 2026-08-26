# torrent-file-editor 1.0.3 as a wasm container: a REAL Qt 6 Widgets
# application — three .ui files, QTreeView/QTableView with custom models and
# item delegates, a QProxyStyle, modal dialogs — drawing on a wasi-gfx surface
# through the wk QPA platform plugin. Run `mise run build` first; it produces
# torrent-file-editor.wasm (and needs plugins/qt built).
#
# FROM scratch, and it really is empty. Everything the app needs is inside the
# component: its icons and its UI as Qt resources, and — because Qt 6 ships no
# fonts and a node has no host font directory — one TTF compiled in under
# :/fonts. A .torrent arrives over a wire, at the path the workspace's `args`
# line names.
#
# QT_QPA_PLATFORM is belt and braces: main.cpp already defaults to `wk` when
# the variable is unset. Setting it here makes the choice visible in
# `wk node inspect`, and makes QT_QPA_PLATFORM=offscreen a one-line override
# for separating "Qt is broken" from "the wk QPA is broken".
FROM scratch
COPY torrent-file-editor.wasm /torrent-file-editor.wasm
ENV QT_QPA_PLATFORM=wk
# QStandardPaths/QSettings need somewhere to point (MainWindow persists its
# last-used folder and window state). Same reason plugins/vim and
# plugins/netsurf set it.
ENV HOME=/root
ENTRYPOINT ["/torrent-file-editor.wasm"]
