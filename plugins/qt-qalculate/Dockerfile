# Qalculate! 5.12.0 (the Qt GUI) as a wasm container: a REAL Qt 6 Widgets
# application on top of a REAL libqalculate — GMP, MPFR and libxml2 all
# cross-compiled to wasm32-wasip2 — drawing on a wasi-gfx surface through the
# wk QPA platform plugin. Run `mise run build` first; it produces
# qalculate-qt.wasm (and needs plugins/qt built).
#
# FROM scratch, and it really is empty. Everything is inside the component:
# the icons as Qt resources, one TTF compiled in under :/fonts (Qt 6 ships no
# fonts and a node has no host font directory), and — the part that makes a
# calculator self-contained — libqalculate's entire unit, function, element and
# currency database, embedded as C string literals by
# --enable-compiled-definitions. Nothing is read from the vfs to compute.
#
# QT_QPA_PLATFORM is belt and braces: main.cpp already defaults to `wk` when
# the variable is unset. Setting it here makes the choice visible in
# `wk node inspect`, and makes QT_QPA_PLATFORM=offscreen a one-line override
# for separating "Qt is broken" from "the wk QPA is broken".
FROM scratch
COPY qalculate-qt.wasm /qalculate-qt.wasm
ENV QT_QPA_PLATFORM=wk
# libqalculate derives getLocalDir() from HOME and writes its preferences,
# expression history and any variables/functions you define there. Unset, the
# wasi build's getHomeDir() returns "/" and the app scatters dotfiles at the
# root of the node's vfs. Same reason plugins/vim and plugins/netsurf set it.
ENV HOME=/root
ENTRYPOINT ["/qalculate-qt.wasm"]
