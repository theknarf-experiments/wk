# Vim as a wasm container: the unmodified upstream vim component plus its full
# runtime tree, built by wk itself (`docker://plugins/vim/Dockerfile` as a
# dependency source — no Docker needed). Run `mise run build` first: it
# produces vim.wasm and fetches vim-src/ (which includes runtime/).
#
# The runtime lands where the build's compiled-in default_vimruntime_dir
# points, so :help (and friends) find their files; ENV makes it explicit.
# CMD keeps `-u NONE`: the tiny feature build has no +eval, so sourcing
# defaults.vim would error at startup.
FROM scratch
COPY vim.wasm /vim.wasm
COPY vim-src/runtime /usr/share/vim/runtime
ENV VIMRUNTIME=/usr/share/vim/runtime
ENTRYPOINT ["/vim.wasm"]
CMD ["-u", "NONE"]
