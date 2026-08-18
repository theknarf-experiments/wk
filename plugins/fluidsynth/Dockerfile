# A real SoundFont synthesizer as a wasm container: the FluidLite engine (the
# glib-free fork of FluidSynth's synthesis core) with TimGM6mb as the General
# MIDI soundfont. Run `mise run build` first: it produces fluidsynth.wasm and
# fetches soundfont.sf2 (both pinned; see build.sh).
#
# Wire MIDI in (a piano node, a hardware MidiIn node, a sequencer) and it
# renders through wk:webaudio to the speakers. Note events log to the node
# terminal.
FROM scratch
COPY fluidsynth.wasm /fluidsynth.wasm
COPY soundfont.sf2 /soundfont.sf2
ENTRYPOINT ["/fluidsynth.wasm", "/soundfont.sf2"]
