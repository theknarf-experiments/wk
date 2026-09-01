#!/usr/bin/env bash
# Shared preamble for plugins/libreoffice's build stages. SOURCED, never run.
#
# WHY THIS FILE EXISTS AT ALL — plugins/qt, plugins/netsurf and plugins/mupdf
# each repeat their seven-line WASI_SDK guard verbatim in every script, and
# that duplication is the house style. This port deviates because its shared
# surface is much larger than a guard: the toolchain triple, the exception
# flag set, the PATH discipline (which differs BETWEEN stages here — the native
# bootstrap must NOT see wasi-sdk), the out-of-tree build directory, and the
# rule about which flags may and may not be passed through the environment.
# Four scripts drifting apart on any one of those is a wasted overnight build.
# The guard itself is still copied verbatim from the other plugins so a
# `grep -r EXPECT plugins/*/build-*.sh` still lines them all up.
#
# Everything here is idempotent and side-effect-free except for creating the
# tarballs/, logs/ and .toolbin/ directories.

# --- toolchain guard (same seven lines as plugins/qt, netsurf, mupdf) --------
MISE_SDK="$HOME/.local/share/mise/installs/github-web-assembly-wasi-sdk/wasi-sdk-34-rc.2"
WASI_SDK="${WASI_SDK:-$([ -d "$MISE_SDK" ] && echo "$MISE_SDK" || echo "$HOME/wasi-sdk")}"
EXPECT="wasi-sdk-34-rc.2"
case "$WASI_SDK" in
    *"$EXPECT"*) ;;
    *)
        echo "libreoffice/${LO_STAGE:-common}: expected $EXPECT (set WASI_SDK), got: $WASI_SDK" >&2
        exit 1
        ;;
esac

# --- layout ------------------------------------------------------------------
# LO_ROOT is plugins/libreoffice. Every path below hangs off it, and every one
# except patches/ is gitignored.
LO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LO_SRC="$LO_ROOT/src"              # the pinned upstream checkout; never edited in place
LO_BUILD="$LO_ROOT/build"          # out-of-tree build dir (workdir/, instdir/, config_host.mk)
LO_TARBALLS="$LO_ROOT/tarballs"    # --with-external-tar: the ~149 external tarballs
LO_PATCHES="$LO_ROOT/patches"      # the only tracked derived-from-upstream thing
LO_LOGDIR="${LOGDIR:-$LO_ROOT/logs}"
LO_TOOLBIN="$LO_ROOT/.toolbin"
LO_HOSTTOOLS="$LO_ROOT/.hosttools"  # gmake/gperf this port builds; see build-deps.sh
LO_TAG="libreoffice-26.2.6.2"      # what src/ must be at; see PORTING.md for why this tag

# The target triple. Deliberately NOT wasm32-local-emscripten: this port adds a
# `wasi*)` host_os arm beside upstream's `emscripten)` one rather than pretending
# to be Emscripten. See PORTING.md, "The strategy".
#
# It must stay `wasip2` and not bare `wasi`: config.sub normalises both, but the
# component is linked wasip2-direct by wasm-component-ld (no preview1 adapter),
# same as plugins/netsurf and plugins/bun.
LO_HOST_TRIPLE="wasm32-unknown-wasip2"

mkdir -p "$LO_TARBALLS" "$LO_LOGDIR" "$LO_TOOLBIN"

LO_JOBS="${JOBS:-$(sysctl -n hw.ncpu 2>/dev/null || nproc)}"

# --- host tools --------------------------------------------------------------
# The plugins/netsurf .toolbin shape rather than plugins/qt's fixed BUILD_PATH,
# because LibreOffice needs a lot more from the host than cmake+pkg-config and
# they are scattered between /usr/bin and /opt/homebrew/bin. Symlinking the ones
# we resolved into one directory lets BUILD_PATH stay narrow — notably free of
# any wasm-opt (clang runs it as an optional post-link pass and the one on this
# machine, ~/.cargo/bin/wasm-opt, cannot parse exnref; it would silently corrupt
# the output). Same trap plugins/qt and plugins/mupdf document.
#
# gmake is listed FIRST and separately because it is the one whose absence stops
# everything: configure.ac:6907 requires GNU Make >= 4.2 and this machine's
# /usr/bin/make is 3.81.
#
# The autotools entries are not just `autoconf` and `aclocal`: those two are
# Perl/shell front ends that re-exec autom4te, autoheader and friends, and
# Homebrew's aclocal additionally shells out to its versioned twin. Omitting one
# surfaces as autogen.sh's bare "Failed to run aclocal at ... line 210", which
# names neither the tool nor the PATH.
LO_HOST_TOOLS="gmake make gperf ccache flex bison m4 gm4 perl python3 pkg-config
               autoconf autoheader autom4te autoreconf autoupdate ifnames
               aclocal aclocal-1.18 automake automake-1.18 libtool libtoolize glibtoolize
               zip unzip xsltproc xmllint tar curl git
               meson ninja cmake sed awk grep patch install"
lo_link_toolbin() {
    local t p
    for t in $LO_HOST_TOOLS; do
        p="$(command -v "$t" 2>/dev/null || true)"
        [ -n "$p" ] && ln -sf "$p" "$LO_TOOLBIN/$t"
    done
    return 0
}

# Two PATHs, and the difference is load-bearing.
#
# LO_BUILD_PATH  the cross build. wasi-sdk first so `clang` is the cross clang.
# LO_HOST_PATH   the NATIVE bootstrap (`make cross-toolset`), which builds ~26
#                code generators plus a native ICU that must run on arm64
#                macOS. wasi-sdk is deliberately ABSENT: `which -a clang` on
#                this machine puts wasi-sdk's wasm32-wasip1 clang ahead of
#                Apple's, and configure.ac:6206 unsets CC before running the
#                BUILD-side sub-configure, which then autodetects from PATH.
#                Left alone, the native bootstrap silently picks a wasm cross
#                compiler. (CC_FOR_BUILD/CXX_FOR_BUILD below is the belt to
#                this brace; configure.ac:6209 consumes them.)
LO_BUILD_PATH="$WASI_SDK/bin:$LO_TOOLBIN:/usr/bin:/bin"
LO_HOST_PATH="$LO_TOOLBIN:/usr/bin:/bin"

# --- the target toolchain ----------------------------------------------------
# The exception/sjlj flag set. Identical to plugins/qt/wasip2.cmake:175 and
# proven end to end on this machine (throw/catch + setjmp/longjmp, linked with
# -lunwind -lsetjmp, run under wasmtime). Non-negotiable for LibreOffice, which
# throws everywhere and whose bundled libpng/libjpeg are setjmp-based.
#
#   -fwasm-exceptions          exnref EH, and — separately important — it is
#                              what selects wasi-sdk 34's eh/ variant of
#                              libc++/libc++abi. Without it you silently get
#                              noeh/ and -lunwind does not resolve.
#   -wasm-enable-sjlj          setjmp/longjmp lowered to the same mechanism.
#   -wasm-use-legacy-eh=false  wk's wasmtime is configured with
#                              config.wasm_exceptions(true) and REJECTS the
#                              legacy encoding at instantiate time, naming no
#                              file. One TU built without this poisons the
#                              whole component.
LO_EH_FLAGS="-fwasm-exceptions -mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false"

# wasi-libc emulation. LibreOffice's sal calls signal(), getpid() and
# clock-family functions from code with no OS conditional; these turn the
# missing symbols into stubs. The MATCHING -lwasi-emulated-* link halves are
# NOT set here — see the LDFLAGS note below.
LO_EMULATION_DEFS="-D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_MMAN -D_WASI_EMULATED_PROCESS_CLOCKS -D_WASI_EMULATED_GETPID"

# THE COMPILE FLAGS GO IN CC/CXX, NOT IN CFLAGS/CXXFLAGS. Two independent
# reasons, both verified by reading:
#
#  1. CFLAGS is a TRAP in gbuild. solenv/gbuild/LinkTarget.mk:66 is
#     `gb_LinkTarget__get_cflags=$(if $(CFLAGS),$(CFLAGS),...debugflags...)` —
#     a non-empty CFLAGS REPLACES LibreOffice's own -g/-O handling wholesale
#     rather than adding to it. Same for CXXFLAGS (:68) and LDFLAGS (:72).
#     config_host.mk.in:69 only exports CFLAGS at all when it was set.
#
#  2. It is the only mechanism that reaches the EXTERNALS. config_host.mk.in:64
#     is `export CC=@CC@`, exported into the whole make environment, so every
#     external's own ./configure sees it. Upstream's Emscripten build gets this
#     wrong in the other direction: `grep -rn gb_EMSCRIPTEN_EXCEPT` matches only
#     the platform .mk — the EH flag reaches LO's gbuild targets and the link
#     line and NOTHING under external/. Survivable for emcc; for us every C++
#     external (icu, boost, harfbuzz, liborcus) would compile against noeh
#     libc++ and, the moment one of them emits a `try`, produce a component
#     wasmtime refuses to instantiate.
#
# The LINK half (-lunwind -lsetjmp, -lwasi-emulated-*, -Wl,-z,stack-size=...,
# and the wkgfx component-type object) belongs in
# solenv/gbuild/platform/WASI_INTEL_GCC.mk's gb_LinkTarget_LDFLAGS — i.e. in
# patches/, exactly where EMSCRIPTEN_INTEL_GCC.mk:58 puts its own. Putting -l
# flags in CC would pass them on compile steps too.
LO_CC="$WASI_SDK/bin/clang --target=$LO_HOST_TRIPLE $LO_EH_FLAGS $LO_EMULATION_DEFS"
LO_CXX="$WASI_SDK/bin/clang++ --target=$LO_HOST_TRIPLE $LO_EH_FLAGS $LO_EMULATION_DEFS"

# Binutils. Pass them explicitly or configure.ac:7092's AC_CHECK_TOOLS falls
# back to Apple's, which cannot read wasm archives. There is deliberately no
# READELF: wasi-sdk 34 ships no llvm-readelf and macOS has no readelf either.
# Harmless — its only gbuild consumer, unxgcc.mk:179, is the SONAME export step
# inside the Library branch, which DISABLE_DYNLOADING never takes (Libraries go
# through gb_LinkTarget__command_staticlink instead).
LO_AR="$WASI_SDK/bin/llvm-ar"
LO_NM="$WASI_SDK/bin/llvm-nm"
LO_RANLIB="$WASI_SDK/bin/llvm-ranlib"
LO_STRIP="$WASI_SDK/bin/llvm-strip"
LO_OBJDUMP="$WASI_SDK/bin/llvm-objdump"

# The native compiler for the BUILD side. Absolute paths, never `clang`.
LO_CC_FOR_BUILD="/usr/bin/clang"
LO_CXX_FOR_BUILD="/usr/bin/clang++"

# --- helpers -----------------------------------------------------------------
lo_die() { echo "libreoffice/${LO_STAGE:-?}: $*" >&2; exit 1; }

# GNU Make >= 4.2 (configure.ac:6907, a hard AC_MSG_ERROR). The search loop at
# configure.ac:635 tries "$MAKE" "$GNUMAKE" make gmake gnumake in that order, so
# exporting GNUMAKE is enough — we do not have to be first on PATH.
lo_find_gnumake() {
    local c v
    # .hosttools first: `mise run deps` builds GNU Make there precisely because
    # every make on this machine's PATH is too old, and searching PATH first
    # would find 3.81 and reject it before reaching ours.
    for c in "${GNUMAKE:-}" "$LO_HOSTTOOLS/bin/make" gmake make; do
        [ -n "$c" ] || continue
        command -v "$c" >/dev/null 2>&1 || continue
        v=$("$c" --version 2>/dev/null | head -1 | grep GNU | sed -e 's@^[^0-9]*@@' -e 's@ .*@@')
        [ -n "$v" ] || continue
        # 4.2 → 40200, matching configure.ac's own arithmetic
        if [ "$(echo "$v" | awk -F. '{ print $1*10000+$2*100+$3 }')" -ge 40200 ]; then
            command -v "$c"
            return 0
        fi
    done
    return 1
}

# gperf >= 3.1 (configure.ac:8201, also a hard error). /usr/bin/gperf on macOS
# is Xcode's 3.0.3.
lo_find_gperf() {
    local c v
    for c in "${GPERF:-}" "$LO_HOSTTOOLS/bin/gperf" /opt/homebrew/bin/gperf gperf; do
        [ -n "$c" ] || continue
        command -v "$c" >/dev/null 2>&1 || continue
        v=$("$c" --version 2>/dev/null | head -1)
        v=${v#GNU gperf }
        if [ "$(printf %s "$v" | awk -F. '{ print $1*100+($2<100?$2:99) }')" -ge 301 ]; then
            command -v "$c"
            return 0
        fi
    done
    return 1
}

# Refuse to go on unless the structural patches are in place. Their absence
# produces failures that read like something else entirely — the configure.ac
# one as "wasip2 operating system is not suitable to build LibreOffice for!"
# (configure.ac:1266, the `*)` fallthrough), and the missing platform makefile
# as a gbuild include error naming a file nobody has heard of. Same idea as
# plugins/qt/build-qtbase.sh's preflight, and for the same reason.
lo_require_patches() {
    local missing=0
    if ! grep -q 'wasi\*)' "$LO_SRC/configure.ac" 2>/dev/null; then
        cat >&2 <<EOF

libreoffice: src/configure.ac has no \`wasi*)\` host_os arm.

  configure.ac:1247 has exactly one wasm arm, \`emscripten)\`, and the next case
  at :1265 is a bare AC_MSG_ERROR. \`--host=$LO_HOST_TRIPLE\` gives
  \$host_os = "wasip2" and falls straight through it.

  Needed: patches/core-NNNN-configure-wasi-host-arm.patch adding a \`wasi*)\`
  arm beside it (setting _os=WASI, usable_dlapi=no, using_x11=yes,
  using_freetype_fontconfig=yes, using_headless_plugin=no,
  enable_customtarget_components=yes, enable_wasm_strip=yes, with_theme=colibre,
  test_system_freetype=no, with_system_zlib=no), plus the OS/CPUNAME arm at
  :5801 and WASI on the ENABLE_WASM_STRIP_* gate at :4311.
  See PORTING.md, "The strategy" — this exact patch has already been probed
  end to end and reached the BUILD-side sub-configure.
EOF
        missing=1
    fi
    if [ ! -f "$LO_SRC/solenv/gbuild/platform/WASI_INTEL_GCC.mk" ]; then
        cat >&2 <<'EOF'

libreoffice: src/solenv/gbuild/platform/WASI_INTEL_GCC.mk does not exist.

  gbuild.mk:195 includes platform/$(OS)_$(CPUNAME)_$(COM).mk. With the WASI arm
  keeping Emscripten's CPUNAME=INTEL lie (PORTING.md decision 9) that file is
  WASI_INTEL_GCC.mk, and only EMSCRIPTEN_INTEL_GCC.mk exists today.

  Needed: patches/core-NNNN-gbuild-wasi-platform.patch. Derive it from
  EMSCRIPTEN_INTEL_GCC.mk (122 lines, itself a thin delta over unxgcc.mk):
  drop every `-s FOO=` emcc flag, -pthread/USE_PTHREADS, --bind,
  FORCE_FILESYSTEM, EXPORTED_RUNTIME_METHODS and the .worker.js/emdwp
  auxtargets; KEEP -Wl,--gc-sections (unlike Emscripten we probe as supporting
  it); set gb_Executable_EXT to empty (wasm-component-ld emits the component at
  link time); and put the LINK half of the flag set in gb_LinkTarget_LDFLAGS:
  -lunwind -lsetjmp, -lwasi-emulated-{signal,mman,process-clocks,getpid},
  -Wl,-z,stack-size=8388608 and the wkgfx component-type object.
  It must also strip -Wl,--start-group/--end-group, which unxgcc.mk:159,166
  emits on every DISABLE_DYNLOADING executable link and which BOTH wasm-ld and
  wasm-component-ld reject (verified). See PORTING.md experiment E1 first.
EOF
        missing=1
    fi
    [ "$missing" -eq 0 ] || lo_die "missing structural patches; see above and PORTING.md"
}

# patches/core-NNNN-*.patch, -p1 at the LibreOffice source root, applied
# idempotently via the reverse-check idiom the other plugins use. src/ is a git
# clone rather than a tarball extraction, which makes the reverse check cheap.
lo_apply_patches() {
    local p
    [ -d "$LO_PATCHES" ] || return 0
    for p in "$LO_PATCHES"/core-*.patch; do
        [ -e "$p" ] || continue
        if git -C "$LO_SRC" apply --reverse --check "$p" >/dev/null 2>&1; then
            echo "  patch (already applied): $(basename "$p")"
            continue
        fi
        echo "  patch: $(basename "$p")"
        git -C "$LO_SRC" apply "$p" || lo_die "patch failed: $(basename "$p")"
    done
}

lo_require_src() {
    [ -d "$LO_SRC/.git" ] || lo_die "src/ is not a LibreOffice checkout — see PORTING.md"
    local t
    t="$(git -C "$LO_SRC" describe --tags 2>/dev/null || true)"
    [ "$t" = "$LO_TAG" ] || echo "libreoffice: WARNING src/ is at '$t', expected '$LO_TAG'" >&2
}

lo_require_configured() {
    [ -f "$LO_BUILD/config_host.mk" ] || lo_die \
        "build/config_host.mk missing — run ./build-configure.sh first"
}
