#!/usr/bin/env bash
# Build UNMODIFIED upstream doomgeneric (https://github.com/ozkl/doomgeneric)
# into a wk graphics node: the real DOOM engine, its window a wasi-gfx surface
# through the shared ../gfx-compat shim. Only this script and doomgeneric_wk.c
# (the platform file every doomgeneric port provides) live in the repo — the
# game sources are fetched at build, pinned to a commit, and gitignored.
#
# Also fetches Freedoom Phase 1 (freedoom1.wad, pinned release) so the
# Dockerfile can ship a playable image out of the box.
#
# Requires wasi-sdk (set WASI_SDK; defaults to the mise-pinned install),
# wasm-tools, wit-bindgen, curl, unzip.
set -euo pipefail
cd "$(dirname "$0")"

# Default to the mise-pinned toolchain when present; ~/wasi-sdk may be stale.
MISE_SDK="$HOME/.local/share/mise/installs/github-web-assembly-wasi-sdk/wasi-sdk-34-rc.2"
WASI_SDK="${WASI_SDK:-$([ -d "$MISE_SDK" ] && echo "$MISE_SDK" || echo "$HOME/wasi-sdk")}"
CLANG="$WASI_SDK/bin/clang"

# Upstream doomgeneric, pinned. GPLv2 — fetched at build like bash's sources,
# not vendored into this repo.
DG_COMMIT=dcb7a8dbc7a16ce3dda29382ac9aae9d77d21284
DG_DIR="doomgeneric-$DG_COMMIT"
if [ ! -d "$DG_DIR" ]; then
    echo "fetching doomgeneric @ ${DG_COMMIT:0:12}..."
    curl -fsSL "https://github.com/ozkl/doomgeneric/archive/$DG_COMMIT.tar.gz" | tar xz
fi
SRC="$DG_DIR/doomgeneric"

# Freedoom Phase 1: a free IWAD, pinned release. Kept beside the wasm for the
# Dockerfile's COPY.
FREEDOOM_VER=0.13.0
if [ ! -f freedoom1.wad ]; then
    echo "fetching freedoom $FREEDOOM_VER..."
    curl -fsSL "https://github.com/freedoom/freedoom/releases/download/v$FREEDOOM_VER/freedoom-$FREEDOOM_VER.zip" -o freedoom.zip
    unzip -j -o freedoom.zip "freedoom-$FREEDOOM_VER/freedoom1.wad" >/dev/null
    rm -f freedoom.zip
fi

# Shared gfx shim + its wasi-gfx bindings (regenerated each build).
GFXCOMPAT="$(pwd)/../gfx-compat"
GFXGEN="$GFXCOMPAT/gen"
mkdir -p "$GFXGEN"
wit-bindgen c --world wkgfx "$GFXCOMPAT/wit" --out-dir "$GFXGEN"

# WASIp1→component adapter, pinned to our wasmtime (46); fetched and cached if a
# registry copy isn't present. Named `wasi_snapshot_preview1=` so wasm-tools
# binds it regardless of the file's stem.
WASMTIME_VER=46.0.1
ADAPTER="${WASI_ADAPTER:-$(find "$HOME/.cargo/registry/src" -name 'wasi_snapshot_preview1.command.wasm' 2>/dev/null | head -1)}"
if [ -z "$ADAPTER" ] || [ ! -f "$ADAPTER" ]; then
    ADAPTER="$GFXGEN/wasi_snapshot_preview1.command.wasm"
    if [ ! -f "$ADAPTER" ]; then
        echo "fetching WASI command adapter $WASMTIME_VER..."
        curl -fsSL "https://github.com/bytecodealliance/wasmtime/releases/download/v$WASMTIME_VER/wasi_snapshot_preview1.command.wasm" -o "$ADAPTER"
    fi
fi

# The game sources: doomgeneric's Makefile SRC_DOOM list minus every platform
# port (doomgeneric_*.c) — our doomgeneric_wk.c is the platform. No setjmp in
# doom (verified: grep -r setjmp finds nothing), so no sjlj flags.
GAME_SRCS=(
    dummy.c am_map.c doomdef.c doomstat.c dstrings.c d_event.c d_items.c
    d_iwad.c d_loop.c d_main.c d_mode.c d_net.c f_finale.c f_wipe.c g_game.c
    hu_lib.c hu_stuff.c info.c i_cdmus.c i_endoom.c i_joystick.c i_scale.c
    i_sound.c i_system.c i_timer.c memio.c m_argv.c m_bbox.c m_cheat.c
    m_config.c m_controls.c m_fixed.c m_menu.c m_misc.c m_random.c p_ceilng.c
    p_doors.c p_enemy.c p_floor.c p_inter.c p_lights.c p_map.c p_maputl.c
    p_mobj.c p_plats.c p_pspr.c p_saveg.c p_setup.c p_sight.c p_spec.c
    p_switch.c p_telept.c p_tick.c p_user.c r_bsp.c r_data.c r_draw.c
    r_main.c r_plane.c r_segs.c r_sky.c r_things.c sha1.c sounds.c statdump.c
    st_lib.c st_stuff.c s_sound.c tables.c v_video.c wi_stuff.c w_checksum.c
    w_file.c w_main.c w_wad.c z_zone.c w_file_stdc.c i_input.c i_video.c
    doomgeneric.c
)
SRCS=()
for s in "${GAME_SRCS[@]}"; do SRCS+=("$SRC/$s"); done

"$CLANG" --target=wasm32-wasip1 -O2 \
    -DNORMALUNIX -DLINUX -DSNDSERV -D_DEFAULT_SOURCE \
    -I"$SRC" -I"$GFXCOMPAT" -I"$GFXGEN" -Icompat \
    "${SRCS[@]}" doomgeneric_wk.c compat/system_stub.c \
    "$GFXCOMPAT/wkgfx.c" "$GFXGEN/wkgfx.c" "$GFXGEN/wkgfx_component_type.o" \
    -o doom.core.wasm

wasm-tools component new doom.core.wasm --adapt "wasi_snapshot_preview1=$ADAPTER" -o doom.wasm
rm -f doom.core.wasm
echo "built plugins/doom/doom.wasm"
