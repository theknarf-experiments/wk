# Quake as a wasm container: unmodified upstream quakegeneric (WinQuake's
# software renderer behind the QG_* platform contract) with the id 1.06
# shareware episode as the game data, its window a wasi-gfx surface via the
# shared gfx-compat shim. Run `mise run build` first: it produces quake.wasm
# and fetches pak0.pak (both pinned; see build.sh).
#
# Quake finds its data under <basedir>/id1/, so the pak lands at /id1/pak0.pak
# and -basedir points at /. Controls: arrows move/turn, Ctrl fires, Space
# jumps, Enter/Escape drive the menus, ` opens the console; the mouse looks
# (deltas synthesized from pointer positions). Silent by design: quakegeneric
# has no audio hook (it compiles snd_null.c).
FROM scratch
COPY quake.wasm /quake.wasm
COPY pak0.pak /id1/pak0.pak
ENTRYPOINT ["/quake.wasm", "-basedir", "/"]
