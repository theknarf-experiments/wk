#!/usr/bin/env bash
# Cross-compile the REAL pdfTeX (TeX Live's web2c engine, 1.40.29) to a
# wasm32-wasip2 component, and assemble the minimal texmf tree it typesets
# from. The result: a wk bash node runs
#     pandoc notes.md -s -o notes.tex && pdflatex notes.tex
# and gets a PDF, entirely inside the sandbox. latex.fmt is NOT built here —
# the Dockerfile dumps it with `pdftex -ini` in a RUN step, because wk's image
# builds execute wasm (see ./Dockerfile).
#
# Resumable: every stage checks for its artifact and is skipped when present
# (delete tex/<stage> to redo one). Stages:
#   1. tex/texlive-source     pinned TeX Live source checkout (+ our patch)
#   2. tex/tl-native          native pass: tangle/ctangle/tie/otangle + a host
#                             pdftex/kpsewhich (standard cross-TeX practice —
#                             web2c generates C with build-machine tools)
#   3. tex/tl-wasm            the cross pass -> pdftex.wasm
#   4. texmf/                 minimal texmf-dist from CTAN tlnet packages
#
# The build knowledge, each item a failure first (bash/build.sh style):
#   * --enable-web2c is required alongside --enable-pdftex: --disable-all-pkgs
#     turns web2c itself off, and pdftex lives inside it.
#   * every engine except pdftex is disabled explicitly, or the tree drags in
#     gmp/mpfr/cairo (mp, mf) and icu/harfbuzz (xetex) — all unbuildable or
#     pointless on wasm.
#   * kpathsea's line.c calls flockfile/getc_unlocked *unconditionally*
#     ("perhaps we will be lucky enough" — not on WASI): compat/wasi-compat.h
#     is force-included (-include) to paper over that and to declare the
#     process-y functions (execvp, popen, getuid...) wasi-libc's headers omit.
#     compat/include/ adds stub <sys/wait.h>, <pwd.h>, <syslog.h>.
#   * the runtime halves of those stubs live in compat/wasi-shim.c, an archive
#     (libwkshim.a) every link pulls: system()/popen() fail with ENOSYS (TeX
#     reads that as "no shell escape"), getpwnam answers nobody, mkstemp is
#     implemented for real. It also carries wk's __WK_EXEC_CWD constructor
#     (exec-compat/chdir_shim.c): without it every child starts at "/" and
#     `cd /work && pdflatex notes.tex` can't find its input. It rides in this
#     object *because* the other symbols are referenced — archive laziness
#     would drop a constructor-only member.
#   * wasm-specific link inputs go in LDFLAGS, NOT LIBS: TL configures some
#     subdirectories for the *build* machine (BUILDCC/BUILDLDFLAGS override
#     CC/LDFLAGS there), but nothing overrides LIBS, so a wasm -l in LIBS
#     poisons every native sub-configure ("C compiler cannot create
#     executables").
#   * TeX itself needs no setjmp, but bibtex (built alongside) and libpng's
#     error paths longjmp: the sjlj flags + -lsetjmp (same recipe as
#     plugins/lua) lower it to wasm EH, which wk's engine enables.
#   * libs/luajit's configure probes a *32-bit native* compiler (-m32) even
#     though nothing needs luajit — impossible on arm64 macOS. The generated
#     libs/Makefile's CONF_SUBDIRS is trimmed to what pdftex actually uses.
#   * web2c's cross configure requires native tangle, ctangle, tie AND otangle
#     in PATH; otangle isn't built by a pdftex-only native pass, so stage 2
#     makes it explicitly.
#   * patches/wk-0001: kpathsea's PATH search for argv[0] requires an execute
#     bit; wk's vfs has no mode bits at all, so `pdflatex` (found via PATH by
#     bash, argv[0] unqualified) died with "Can't get directory of program
#     name". On WASI a stat-able non-directory on PATH is the program.
#
# texmf notes:
#   * tlnet archive packages are ready-made TDS trees (some nest a
#     texmf-dist/ prefix — flattened here). They are NOT versioned upstream:
#     the archive tracks current TeX Live. Acceptable for this exploratory
#     tree; the pinned thing is the engine source.
#   * the package set = what latex.fmt needs (latex-base, l3kernel, cm,
#     hyphenation, unicode-data) + what pandoc's default standalone template
#     loads (amsmath, xcolor, lmodern, hyperref/bookmark and its oberdiek
#     constellation, ec metrics for the transient T1+cm moment before lmodern
#     takes over). ~35 MB; wk's lazy layers materialize per file read.
#   * language.def/language.dat ship listing every hyphenation language in
#     the distribution; ours are replaced by the English-only .us variants or
#     etex.src aborts on the missing pattern files.
#   * fonts/map/pdftex/pdftex/pdftex.map is synthesized by concatenating the
#     amsfonts cm maps + the lm maps — the moral equivalent of updmap for a
#     two-family tree.
#   * texmf.cnf (tracked, ../texmf.cnf) sets search paths for exactly this
#     layout and generous array sizes (param_size etc.: the l3 kernel blows
#     through web2c's tiny defaults at runtime).
#
# Requires: wasi-sdk-34-rc.2 (same guard as plugins/bash), git, curl, cc.
set -euo pipefail
cd "$(dirname "$0")"

MISE_SDK="$HOME/.local/share/mise/installs/github-web-assembly-wasi-sdk/wasi-sdk-34-rc.2"
WASI_SDK="${WASI_SDK:-$([ -d "$MISE_SDK" ] && echo "$MISE_SDK" || echo "$HOME/wasi-sdk")}"
EXPECT="wasi-sdk-34-rc.2"
case "$WASI_SDK" in
    *"$EXPECT"*) ;;
    *)
        echo "doctools: expected $EXPECT (set WASI_SDK), got: $WASI_SDK" >&2
        exit 1
        ;;
esac

# TeX Live source, pinned by commit (master has no useful tags on GitHub).
TL_REPO="https://github.com/TeX-Live/texlive-source.git"
TL_COMMIT="0e9787e9100b91502f39c5cbc7761738de07dc19"

CMP="$PWD/compat"
TEX="$PWD/tex"
mkdir -p "$TEX"

# ------------------------------------------------------------- 1. source ----
if [ ! -d "$TEX/texlive-source" ]; then
    echo "== fetching texlive-source @ ${TL_COMMIT:0:12}"
    git init -q "$TEX/texlive-source"
    git -C "$TEX/texlive-source" remote add origin "$TL_REPO"
    git -C "$TEX/texlive-source" fetch -q --depth 1 origin "$TL_COMMIT"
    git -C "$TEX/texlive-source" checkout -q FETCH_HEAD
    for p in patches/wk-*.patch; do
        (cd "$TEX/texlive-source" && patch -p1 --forward < "$OLDPWD/$p")
    done
fi

# The engines nobody asked for, off explicitly (see header).
DISABLES="--disable-tex --disable-mf --disable-mf-nowin --disable-mp \
  --disable-pmp --disable-upmp --disable-luatex --disable-luajittex \
  --disable-luahbtex --disable-luajithbtex --disable-xetex --disable-ptex \
  --disable-eptex --disable-uptex --disable-euptex --disable-aleph \
  --disable-hitex --disable-etex --disable-mflua --disable-mfluajit"
COMMON="--disable-all-pkgs --enable-web2c --enable-pdftex $DISABLES \
  --disable-shared --without-x --disable-multiplatform"

# ------------------------------------------------- 2. native tools pass ----
if [ ! -x "$TEX/tl-native-prefix/bin/tangle" ]; then
    echo "== native pass (tangle/ctangle/tie/otangle + host pdftex)"
    rm -rf "$TEX/tl-native" && mkdir -p "$TEX/tl-native"
    (cd "$TEX/tl-native" && \
        "$TEX/texlive-source/configure" -C $COMMON \
            --prefix="$TEX/tl-native-prefix" && \
        make -j8 && make install && \
        make -C texk/web2c otangle && \
        cp texk/web2c/otangle "$TEX/tl-native-prefix/bin/")
fi

# ------------------------------------------------------ 3. cross pass ----
if [ ! -f pdftex.wasm ] || [ compat/wasi-shim.c -nt pdftex.wasm ]; then
    echo "== cross pass (wasm32-wasip2)"
    mkdir -p "$TEX/shim"
    "$WASI_SDK/bin/clang" --target=wasm32-wasip2 -O2 \
        -c "$CMP/wasi-shim.c" -o "$TEX/shim/wasi-shim.o"
    "$WASI_SDK/bin/llvm-ar" rcs "$TEX/shim/libwkshim.a" "$TEX/shim/wasi-shim.o"

    WFLAGS="-O2 -include $CMP/wasi-compat.h -isystem $CMP/include \
      -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_PROCESS_CLOCKS \
      -D_WASI_EMULATED_GETPID \
      -mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false"
    rm -rf "$TEX/tl-wasm" && mkdir -p "$TEX/tl-wasm"
    (cd "$TEX/tl-wasm" && \
        PATH="$TEX/tl-native-prefix/bin:$PATH" \
        "$TEX/texlive-source/configure" -C $COMMON \
            --host=wasm32-wasip2 --build="$(cc -dumpmachine)" \
            --prefix="$TEX/tl-wasm-prefix" \
            CC="$WASI_SDK/bin/clang --target=wasm32-wasip2" \
            CXX="$WASI_SDK/bin/clang++ --target=wasm32-wasip2" \
            AR="$WASI_SDK/bin/llvm-ar" RANLIB="$WASI_SDK/bin/llvm-ranlib" \
            CFLAGS="$WFLAGS" CXXFLAGS="$WFLAGS -fno-exceptions" \
            LDFLAGS="-L$TEX/shim -lwkshim -lsetjmp -lwasi-emulated-signal \
              -lwasi-emulated-process-clocks -lwasi-emulated-getpid" \
            BUILDCC=cc BUILDCFLAGS=-O2 BUILDLDFLAGS= BUILDCPPFLAGS= \
            ac_cv_func_flockfile=no ac_cv_func_funlockfile=no && \
        LC_ALL=C sed -i.bak 's/^CONF_SUBDIRS = .*/CONF_SUBDIRS = zlib libpng xpdf /' \
            libs/Makefile && \
        PATH="$TEX/tl-native-prefix/bin:$PATH" make -j8)
    cp "$TEX/tl-wasm/texk/web2c/pdftex" pdftex.wasm
    echo "pdftex.wasm ready ($(du -h pdftex.wasm | cut -f1))"
fi

# ---------------------------------------------------------- 4. texmf ----
# The format-time set, then the pandoc-template set (see header).
TEXMF_PKGS="latex latex-fonts l3kernel l3backend cm knuth-lib hyphen-base \
  plain etex tex-ini-files latexconfig amsfonts pdftex unicode-data \
  xcolor amsmath iftex lm hyperref bookmark url kvoptions kvsetkeys ltxcmds \
  pdfescape pdftexcmds infwarerr intcalc etexcmds bitset bigintcalc \
  letltxmacro auxhook kvdefinekeys refcount gettitlestring uniquecounter \
  rerunfilecheck hycolor stringenc atbegshi atveryend graphics graphics-cfg \
  graphics-def epstopdf-pkg upquote etoolbox ec"

if [ ! -f texmf/texmf-dist/tex/latex/base/latex.ltx ]; then
    echo "== assembling the texmf tree"
    mkdir -p "$TEX/pkgs" texmf/texmf-dist
    for p in $TEXMF_PKGS; do
        [ -f "$TEX/pkgs/$p.tar.xz" ] || \
            curl -fsSL --retry 3 -o "$TEX/pkgs/$p.tar.xz" \
                "https://mirrors.ctan.org/systems/texlive/tlnet/archive/$p.tar.xz"
        tar -xf "$TEX/pkgs/$p.tar.xz" -C texmf/texmf-dist
    done
    (cd texmf/texmf-dist && \
        rm -rf tlpkg doc source && \
        if [ -d texmf-dist ]; then \
            cp -R texmf-dist/. . && rm -rf texmf-dist; \
        fi && \
        rm -rf doc fonts/source fonts/opentype fonts/afm && \
        cd tex/generic/config && \
        cp language.us.def language.def && \
        cp language.us language.dat && \
        cp language.us.lua language.dat.lua)
    # updmap, morally: the cm + lm families this tree ships.
    (cd texmf/texmf-dist && mkdir -p fonts/map/pdftex/pdftex && \
        cat fonts/map/dvips/amsfonts/cm.map \
            fonts/map/dvips/amsfonts/cmextra.map \
            fonts/map/dvips/amsfonts/latxfont.map \
            fonts/map/dvips/amsfonts/symbols.map \
            fonts/map/dvips/lm/lm-ec.map \
            fonts/map/dvips/lm/lm-ts1.map \
            fonts/map/dvips/lm/lm-rm.map \
            fonts/map/dvips/lm/lm-math.map \
            fonts/map/dvips/lm/lm-texnansi.map \
            > fonts/map/pdftex/pdftex/pdftex.map)
fi
mkdir -p texmf/texmf-dist/web2c
cp texmf.cnf texmf/texmf-dist/web2c/texmf.cnf

# The command names, multicall-style.
mkdir -p bin
ln -sf pdftex.wasm bin/pdftex
ln -sf pdftex.wasm bin/pdflatex

echo "tex artifacts ready: pdftex.wasm + texmf/ ($(du -sh texmf | cut -f1))"
