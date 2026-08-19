#!/usr/bin/env bash
# Build UNMODIFIED upstream quakegeneric (https://github.com/erysdren/quakegeneric)
# into a wk graphics node: the real Quake engine (WinQuake's software
# renderer), its window a wasi-gfx surface through the shared ../gfx-compat
# shim. Only this script and quakegeneric_wk.c (the platform file every
# quakegeneric port provides, like doomgeneric's) live in the repo — the game
# sources are fetched at build, pinned to a commit, and gitignored. Sound is
# silent by design: quakegeneric compiles snd_null.c and exposes no audio
# hook.
#
# Also fetches the id Quake 1.06 shareware pak0.pak (from libsdl.org's
# shareware data tarball, checksum-pinned) so the Dockerfile can ship a
# playable image out of the box.
#
# Quake recovers from Host_Error/Host_EndGame via setjmp/longjmp (host.c's
# host_abortserver), which lowers to wasm exception handling — same recipe as
# plugins/lua: compile with -wasm-enable-sjlj + the new exnref EH (wasmtime
# only supports that model; the host enables Config::wasm_exceptions), link
# -lsetjmp, no -flto, and keep wasm-opt off clang's PATH (the one on PATH
# can't parse exnref).
#
# Requires wasi-sdk (set WASI_SDK; defaults to the mise-pinned install),
# wasm-tools, wit-bindgen, curl.
set -euo pipefail
cd "$(dirname "$0")"

# Default to the mise-pinned toolchain when present; ~/wasi-sdk may be stale.
MISE_SDK="$HOME/.local/share/mise/installs/github-web-assembly-wasi-sdk/wasi-sdk-34-rc.2"
WASI_SDK="${WASI_SDK:-$([ -d "$MISE_SDK" ] && echo "$MISE_SDK" || echo "$HOME/wasi-sdk")}"
CLANG="$WASI_SDK/bin/clang"

# wasi-sdk's clang runs wasm-opt as an optional post-link step, but the
# wasm-opt on PATH can't parse the new exnref EH we emit ("bad node code").
# Run clang with a PATH that omits it so the pass is simply skipped;
# wasm-tools still runs under the normal PATH below.
CLANG_PATH="$WASI_SDK/bin:/usr/bin:/bin"

# Upstream quakegeneric, pinned. GPLv2 — fetched at build like doomgeneric,
# not vendored into this repo. (erysdren/quakegeneric is the canonical repo:
# WinQuake reduced to the QG_* platform contract.)
QG_COMMIT=13052102577c629650cf07a46151a4b6e1b19c3c
QG_DIR="quakegeneric-$QG_COMMIT"
if [ ! -d "$QG_DIR" ]; then
    echo "fetching quakegeneric @ ${QG_COMMIT:0:12}..."
    curl -fsSL "https://github.com/erysdren/quakegeneric/archive/$QG_COMMIT.tar.gz" | tar xz
fi
SRC="$QG_DIR/source"

# id Quake 1.06 shareware data (episode 1), redistributable as-is. libsdl.org
# has hosted this exact tarball since the Linux SDL Quake port; the pak inside
# is the canonical 1.06 shareware pak0.pak. Kept beside the wasm for the
# Dockerfile's COPY.
QSW_URL="https://www.libsdl.org/projects/quake/data/quakesw-1.0.6.tar.gz"
QSW_SHA256=d173e9f828b932a8160d4c65927281d0c28131cd922f0bf0d69e92a35185b499
if [ ! -f pak0.pak ]; then
    echo "fetching Quake 1.06 shareware data..."
    curl -fsSL "$QSW_URL" -o quakesw.tar.gz
    # coreutils sha256sum on Linux, perl shasum on macOS — whichever exists
    if command -v sha256sum >/dev/null 2>&1; then
        echo "$QSW_SHA256  quakesw.tar.gz" | sha256sum -c -
    else
        echo "$QSW_SHA256  quakesw.tar.gz" | shasum -a 256 -c -
    fi
    tar xzf quakesw.tar.gz id1/pak0.pak
    mv id1/pak0.pak pak0.pak
    rmdir id1
    rm -f quakesw.tar.gz
fi

# Shared gfx shim + its wasi-gfx bindings (regenerated each build).
GFXCOMPAT="$(pwd)/../gfx-compat"
GFXGEN="$GFXCOMPAT/gen"
mkdir -p "$GFXGEN"
wit-bindgen c --world wkgfx "$GFXCOMPAT/wit" --out-dir "$GFXGEN"

# WASIp1→component adapter, pinned to our wasmtime (46); fetched and cached if
# a registry copy isn't present. Named `wasi_snapshot_preview1=` so wasm-tools
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

# The game sources: upstream CMakeLists.txt's QUAKEGENERIC_SOURCES list — the
# whole engine plus its null drivers (vid_null.c routes video through the QG_*
# hooks, in_null.c input, snd_null.c no sound, sys_null.c stdio file I/O) —
# minus every platform port (quakegeneric_*.c): our quakegeneric_wk.c is the
# platform.
GAME_SRCS=(
    cd_null.c chase.c cl_demo.c cl_input.c cl_main.c cl_parse.c cl_tent.c
    cmd.c common.c console.c crc.c cvar.c d_edge.c d_fill.c d_init.c
    d_modech.c d_part.c d_polyse.c d_scan.c d_sky.c d_sprite.c d_surf.c
    d_vars.c d_zpoint.c draw.c host_cmd.c host.c in_null.c keys.c mathlib.c
    menu.c model.c net_loop.c net_main.c net_none.c net_vcr.c nonintel.c
    pr_cmds.c pr_edict.c pr_exec.c r_aclip.c r_alias.c r_bsp.c r_draw.c
    r_edge.c r_efrag.c r_light.c r_main.c r_misc.c r_part.c r_sky.c
    r_sprite.c r_surf.c r_vars.c sbar.c screen.c snd_null.c sv_main.c
    sv_move.c sv_phys.c sv_user.c sys_null.c vid_null.c view.c wad.c world.c
    zone.c quakegeneric.c
)
SRCS=()
for s in "${GAME_SRCS[@]}"; do SRCS+=("$SRC/$s"); done

# Stack: COM_LoadPackFile alone puts a 128 KiB dpackfile_t[MAX_FILES_IN_PACK]
# on the stack — far over wasm-ld's 64 KiB default, so give the guest a real
# native-sized stack.
env PATH="$CLANG_PATH" "$CLANG" --target=wasm32-wasip1 -O2 -std=gnu99 \
    -Wl,-z,stack-size=4194304 \
    -mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false \
    -Wno-deprecated-non-prototype -Wno-implicit-function-declaration \
    -I"$SRC" -I"$GFXCOMPAT" -I"$GFXGEN" \
    "${SRCS[@]}" quakegeneric_wk.c \
    "$GFXCOMPAT/wkgfx.c" "$GFXGEN/wkgfx.c" "$GFXGEN/wkgfx_component_type.o" \
    -lsetjmp \
    -o quake.core.wasm

wasm-tools component new quake.core.wasm --adapt "wasi_snapshot_preview1=$ADAPTER" -o quake.wasm
rm -f quake.core.wasm
echo "built plugins/quake/quake.wasm"
