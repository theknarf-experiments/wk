# clap — compile CLAP plugins into wk:clap components

A toolkit that compiles **unmodified CLAP plugins** into wk components
implementing the [`wk:clap`](../../crates/wk-server/wit-clap/world.wit) WIT world,
so wk can host the CLAP ecosystem. wk drives them from the host side
(`crates/wk-server/src/clap_host.rs`).

## How it works

- `shim.c` — the bridge. Implements the `wk:clap` `plugins` exports by driving a
  plugin's `clap_plugin` vtable, and presents a `clap_host` (log / thread-check /
  latency / state / params) that forwards to the imported wk `host` interface.
  `get_extension`'s dynamic negotiation is replaced by folding the core
  extensions into the exported resource, with `features()` reporting which the
  plugin implements. Reusable across all plugins.
- `clap-include/` — the vendored CLAP SDK headers (MIT; see `clap-include/LICENSE`).
- `examples/` — one `clap_entry` translation unit per plugin. Porting a CLAP
  plugin is just dropping its source(s) here; no plugin changes.
- `gen/`, `build/` — generated bindings and output components (git-ignored).

## Examples

| file | what | extensions |
|------|------|------------|
| `examples/template.c` | official minimal CLAP plugin (free-audio/clap, MIT); L/R-swap effect | audio-ports, note-ports, state |
| `examples/gain.c` | stereo gain effect with one automatable parameter | audio-ports, params |
| `examples/polysynth.c` | polyphonic sine synth (note-on → voice, AR envelope) | note-ports, audio-ports, params |
| `examples/octaver.c` | note effect: doubles each note an octave up (CLAP output events) | note-ports |

Each builds to a reactor component exporting only `wk:clap/plugins`. They double
as the host-runtime test fixtures in `crates/wk-server/testdata/`.

## Build

    mise run build            # build every example -> build/*.wasm
    ./build.sh polysynth      # build one

Requires the shared toolchain from the repo root `mise.toml` (wasi-sdk,
wit-bindgen, wasm-tools).

## Porting another plugin

Drop its `clap_entry` source(s) into `examples/` (C or C++), then
`./build.sh <name>`. Each source is compiled with the right front-end and linked
with the (C) shim + bindings. Plugins that use only the core extensions
(audio-ports, note-ports, params, state) work as-is; unmodeled/draft/vendor
extensions degrade to unavailable. GUI (CLAP's webview extension) is not wired up
yet.

### Real third-party example: nakst's HelloCLAP

`./fetch-third-party.sh` downloads a genuine third-party CLAP plugin — nakst's
[HelloCLAP tutorial synth](https://nakst.gitlab.io/tutorial/clap-part-2.html), a
single-file C++ instrument — into `examples/` (git-ignored; not redistributed
here). Then:

    ./build.sh nakst-hello        # -> build/nakst-hello.wasm, a wk:clap component

This has been verified end to end: the unmodified plugin builds, and the wk:clap
host runtime instantiates it and plays it on a note-on — a real CLAP plugin
running in wk with no source changes.
