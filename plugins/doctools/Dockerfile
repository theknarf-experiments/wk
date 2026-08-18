# doc-tools: document conversion + typesetting on top of the wk-shell image.
#
#   pandoc notes.md -s -o notes.tex && pdflatex notes.tex
#
# * pandoc — the real pandoc 3.5 (GHC wasm backend, upstream
#   haskell-wasm/pandoc-wasm build), fetched + componentized by build.sh
#   (pinned by sha256). Its data files are baked into the binary, so no
#   --data-dir tree is shipped.
# * pdftex/pdflatex — the real pdfTeX 1.40.29, cross-compiled from TeX Live
#   source by build-tex.sh (wasi-sdk + a small POSIX shim), with a minimal
#   texmf tree (latex-base + l3kernel + cm/amsfonts Type1s, ~18 MB — CTAN
#   packages, not the full distribution).
#
# The formats are not shipped: RUN steps execute wasm, so pdftex -ini dumps
# latex.fmt (and plain pdftex.fmt) DURING this build, exactly the way a real
# distribution's fmtutil would on install.
#
# Command names are symlinks onto the components, the same multicall layout
# the base image uses for coreutils (/bin/ls -> coreutils.wasm).
#
# Build + tag (also builds/tags the bash base): ./build.sh
FROM bash
COPY pandoc.wasm /bin/pandoc.wasm
COPY pdftex.wasm /bin/pdftex.wasm
COPY bin/ /bin/
COPY texmf/ /usr/share/texlive/
ENV TEXMFCNF=/usr/share/texlive/texmf-dist/web2c
ENV TEXMFVAR=/var/lib/texmf
RUN ["/bin/bash.wasm", "-c", "mkdir -p /var/lib/texmf /usr/share/texlive/texmf-dist/web2c/pdftex"]
# Dump the formats with the engine itself (a wasm RUN step): LaTeX + plain
# (pdfetex.ini is what fmtutil builds plain pdftex.fmt from these days).
RUN ["/bin/pdftex.wasm", "-ini", "-etex", "-interaction=nonstopmode", "-progname=pdflatex", "-jobname=pdflatex", "-output-directory=/usr/share/texlive/texmf-dist/web2c/pdftex", "pdflatex.ini"]
RUN ["/bin/pdftex.wasm", "-ini", "-etex", "-interaction=nonstopmode", "-progname=pdftex", "-jobname=pdftex", "-output-directory=/usr/share/texlive/texmf-dist/web2c/pdftex", "pdfetex.ini"]
ENTRYPOINT ["/bin/bash.wasm"]
