# drumstick-vpiano 2.11.1 as a wk node

A real third-party Qt 6 **MIDI** application, cross-compiled to
`wasm32-wasip2`, drawing on a `wasi:surface` through the wk QPA plugin and
**exchanging MIDI with other wk nodes over `wk:midi`**.

The previous two Qt ports (`qt-torrentfileeditor103`, `qt-slate…`) established
that a Qt app can render and take input inside a node. This one is about the
other half of being a node: having a **port** and being **wired to something**.
Click a key and a separate `plugins/fluidsynth` node plays the note; play a
`piano` node (or a USB keyboard through `midiin`) and this app lights the key
up and passes it through.

```
  piano / midiin ──midi──▶ [ drumstick-vpiano ] ──midi──▶ fluidsynth
                              (this node)
```

## Status

Built and **run**. `mise run test` passes end to end on wk's real runtime:

```
platform=wk style=fusion families=1
screen=1024x768
STATE tops=1 in='wk' out='wk'
keys drawn in rows 345..390
pressing a white key at (461.7, 385.5)
the piano sent note-on ch=0 note=60 vel=100
the SYNTH node logged: note-on ch=0 key=60 vel=100
the SYNTH node logged: note-off ch=0 key=60
wire cut; pressing another key at (632.72, 385.5)
the piano sent note-on ch=0 note=75 vel=100 into a cut wire
...and the synth heard nothing, as it must
injecting note-on 90 3c 64 from a phantom MIDI source
the piano logged: RECV noteon ch=0 note=60 vel=100
the keyboard repainted with the incoming note held down
PASS
```

Artifact: `drumstick-vpiano.wasm`, ~21 MB, a wasip2 **component** whose world
is `wasi:surface` + `wasi:frame-buffer` + `wasi:graphics-context` +
**`wk:midi/midi`** + `wk:clipboard/clipboard` + the usual `wasi:cli` set.

## Why drumstick-vpiano and not VMPK

VMPK is the better-known app and it is the wrong first target: its
`CMakeLists.txt` requires `Qt6::Network` unconditionally, it uses QDBus, and
`main.cpp` includes `<QThread>`. `plugins/qt` builds with `FEATURE_thread=OFF`
and no DBus.

`drumstick-vpiano` is the Drumstick libraries' own virtual-piano utility, and
it is the only tool in `utils/` that is not gated on ALSA. It uses **Qt Core,
Gui and Widgets only**, has no thread, no DBus, no socket and no `dlopen`, and
it proves both MIDI directions in one binary — clicking a key sends note-on,
an incoming note calls `PianoKeybd::showNoteOn`.

**No port-local Qt sysroot.** That is the one structural difference from
`qt-torrentfileeditor103`, which builds `qt5compat` and `qtsvg` into its own
prefix. `Drumstick::RT` links Qt Core alone, `Drumstick::Widgets` links Qt
Widgets alone, `PianoKeybd`'s resource file is PNGs (so no Qt Svg), and
`Core5Compat` is wanted only by `Drumstick::File`, which `BUILD_FILE=OFF`
removes. `plugins/qt/sysroot` is enough.

## The port: a Drumstick RT backend over wk:midi

Drumstick reaches MIDI hardware through plugins implementing `MIDIInput` and
`MIDIOutput`. On this platform **every backend upstream ships is gated off** —
ALSA is Linux-only, mac/win are the wrong OS, OSS wants CMake's `UNIX` (which
`Platform/WASI.cmake` does not set), FluidSynth/Sonivox/PulseAudio are absent,
and the Network one is `QUdpSocket` over multicast. So drumstick builds *zero*
backends and `BackendManager` finds nothing.

`patches/0003` adds a `wk` pair, modelled line for line on upstream's own
`net-in`/`net-out`:

| | |
|---|---|
| `library/rt-backends/wk-common` | the wk:midi shim (`../../shim/wkmidiio.c` + wit-bindgen output), compiled **once** for both halves, plus the shared connection name |
| `library/rt-backends/wk-out` | `MIDIOutput`: nine typed slots → MIDI 1.0 wire bytes → `wkmidi_send` |
| `library/rt-backends/wk-in` | `MIDIInput`: drains the node's inbox into drumstick's own `MIDIParser`, which emits all ten signals and implements MIDI-thru |

Both are `QT_STATICPLUGIN` archives named with `Q_IMPORT_PLUGIN` in
`vpianomain.cpp`; `BackendManager::refresh()` finds them through
`QPluginLoader::staticInstances()` — the mechanism upstream already has, no
`dlopen` involved. **Not one line of the application changed** to make this
work.

### The input pump: why a 1 ms timer is the right answer here

This is the part that shaped the port, and it deserves to be read before
anybody "fixes" it.

Every Drumstick input backend either blocks a `QThread` on a device
(`alsa-in`) or hangs a `QSocketNotifier` off a descriptor (`net-in`, `oss-in`).
Neither is available:

* `FEATURE_thread=OFF` — a `QThread` links and never runs. There is one thread
  and it is the one painting.
* **`wk:midi/midi` has no pollable.** The interface is
  `resource input { constructor(); receive: func() -> option<list<u8>> }` —
  a non-blocking pop off a queue the host owns
  (`crates/wk-server/src/midi.rs`). There is no fd, and nothing to hand the QPA
  dispatcher's single `wasi:io/poll` call.

So `wk-in` polls, on a `Qt::PreciseTimer` at 1 ms (`WK_MIDI_POLL_MS`
overrides; `0` disables the pump, which is a useful negative control).

`plugins/qt/PORTING.md` forbids papering over a missing capability with a
polling `QTimer`, and this is **not** that: that rule is about a descriptor
that *has* a pollable and wants a socket notifier. Polling is the only
mechanism this interface offers, it is what every existing wk MIDI guest
already does (`plugins/fluidsynth`, `arp`, `synth`), and it costs nothing
structurally — `QWkEventDispatcher` already folds `m_timers.timerWait()` into
its frame wait, so a running timer only shortens a block that existed anyway.

Measured honestly: expect ~1–3 ms of jitter, with a tail out to one frame
(~16.7 ms) whenever a repaint, a modal `exec()` or a scene relayout is in
flight. Fine for playing notes (human timing is ±10 ms); **not** good enough
for a tight sequencer. vpiano sidesteps the question entirely because it is
purely reactive and has no transport of its own — any future app *with* a
sequencer inherits the whole problem.

**The principled follow-on** is `subscribe: func() -> pollable` on
`wk:midi/midi`, which would delete this timer and put MIDI on the same poll
list socket notifiers use. It is backward-compatible for the five existing Rust
MIDI plugins (a component import only has to be a subtype of what the host
provides). It is deliberately **not** part of this work: it needs a `Pollable`
over `midi.rs`'s `Inbox` plus threading extra pollables through
`wkgfx_wait_frame_timeout`, i.e. changes to the *shared* `gfx-compat` and QPA,
which should not ride on an app port.

## The shim is local, and should not stay that way

`shim/wkmidiio.{c,h}` is this port's own C wrapper over `wk:midi`. It generates
its bindings from `../midi-compat/wit`, so there is exactly one definition of
the interface in the tree — but the wrapper itself is duplicated, and that is a
deliberate, temporary choice.

`plugins/midi-compat` is **input-only** (`wkmidi_open` + `wkmidi_recv`); it was
written for `plugins/fluidsynth`, which only ever consumes MIDI. A virtual
piano is the first guest that has to **send**, and `resource output { send:
func(data: list<u8>) }` has been in the WIT all along with no C wrapper over
it. The output half here is therefore new code, not a new capability, and it
belongs upstream in `plugins/midi-compat` as `wkmidi_open_out` / `wkmidi_send`.
It was kept local because several Qt ports were being written against the
shared directories at the same time and this one must not break
`plugins/fluidsynth`. **Fold it back when that is over**, and delete
`shim/`.

## Three worlds in one component

The thing this port had to establish, and the thing that could have sunk it:
**several `*_component_type.o` objects link into one wasip2-direct component
and their worlds MERGE**. The result imports `wasi:surface`, `wk:midi/midi` and
`wk:clipboard/clipboard` together, and `wk-server`'s `component_imports_midi`
then gives the node MIDI ports on the canvas with **no server change at all**.

Each object must arrive as a **link option**, never as a library: it carries
only a `component-type` custom section that `wasm-component-ld` reads, nothing
references it, so as an archive member the linker drops it and the component
comes out with no imports at all. `build.sh` passes them in
`WK_COMPONENT_TYPE_OBJS` and verifies the result with `wasm-tools component
wit` at the end of every build.

The clipboard one is **probed, not assumed**: the wk QPA grew a `QWkClipboard`
that lives inside `libqwk.a`, so an app linking that QPA has an undefined
`__component_type_object_force_link_wkclipboard` whether or not it ever copies
anything. `build.sh` runs `llvm-nm` over the archive and only then generates
and links the clipboard bindings, so this port builds against a `libqwk.a` from
either side of that change.

## Traps found here that will bite the next port

**`if(${CMAKE_SYSTEM_NAME} MATCHES "WASI")` is always FALSE.** It is the form
upstream uses for its Darwin and Windows arms, so copying it is the natural
thing to do. `${CMAKE_SYSTEM_NAME}` expands to the bare word `WASI`, CMake
dereferences an unquoted `MATCHES` operand when a variable of that name exists,
and `Platform/WASI.cmake` is literally `set(WASI 1)` — so the test becomes
`if(1 MATCHES "WASI")`. Silently. Nothing is built, the app links, and the
first symptom is `qFatal("Unable to find a suitable input backend")` at
runtime. Use `STREQUAL`. Probe:

```cmake
set(WASI 1)
set(X WASI)
if(${X} MATCHES "WASI")  # FALSE
if(X STREQUAL "WASI")    # TRUE
```

**`git apply` inside the wk work tree silently skips every hunk.** The
extracted upstream tree lives under `plugins/<port>/src/`, which is inside wk's
own git work tree, so `git -C "$tree" apply patch.diff` discovers wk's `.git`,
resolves the patch's paths against the **repo root**, finds them outside the
current subdirectory and skips them — **exit status 0**, message only under
`--verbose`. The build then compiles pristine sources and fails somewhere
unrelated a minute later. `build.sh` sets `GIT_CEILING_DIRECTORIES="$SRCDIR"`,
which stops git's upward search and makes `git apply` resolve against the cwd.
*Every other port in this repo using `git -C "$tree" apply` is exposed to the
same thing.*

**`project(... LANGUAGES CXX)` cannot link a C-only target**: "CMake can not
determine linker language". The wk shim is C; `wk-common/CMakeLists.txt` calls
`enable_language(C)` rather than editing upstream's `project()`. Compiling the
generated bindings as C++ is not an option — they use C compound literals.

**`find_package(PkgConfig REQUIRED)` fails inside this cross build** even with
`pkg-config` plainly on `PATH` and `CMAKE_FIND_ROOT_PATH_MODE_PROGRAM=NEVER` in
`wasip2.cmake`: something between that and Qt's own find-root handling confines
the search to the sysroot. `build.sh` passes `-DPKG_CONFIG_EXECUTABLE=` and
sidesteps the question; nothing here actually uses pkg-config.

## What was new for the wk Qt platform

**`QGraphicsView` / `QGraphicsScene`, exercised for the first time.**
`PianoKeybd` *is* a `QGraphicsView` and `PianoScene` a `QGraphicsScene`; the
whole app is that one widget. The existing ports exercise item views
(torrent-file-editor) and Qt Quick software (Slate). It works: the scene
paints, `fitInView` scales, and `QGraphicsSceneMouseEvent` hit-testing turns a
real `wasi:surface` pointer press into the right key.

One practical consequence for anyone writing a test against a `QGraphicsView`:
`fitInView(sceneRect, Qt::KeepAspectRatio)` means the *drawn* content is not
the widget rect. An 88-key keyboard in a 1006×702 widget is a **46-pixel-tall
strip** centred vertically in a much larger expanse of background, so aiming a
click at a fraction of the widget rect misses entirely — and misses silently,
because a click on the background is not an error. The harness finds the strip
in the pixels (the only rows inside the rect carrying dark pixels) and aims at
the bottom tenth of it, which is white keys all the way across.

## Known gaps

* **The Connections dialog lists exactly one port**, `wk fabric`, for both in
  and out. That is correct — a node has one MIDI inbox and one outbox, and
  where they go is decided by the wires on the canvas — but it looks like a
  broken driver until you know, hence the name and the note in `example.wk`.
* **MIDI-in latency is a poll interval**, not an interrupt. See above.
* **`wk:midi` carries no clock and no timestamps.** A message's arrival time is
  its time. Nothing needing MIDI clock (0xF8) or song-position between nodes
  has a jitter-correction path.
* **The host inbox drops past 1024 queued messages, silently**
  (`crates/wk-server/src/midi.rs`). A guest stalled under a dense stream loses
  notes, most visibly as stuck ones — a dropped note-off never arrives.
* **`QSettings` persistence is per-run.** `HOME=/root` in the image gives
  `QStandardPaths` somewhere to point, but a node's vfs is not persisted by
  default, so window geometry and preferences reset. `--portable` switches to
  an INI beside the executable if that is ever wanted.
* **No IME** (the compositor has no `WindowEvent::Ime` arm), so no dead keys or
  CJK. Irrelevant here; vpiano's computer-keyboard-plays-notes mode uses
  layout-independent `Qt::Key_*` values and works. Its *Raw Computer Keyboard*
  mode does not: `g_DefaultRawKeyMap` in `pianokeybd.cpp` has entries only
  under `Q_OS_LINUX`/`Q_OS_WIN`/`Q_OS_MAC`, all false here, so the map is
  empty.
* **Licence step-up:** drumstick is **GPLv3**, where torrent-file-editor and
  Slate are more permissive. Nothing is distributed from this repo, but it is
  worth knowing before anything is.

## Building

```
cd plugins/qt-drumstickvpiano2111
mise run build       # ~5 min, needs plugins/qt built (hours; see its PORTING.md)
mise run test        # needs plugins/fluidsynth built too — the wire has two ends
wk run example.wk    # from the repo root
```

`build.sh`'s preflight names any missing prerequisite. `plugins/qt` is
deliberately **not** a mise `depends`: that would make a repo-wide
`mise run build-plugins` sweep silently start an hours-long Qt build.
