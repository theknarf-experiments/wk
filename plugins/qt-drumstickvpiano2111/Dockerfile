# drumstick-vpiano 2.11.1 as a wasm container: a REAL Qt 6 MIDI application --
# an 88-key virtual piano drawn with QGraphicsView, playable with the mouse or
# the computer keyboard, that SENDS the notes you play and LIGHTS UP the notes
# it is sent. Run `mise run build` first; it produces drumstick-vpiano.wasm
# (and needs plugins/qt built).
#
# The component imports wk:midi/midi, so wk gives the node MIDI ports on the
# canvas with no configuration here: wire its output to a synth (the
# `fluidsynth` node) and its input to anything that produces MIDI (the `piano`
# node, a `sequencer`, or the `midiin` host node with a USB keyboard plugged
# in). See example.wk.
#
# FROM scratch, and it really is empty. Everything the app needs is inside the
# component: its UI, its key pixmaps and its icons as Qt resources, and --
# because Qt 6 ships no fonts and a node has no host font directory -- one TTF
# compiled in under :/fonts.
#
# QT_QPA_PLATFORM is belt and braces: vpianomain.cpp already defaults to `wk`
# when the variable is unset. Setting it here makes the choice visible in
# `wk node inspect`, and makes QT_QPA_PLATFORM=offscreen a one-line override
# for separating "Qt is broken" from "the wk QPA is broken".
FROM scratch
COPY drumstick-vpiano.wasm /drumstick-vpiano.wasm
ENV QT_QPA_PLATFORM=wk
# QSettings/QStandardPaths need somewhere to point: VPiano persists its window
# geometry, its note-naming preferences and its chosen connections. Same reason
# plugins/vim and plugins/netsurf set it.
ENV HOME=/root
# MIDI-thru, on. It is normally a checkbox in the Connections dialog persisted
# to QSettings, and a node started from a .wk file has neither somebody at the
# dialog nor a settings file to have been persisted into. With it on, a
# hardware keyboard wired INTO this node plays THROUGH it to whatever it is
# wired out to, while still lighting up its keys -- which is the demo. Set it
# to 0 to make the piano a pure controller again.
ENV WK_VPIANO_MIDI_THRU=1
ENTRYPOINT ["/drumstick-vpiano.wasm"]
