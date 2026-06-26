# WK

`wk` the workspace tool

## Setup

First install SDL3:

```
brew install sdl3
```

This repo uses [mise](https://mise.jdx.dev/) to manage the environment
(it sets `LIBRARY_PATH` so SDL3 is found at link time). Install it and
allow this directory's config:

```
brew install mise
mise trust
```

Point git at the tracked `.hooks` directory. The pre-commit hook runs
`cargo fmt --all -- --check`, `cargo check` (warnings denied) and
`cargo nextest run`, so install nextest first:

```
cargo install cargo-nextest
mise run setup-hooks
```

Then run:

```
cargo run -- --help
```

### Setup for components

```
cargo install cargo-component
```

## Plugins & projects

`wk` is a compositor for WASM plugins: each plugin is a component that renders
into a virtual surface (via the standard wasi-gfx interfaces) which `wk`
composites into its own window.

Run plugins directly:

```
cargo run -- run plugins/triangle/target/wasm32-wasip1/debug/triangle.wasm
```

Or manage a set of plugins with a [KDL](https://kdl.dev/) `wk.kdl` project:

```
wk init my-workspace          # create wk.kdl
wk add path/to/plugin.wasm    # register a plugin
wk run                        # run every plugin in the project
```

Example plugins live under `plugins/` (`paint` is a CPU/frame-buffer client,
`triangle` renders on the GPU via `wasi:webgpu`); build them with
`cargo component build`.
