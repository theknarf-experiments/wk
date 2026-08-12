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
  host file (HostMappedFile). The filesystem has real symlinks, so images that
  ship one binary behind many names (busybox, coreutils) work as built.
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
| node → Api              | serves the wk API on the node's network (`api:1337`) |

A document can hold several workspaces (shown as tabs); edits are undoable.

### Node capability tokens

Wiring says what a node *is connected to*; a node's **capability token** (a
[Biscuit](https://www.biscuitsec.org/)) decides what those wires actually
*grant*. Every app node holds a token whose authority block carries one
Datalog rule — "a node may use what it is wired to, in every mode":

```
can_use($kind, $target, $action) <- wired($kind, $target), operation($kind, $target, $action);
```

The server re-checks it every tick, feeding the canvas graph in as
`wired(kind, target)` facts and each grant as an `operation(kind, target,
action)`. Kinds: `file`, `midi`, `port`, `net`, `gateway` (host access is its
own kind, so it can be cut off separately), `capture`, and `scene` (a node's
`wk:scene` 3D objects — no wire needed, so a second rule in the base token
allows it outright). Actions: `read`/`write` on files, `send`/`receive` on
MIDI, `read` on capture, `show` on scene, `use` for the rest. Because the
policy lives in the token, it can be narrowed offline by *attenuation*
(appending checks needs no key) or replaced wholesale to make access work
differently:

```
wk token show vim                                # print the token's Datalog
wk token attenuate vim 'check if operation($k, $t, $a), $k != "net"'                 # off every network
wk token attenuate vim 'check if operation($k, $t, $a), $k != "file" || $a == "read"' # files read-only
wk token attenuate vim 'check if operation($k, $t, $a), $k != "gateway"'             # no host access
wk token attenuate totem 'check if operation($k, $t, $a), $k != "scene"'             # mute its 3D objects
wk token reset vim                               # back to wired ⇒ usable
```

**The API is a node too.** Wire an app to an **Api** node and the wk API
appears on the app's virtual network as a fabric peer named `api`, serving the
same newline-JSON protocol as the CLI socket on port `1337`. Each connection
implicitly bears the *node's own* capability token — so what a node may do
over the API is exactly what its token's Datalog grants (the default grants no
`right(...)` facts: connect-but-do-nothing until you say otherwise). The node
also finds its token at `/run/wk/token`, for presenting elsewhere or
attenuating offline. Craft test tokens with the workspace key:

```
wk create api ; wk wire python <api-id>
wk token mint 'right("document", "read");
               can_use($k,$t,$a) <- wired($k,$t), operation($k,$t,$a);'
wk token set python <hex>          # now GetSnapshot works from inside; commands still refused
```

**Running programs (`wk:exec`).** WASI has no `fork`/`exec`, so a program in
a sandbox normally cannot start another — which is why a shell there can only
be a script engine. wk runs components itself (a node *is* one), so it offers
this as a capability instead: a guest calls `run(path, args, env, stdin)`, wk
reads that program out of the **node's own filesystem**, runs it to completion
sharing that filesystem, and returns its exit code and output. The child gets
the caller's files and nothing else — no surfaces, MIDI, capture, or network —
so it can reach nothing the caller couldn't, and nesting is depth-bounded.
It's `exec` without the `fork`. `spawn` is the same thing without the waiting:
it returns a `child` handle, and two children handed the same `pipe` are a
**real pipeline** — bytes move as they are written, through a bounded buffer,
and the reader sees end-of-file once the last writer exits. That is what `run`
cannot express, since with `run` the producer must finish before the consumer
starts. `plugins/exec-compat/pipedemo.c` is `seq 1 200000 | head -1`: `head`
takes its line and leaves, and `seq` then *fails* writing into a pipe nobody
reads — a pipeline that stops early, exactly as a shell reports it.
Unlike POSIX there is no parent copy of the pipe to remember to close: each
child gets its own counted end, so end-of-file is simply when the last one
exits.

**Every guest gets a real `pipe()`.** wasi-libc's wasip2 `pipe` is `ENOSYS` —
the component model had nothing to build one from until wasip3's async streams
— but wasip2 keeps the descriptor table in *guest* memory, and an entry there
is a fat pointer: data plus a vtable. `plugins/pipe-compat` puts wk's pipe
behind one, so `read`, `write`, `close`, `dup`, `poll` and `fstat` on the
resulting descriptor are libc's own and nothing linking it needs to know a
pipe is involved. It is the same extension point wasi-libc uses for its own
stdio. The self-test is an ordinary C program:

```
pipe() -> fds 3,4
write = 19
read = 19: through libc write
S_ISFIFO = 1 (a pipe, not a file)
dup(read end) = 5
at EOF, read = 0 (0 means EOF)
```

The catch is that the table's layout is private to wasi-libc, so
`wasilibc_descriptor_table.h` transcribes it from one pinned revision and
`build.sh` refuses to build against a different wasi-sdk — a moved field would
corrupt silently rather than fail to link. An image build's `RUN` step gets it
too — that is what makes `RUN ["/bin/bash.wasm", "-c", "..."]` a shell that can
run real commands, and it grants nothing extra: the child comes out of, and
runs against, the same filesystem the step can already write to. `plugins/exec-compat` has a C shim
(`wk_run()`) and a demo that drives the real GNU coreutils this way, pipeline
included — and `plugins/bash` uses it for real: a one-hunk patch replaces the
fork+exec in bash's `execute_disk_command()` with a synchronous run, so
**bash actually runs commands**:

```
bash-5.2# ls -1 /
bin  etc  run
bash-5.2# mkdir -p /work && echo ok
ok
bash-5.2# ls /bin > /tmp/ls.txt && wc -l < /tmp/ls.txt
92
bash-5.2# nosuchcommand
bash: nosuchcommand: command not found     # status 127
```

Command names are ordinary **symlinks** onto the coreutils multicall binary —
`/bin/ls -> coreutils.wasm` — the same install layout coreutils uses
everywhere, since wk's filesystem supports real links (created, followed,
`readlink`ed, and carried through OCI layers). bash's own PATH search finds
them and `argv[0]` stays `ls`, which is what coreutils dispatches on.
**Redirection works** — `>`, `>>`, `2>`, `<`, `exec 9>`, for builtins and for
exec'd commands alike. It needs `dup`, to save a descriptor and put it back,
and that is why bash is built for **wasip2**: there the descriptor table lives
in wasi-libc in guest memory, so wasi-sdk 34 could implement `dup`/`dup2`/
`F_DUPFD` (under wasip1 the table is inside the prebuilt adapter, out of
reach). Since the shell has no child to apply redirections in, the patch
applies them around the `wk:exec` call and undoes them after — the shape bash
already uses for builtins — and a `< file` stdin is read and handed to the
child, which takes its input as bytes. **Pipelines work too**, for external commands: `ls /bin | wc -l` counts 92,
`seq 1 20 | sort -r | head -1` chains three stages, and
`seq 1 200000 | head -1` returns at once rather than generating 1.3 MB. Each
stage is spawned with its stdio wired to the pipe behind the shell's own pipe
descriptors, and only the last is waited for — the earlier ones are let go of,
because waiting on a producer whose reader has left would deadlock until the
shell closes its copy of the descriptor. A stage that is a *builtin*
(`echo hi | wc -l`) is not handled: with no fork it would write to the shell's
stdout instead of the pipe, so rather than quietly give a wrong answer it is
left to fail on `fork`. Command substitution and here-documents are still
missing (the latter wants a temp file).
`wk images build plugins/bash/Dockerfile --tag wk-shell` packages the lot. Like every other capability it is token-gated (kind `exec`, allowed
by default): `wk token attenuate <node> 'check if operation($k, $t, $a), $k !=
"exec"'` revokes it within a tick.

A scene mute is *viewer-side*: the guest keeps its entity and keeps updating
it; it just stops rendering — the seam where future multi-user policy (shared
vs. local-only objects, muting someone else's node in your own view) slots in
as more Datalog, not new mechanism.

A denied wire stays on the canvas but grants nothing — swap the token back and
the mount/port/network returns on the next tick. A write-denied file wire
mounts read-only (writes fail inside the guest with `not-permitted`). Custom
tokens persist in the `.wk` file; the signing key lives beside it
(`workspace.wk.key`, gitignored). The same tokens gate the client side too:
commands need `right(resource, action)` grants, and reads (views, snapshots,
logs, attach) are checked against the same key.

## 3D worlds

Every `.wk` file is also a **world**, VRChat-style: open the command palette
(Cmd+K) and pick **3D View** to walk it. All of the file's workspaces exist in
the world at once (each is a cluster of nodes); the 2D canvas keeps its
per-tab views.

- **Controls** — hold the right mouse button to look around (WASD walks,
  `F` toggles free flight with Q/E for up/down, scroll travels the gaze).
  Grab a card anywhere — or an app panel by its floating label, or Cmd+drag —
  to carry it; the wheel pushes/pulls a held card. Grabbing also focuses the
  node, so the keyboard types into it (terminals and apps both work). Ports
  and wiring, the palette, and Esc-to-exit all work in 3D.
- **Poses** — a dragged node gets a free 3D pose, persisted in the file as
  `pos3d x y z yaw`. Nodes without one sit on a default cylinder derived from
  their 2D canvas position.
- **World scenes** — a top-level `world "scene.glb"` line surrounds the
  workspaces with a glTF scene. `example/home.wk` is a ready-made home world
  (`scripts/gen-home-world.py` regenerates its plaza):

  ```sh
  cargo run -- run example/home.wk     # then Cmd+K -> 3D View
  ```

- **`wk:scene`** — a plugin can be a real 3D object instead of (or as well
  as) a panel: it hands wk a GLB blob and a live transform, and polls
  hover/press/release ray events. `plugins/totem` is the reference — a
  spinning crystal you can click (and Cmd+drag to carry).

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

## The shell (`wk-base`)

`plugins/wsh` is wk's shell, and the base image other images build on. WASI has
no fork/exec, so it is **busybox-style**: one wasm binary where the shell *and*
every "external" command are in-process builtins — coreutils-flavored
(`ls cat cp mv rm mkdir head tail wc grep sort uniq cut tr seq find xxd ...`),
plus `curl` (plain HTTP over the fabric via `std::net`) and `wk` (the workspace
API through a wired Api node). It has pipelines, `> >> <` redirection,
`&& || ;`, quoting, `$VAR`/`$?`, `test`/`[`, and `-c "script"`.

```
cd plugins/wsh && mise run build          # -> wsh.wasm (wasm32-wasip2)
wk images build plugins/wsh/Dockerfile --tag wk-base
```

Then build on it — the shell doubles as the `RUN` interpreter:

```dockerfile
FROM wk-base
RUN ["/bin/wsh.wasm", "-c", "mkdir -p /etc && echo hi > /etc/motd"]
ENTRYPOINT ["/bin/wsh.wasm"]
```

Wire the node to an **Api** node and the shell can drive wk from inside the
sandbox — `wk ps`, `wk snapshot`, `wk send '<json>'` — with exactly the
authority its capability token grants (`wk token` prints the node's own token).

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

Pulled wasm is cached content-addressed (`~/.cache/wk/oci/blobs/` plus a
reference→digest index), so `run` only hits the network the first time.
`cargo run -- pull` re-pulls like `docker pull`: a moved tag repoints the
index; unchanged content is a cheap no-op.

`scripts/publish-known-set.sh` publishes the bundled plugins as a ready-made set.
