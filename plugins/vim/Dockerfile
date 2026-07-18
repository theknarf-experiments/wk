# Vim as a wasm container: the unmodified upstream vim component (built with
# --with-features=normal: +eval, +syntax, +diff, ...) plus its full runtime
# tree, built by wk itself (`docker://plugins/vim/Dockerfile` as a dependency
# source — no Docker needed). Run `mise run build` first: it produces vim.wasm
# and fetches vim-src/ (which includes runtime/).
#
# The runtime lands where the build's compiled-in default_vimruntime_dir
# points; the system vimrc declares the terminal's colours so defaults.vim
# enables syntax highlighting; HOME lets ~-expansion resolve internally
# (without it, vim shells out to glob — and there is no shell).
FROM scratch
COPY vim.wasm /vim.wasm
COPY vim-src/runtime /usr/share/vim/runtime
COPY vimrc /usr/share/vim/vimrc
ENV VIMRUNTIME=/usr/share/vim/runtime
ENV HOME=/root
ENTRYPOINT ["/vim.wasm"]
