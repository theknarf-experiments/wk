# wk

`wk` the workspace tool

`wk` is a **workspace runtime for WebAssembly component plugins**. Each plugin
runs as a sandboxed *node* on a canvas — with its own GPU/CPU surface or
terminal, its own in-memory filesystem, and its own slice of a userspace
network — and you wire nodes together to make them cooperate: mount a file into
a node, route MIDI between two nodes, serve a node over localhost, or drop nodes
onto a shared virtual network.

Plugins are ordinary WASI 0.2/0.3 components. They can live as local `.wasm`
files or be pulled from an OCI registry, so `wk` doubles as a little package
manager for the components it runs.

## Concepts

A workspace is a canvas of **nodes**, saved to a `.wk` file ([KDL](https://kdl.dev/)
syntax). A node is a **plugin instance** by default: it renders into a virtual
surface (GPU via `wasi:webgpu`, or a CPU frame buffer) that `wk` composites into
its window, or it runs in a terminal (`wasi:cli` command components). Some nodes
instead stand for shared resources a plugin can wire to:

- **File** — a shared file, either in-memory (VirtualFile) or backed by a real
  host file (HostMappedFile).
- **HostPort** — a `localhost` port an HTTP node can be served on.
- **Network / Gateway** — an isolated userspace network (smoltcp). A Gateway
  additionally grants its members access to the real host network.

**Wiring** two nodes does something different depending on their kinds:

| wire                    | effect                                               |
| ----------------------- | ---------------------------------------------------- |
| File → node             | mounts the file into the node's sandboxed filesystem |
| node → node             | a MIDI link (source out → destination in)            |
| HTTP node → HostPort    | serves the node on `127.0.0.1:<port>`                |
| node → Network/Gateway  | joins the node to that virtual network               |

A document can hold several workspaces (shown as tabs); edits are undoable.

## Setup

The toolchain is pinned to nightly Rust by `rust-toolchain.toml`, so `rustup`
selects it automatically.

This repo uses [mise](https://mise.jdx.dev/) to manage the environment (it adds
Homebrew's `lib` to `LIBRARY_PATH` for native linking and defines a couple of
tasks). Install it and trust this directory:

```
brew install mise
mise trust
```

The tracked `.hooks/pre-commit` runs `cargo fmt --all -- --check`, `cargo clippy`
(warnings denied) and `cargo nextest run`. Install nextest and point git at the
hooks directory:

```
cargo install cargo-nextest
mise run setup-hooks
```

Then build the CLI:

```
cargo run -- --help
```

## Quick start

```
cargo run -- init                 # create workspace.wk in the current directory
cargo run -- add path/to/plugin.wasm   # register a plugin as a named dependency
cargo run -- run                  # open the workspace in a window
```

Every `.wk` file is its own workspace; pass `-f/--file` to operate on a specific
one (defaults to `workspace.wk`). Other commands: `list`, `remove <name>`, and
`publish` (below). `run --headless` loads and runs the workspace with no window,
keeping the guests alive until Ctrl-C.

## Plugins

Example plugins live under `plugins/`, spanning graphics (GPU via `wasi:webgpu`
and CPU frame buffers), audio and MIDI, terminal programs and recompiled C
software, userspace networking, and filesystem demos.

Every plugin exposes the same `build` task via `mise`, and the whole build
toolchain is pinned and installed by mise (declared in the root `mise.toml`), so
there's no manual toolchain setup — building is uniform:

```
mise trust        # first time only, to trust the plugin's mise.toml
mise run build    # installs the pinned toolchain if needed, then builds
```

Under the hood, Rust plugins build with
[cargo-component](https://github.com/bytecodealliance/cargo-component); C plugins
compile with [wasi-sdk](https://github.com/WebAssembly/wasi-sdk) and `wasm-tools`
— all mise-managed, so `WASI_SDK` and friends are wired up for you.

## OCI registries

`wk` can depend on plugins published to an OCI registry as Wasm OCI Artifacts.
`compose.yml` brings up a local registry (on `:5001`) for testing the whole
package-manager path:

```
docker compose up -d
cargo run -- publish <name> localhost:5001/<name>:1.0
cargo run -- add oci://localhost:5001/<name>:1.0
cargo run -- run
```

`scripts/publish-known-set.sh` publishes the bundled plugins as a ready-made set.
