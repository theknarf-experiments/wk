# Slate as a wasm container: the real Qt 6 Quick pixel-art editor, rendered by
# Qt's software scenegraph onto a wasi-gfx surface.
#
# Build it with no Docker involved:
#   mise run build                       # produces slate.wasm
#   wk images build plugins/qt-slate.../Dockerfile --tag slate
#
# The image is deliberately almost empty. Everything Slate needs at runtime —
# its 105 QML files, its icons, its Roboto/FontAwesome fonts and the
# qmlcachegen bytecode for all of it — is compiled INTO the component by
# qt_add_qml_module and AUTORCC on a static Qt. Only the user's documents come
# in over a wire.
#
# The three QT_* variables are the same defaults node/wkslate.cpp compiles in;
# they are repeated here because a Dockerfile ENV is the layer a user can see
# and change, and because setting QT_QPA_PLATFORM=offscreen here is the fastest
# way to ask "is this Qt broken, or is the wk QPA broken?".
#
#   QT_QPA_PLATFORM=wk        the wk platform plugin (plugins/qt/qpa). The
#                             cross qtbase defaults to `offscreen`, which runs
#                             perfectly and renders to nowhere.
#   QT_QUICK_BACKEND=software Qt Quick's default scenegraph wants OpenGL,
#                             Vulkan or Metal. A wk node has an RGBA8
#                             framebuffer. The software adaptation renders the
#                             scene with QPainter, and renders
#                             QQuickPaintedItem — which is what Slate's canvas,
#                             rulers, guides and cursors are.
#   QSG_RENDER_LOOP=basic     no threads on wasip2, so no threaded render loop.
#
# HOME gives QSettings (Slate's ApplicationSettings) and QStandardPaths
# somewhere to point; without it Slate warns on every setting it writes.
#
# /work is where a BindMount of a host directory belongs, so Slate's file
# dialogs have somewhere to open and save .slp projects and .png exports:
#
#   node "slate" "01K...01" { pos 80.0 80.0  size 1200.0 800.0 }
#   bindmount "example/pixelart" "01K...02" { pos 80.0 920.0 size 220.0 120.0 }
#   connection "01K...02" "01K...01" "/work"
FROM scratch
COPY slate.wasm /slate.wasm
ENV QT_QPA_PLATFORM=wk
ENV QT_QUICK_BACKEND=software
ENV QSG_RENDER_LOOP=basic
ENV HOME=/root
ENTRYPOINT ["/slate.wasm"]
