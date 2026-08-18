# DOOM as a wasm container: unmodified upstream doomgeneric (the engine) with
# Freedoom Phase 1 as the IWAD, its window a wasi-gfx surface via the shared
# gfx-compat shim. Run `mise run build` first: it produces doom.wasm and
# fetches freedoom1.wad (both pinned; see build.sh).
#
# Controls: arrows move/turn, Ctrl fires, Space uses, Enter/Escape drive the
# menus. Sound effects play through wk:webaudio (i_wksound.c + the shared
# audio-compat shim); music is off (no OPL emulation).
FROM scratch
COPY doom.wasm /doom.wasm
COPY freedoom1.wad /freedoom1.wad
ENTRYPOINT ["/doom.wasm", "-iwad", "/freedoom1.wad"]
