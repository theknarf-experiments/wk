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

Point git at the tracked `.hooks` directory (runs `cargo check` before
each commit):

```
mise run setup-hooks
```

Then run:

```
cargo run -- example1
```

### Setup for components

```
cargo install cargo-component
```
