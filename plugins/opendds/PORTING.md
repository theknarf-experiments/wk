# OpenDDS on wasm32-wasip2 — DDS pub/sub between wk nodes

The goal: two wk nodes running **real OpenDDS** — the Object Computing
implementation of the OMG Data Distribution Service, on real ACE/TAO — wired to
the same `Network` node, discovering each other over RTPS and exchanging
samples as ordinary UDP packets that wk's smoltcp fabric routes. Not a DDS
lookalike written in Rust on the host: the actual C++ middleware, cross-compiled,
inside the sandbox, speaking the wire protocol.

Pinned in `common.sh`: **OpenDDS 3.34.0**, **ACE 8 / TAO 4 (8.0.6)**.

## Layout

```
common.sh          version pins and shared paths; sourced by every script
fetch.sh           clone OpenDDS at the pinned tag
build-host.sh      stage 1 — the NATIVE build: tao_idl and opendds_idl
build-target.sh    stage 2 — the wasm32-wasip2 build
ace/               OUR ACE platform: config-wasi.h + platform_wasi.GNU
patches/           genuine diffs against OpenDDS and ACE
shim/              the link-line C shims (threads, sendmsg/recvmsg)
src/, host/        fetched and built trees; gitignored
```

`ace/` is deliberately not `patches/`: `config-<platform>.h` and
`platform_<platform>.GNU` are ACE's *own* extension points, one pair per
platform, so adding WASI to ACE is not a modification of ACE. Likewise
`shim/include/net/if.h` and `shim/include/wk-wasi-net-compat.h` supply
declarations wasi-libc withholds rather than carving the code that needs them
out of ACE. The true diffs are in [Patches](#patches).

## The three decisions everything else follows from

### 1. A genuine WASI platform for ACE, not a bolt-on

ACE has no wasm port and no emscripten port to inherit or fight, so unlike the
Qt port (`plugins/qt/PORTING.md`) there was nothing to opt out of. ACE's
configure-by-macro model is a good fit for a target that is POSIX-shaped with
whole subsystems missing, provided the gaps are *named* rather than discovered
at run time.

`ace/config-wasi.h` is that naming, and it is long on purpose. Two rules were
applied to every line of it, both of which caught real mistakes:

* **The macro must exist in ACE's sources.** An `ACE_LACKS_*` that ACE has
  never heard of is silently ignored — the config looks right and configures
  nothing. Worse, ACE has *retired* macros over the years:
  `ACE_LACKS_AUTO_PTR`, `ACE_LACKS_RPC_H`, `ACE_LACKS_RLIMIT_PROTOTYPE`,
  `ACE_HAS_STDCPP_STL_INCLUDES` and `ACE_LACKS_THREAD_STACK_ADDR` all survive
  only in `ChangeLogs/`, so "it was valid once" is not good enough. The first
  draft of the config contained five of those and seventeen outright
  inventions. `build-target.sh` now re-checks every name against ACE's actual
  sources before each configure and refuses to build otherwise.
* **The function must genuinely be absent, by `llvm-nm` over `libc.a`** — not
  by whether a header declares it. wasi-libc *declares* `getifaddrs`,
  `sem_init`, `sigaction`, `mmap`, `socketpair`, `sendmsg`, `recvmsg` and
  `dlopen`, and defines none of them. Several of those declarations sit inside
  `#ifdef __wasilibc_unmodified_upstream`, which is never defined, so they are
  not even visible — a *compile* error rather than a link error, in files far
  from anything you were thinking about.

### 2. One thread, and a condition variable that pumps

This is the crux of the port, and it is where OpenDDS differs from every other
port in this repo.

`plugins/libreoffice` could be threadless because Impress genuinely does not use
threads: its policy is "a timed wait times out, an untimed wait is a bug and
crashes loudly". **OpenDDS is not like that.** Its whole event architecture is
threads: `ReactorTask` spawns a thread to run `ACE_Reactor::run_reactor_event_loop`,
`DispatchService` runs a `ThreadPool` over `run_event_loop()`, and the entities
users touch (`WaitSet::wait`, `wait_for_acknowledgments`, discovery settling)
block on condition variables that only those threads can signal.

Two routes were considered and one is impossible:

* **Green threads** — an M:1 pthread emulation over `setjmp`/`longjmp`, the
  classic answer. It cannot work on wasm. `longjmp` can only unwind *down* the
  wasm call stack; the operand and control stacks are not addressable, so there
  is no way to switch to another one. (The C shadow stack in linear memory
  *can* be swapped, but that is only half a context.) Without the
  stack-switching proposal — which no C toolchain targets — a wasm guest has
  one stack, full stop.
* **Single-threaded OpenDDS**, driven by a pump. This is the route.

The mechanism is a link-line shim, `shim/wk-opendds-threads.c`, in the same
place and for the same reasons as LibreOffice's — the threading policy is one
file on the link line, not a hundred `#ifdef`s across two upstream trees — but
with the opposite semantics:

* `pthread_mutex_lock`/`unlock` become **no-ops that succeed**. In a process
  with one thread there is nobody to contend with, so this is not an
  approximation; it is exact.
* `pthread_cond_wait` **pumps and returns as a spurious wakeup.** This is the
  whole idea. In a single-threaded process the only thing that can ever make
  `while (!condition) cv.wait()` terminate is running the work that would have
  signalled it — so the wait runs one pass of the reactor and drains the event
  dispatcher, then returns. A spurious wakeup is legal POSIX, and every
  OpenDDS wait is already written as a predicate loop, so each of those loops
  turns into a correct polling loop with no change to OpenDDS at all.
* `pthread_cond_timedwait` does the same with a deadline, returning
  `ETIMEDOUT` once it passes. It must never return instantly when there is
  nothing to do, or an idle DDS participant becomes a 100%-CPU spin — the same
  trap LibreOffice's shim documents.

Recursion is the thing to get right: pumping runs callbacks that may themselves
wait. A re-entrant wait returns immediately as a spurious wakeup rather than
pumping again. And a wait that spins without progress for longer than a
watchdog interval **fails loudly**, because at that point it is a deadlock
wearing a livelock's clothes, and the honest thing is a backtrace.

The shim knows nothing about C++ or OpenDDS. It calls one hook:

```c
void (*wk_dds_pump_hook)(void);   /* NULL until a participant exists */
```

which the OpenDDS side installs. That keeps the layering clean and the shim
testable on its own.

Two OpenDDS patches then make the threads that must *run a loop* — as opposed
to merely blocking — unnecessary; see [Patches](#patches).

### 3. rtps_udp, and unicast discovery

The port builds the **rtps_udp** transport and RTPS discovery. Not `shmem`
(there is no shared memory and no second process), not `tcp` (works, but is not
the interesting case), and not `InfoRepo` (a CORBA service, and a second
process again).

RTPS discovery normally announces participants by **UDP multicast** (SPDP).
wk's fabric has no multicast today — `Network` is a hub that routes unicast IP
between its members — so the demo nodes use unicast SPDP:

```
[rtps_discovery/wk]
SedpMulticast=0
SpdpSendAddrs=<the other node>:7400

[transport/wk_rtps]
transport_type=rtps_udp
use_multicast=0
local_address=<this node>:<port>
```

`SpdpSendAddrs` does not *disable* multicast — a participant still emits SPDP
to the multicast address, where it is dropped — it adds unicast announcements
to named peers, and those are what actually carry discovery here. That is the
supported configuration for any network without multicast, which is what
OpenDDS's own docs recommend for NAT and WAN deployments.

Making the fabric carry real IP multicast is the better answer and is the
obvious next capability (the hub already sees every member's packets); it would
let stock RTPS discovery work unmodified, with no per-node peer list. It is not
a prerequisite for this port and is not attempted here.

### Which address am I

A wasm guest has no interface table: wasi-libc ships an `<ifaddrs.h>` with no
`getifaddrs` behind it, and ACE's `SIOCGIFCONF` fallback has nothing to answer
it either, so `ACE::get_ip_interfaces()` returns nothing. Under wk that is the
honest model rather than a gap — a node's fabric address (`10.0.0.x` /
`fd00::x`) is assigned by the *host*, not owned by the guest — so OpenDDS is
told its address explicitly through `local_address` rather than discovering it.

### Two nodes, two GUIDs

An RTPS participant's GUID is derived from process id plus host by default.
`getpid()` here comes from `libwasi-emulated-getpid` and answers the same
number in every node, so every participant would claim the same GUID and
discovery would collapse. The demo nodes set `GuidInterface` explicitly.

## Patches

Seven, none of them large.

| patch | what |
| --- | --- |
| `opendds-0001-configure-wasi-target.patch` | teaches OpenDDS's `configure` that `wasi` is a cross target, whose ACE config and platform file are the two in `ace/`. 12 lines, the same shape as its `linux-cross` and `android` entries. |
| `ace-0001-wasi-fd-set.patch` | `fd_set` is not a bitmask on this target — see below. |
| `ace-0002-max-handles-indeterminate.patch` | `ACE::max_handles()` returned `sysconf(_SC_OPEN_MAX)` verbatim, including POSIX's -1 "indeterminate". Falls through to `FD_SETSIZE` instead. Not WASI-specific; see "What the smoke test caught". |
| `ace-0003-wasi-nonblocking-datagrams.patch` | every datagram socket is non-blocking at `open()`, because wasip2 reports a UDP socket readable one time too many — see [The readiness that outlives the datagram](#the-readiness-that-outlives-the-datagram). |
| `opendds-0002-threadless-event-loops.patch` | `DispatchService` and `ReactorTask` expose one non-blocking pass of their loop and register it with the pump when no thread can be spawned; `ThreadPool` stops waiting at a barrier nothing can reach; `ReactorEvent` runs inline instead of through `reactor->notify()`. |
| `opendds-0003-no-ancillary-data.patch` | `set_socket_multicast_ttl` and `set_recvpktinfo` report success on a platform that has neither multicast options nor ancillary data. Both are fatal to participant creation otherwise. |
| `opendds-0004-eagain-is-not-a-broken-link.patch` | an `EWOULDBLOCK` read is "nothing to read", not a dead link. Three call sites; see [The empty read that killed the transport](#the-empty-read-that-killed-the-transport). |

### The fd_set that is not a bitmask

The most surprising thing wasi-libc does to ACE. Its `fd_set` is

```c
typedef struct { size_t __nfds; int __fds[FD_SETSIZE]; } fd_set;
```

— a count and an array of descriptors — and there is no `fd_mask`, no
`NFDBITS` and no `fds_bits` member anywhere in the sysroot. `ACE_Handle_Set`,
which backs the Select Reactor, indexes `fds_bits` as machine words.

ACE already has a second mode for precisely this shape, `ACE_HANDLE_SET_USES_FD_ARRAY`,
written for Win32's `{ u_int fd_count; SOCKET fd_array[]; }`. It was
unreachable for anyone else only because the member *names* were hard-coded in
five places. The patch turns those five into `ACE_FD_SET_COUNT` /
`ACE_FD_SET_ARRAY`, defaulting to the Win32 names, so a config can say
`__nfds` / `__fds` instead; adds `ACE_WASI` to the two `ACE_WIN32` guards that
were really "does this platform have an array fd_set" (the `NFDBITS` stand-in
in `os_select.h`, whose own comment already says it is *only used in unused
functions*); and drops the Win32-only `(SOCKET)` cast from the one `FD_SET`
call on that path. Nothing emulates a bitmask: `FD_SET`/`FD_CLR`/`FD_ISSET`
are wasi-libc's own, and only ACE's iterator, `num_set()` and `dump()` ever
look inside the struct.

### The readiness that outlives the datagram

The single most consequential platform fact this port found, and the one that
took longest to see, because every layer above it looked innocent.

**On wasm32-wasip2 a UDP socket stays reported readable until a receive finds
nothing.** Reading the last datagram does not clear readiness; the *empty* read
does. Five lines are enough to show it:

```
select before read      -> 1
recv                    -> 5
select after read       -> 1     <-- still ready
recv again              -> -1 (EAGAIN)
select after empty read -> 0
```

So any code that trusts `select()` and does **one blocking read per readable
event** — which is exactly what OpenDDS's SPDP does, and which is correct
everywhere else — performs one read too many. On this runtime that read never
returns: there is no second thread to deliver a datagram and no signal to
interrupt the wait. The process simply stops.

It presented as a participant that announced itself once and went quiet.
`WASMTIME_LOG=wasmtime_wasi=trace` ended at `poll.pollable.block` inside
`udp.incoming-datagram-stream.receive`; the shim's own trace named the
descriptor (`[wk] recvmsg fd=4 nonblock=0`); and a forty-line reproduction in
the smoke test showed the reactor dispatching `handle_input` for a drained
socket exactly once more.

The fix is one line of ACE with a long comment: datagram sockets are
non-blocking at `open()`. The extra read then returns `EAGAIN` — which every
caller already handles, and which is also what clears the readiness for the
next round. `smoke/ace-smoke.cpp` asserts both halves, so a future wasi-sdk
that changes this behaviour fails a test rather than hanging a node.

### The empty read that killed the transport

The readiness quirk above has a second half, and it is the one that cost the
most: **what the extra empty read is then treated as.**

`RtpsUdpReceiveStrategy::handle_input()` — and the generic
`TransportReceiveStrategy::handle_dds_input()` behind it, and
`Spdp::SpdpTransport::handle_input()` beside it — all end a short read with

```cpp
if (bytes_remaining < 0) { relink(); return -1; }
```

Returning -1 to the reactor does not merely log. `ACE_Select_Reactor_T::notify_handle()`
reads it as "this handler is finished" and **removes the handler**. So the
transport read its first datagram, read again, got `EAGAIN`, and was
unregistered — silently, with no message at any debug level. Everything after
that looked like a network fault: `netstat` showed 1552 bytes sitting unread in
the socket's receive queue while the peer went on sending.

It was slow to find because every layer looked healthy. Discovery worked. The
participants found each other. SEDP's built-in endpoints associated in both
directions. Heartbeats were processed — a single datagram can carry a bundle of
RTPS submessages, so *one* delivered datagram produced five `process_heartbeat_i`
lines and the transport looked alive. The publisher wrote its publication
announcement and the write succeeded. Only a socket-level census
(`WK_DDS_TRACE=1`, which counts every `sendmsg` and `recvmsg` per descriptor)
made it obvious: 23–45 datagrams sent each way, exactly **two** `recvmsg` calls
received — one datagram and one `EAGAIN`.

Three things had to be true at once for this to bite, which is why upstream has
never hit it: the socket must be non-blocking (this port made it so, to fix the
hang), the platform must report a spurious readiness (wasip2 always does), and
the receive path must treat a short read as fatal (OpenDDS does). Take away any
one and it disappears.

## Building

```
mise run fetch          # OpenDDS at the pinned tag
mise run build-host     # native ACE + tao_idl + opendds_idl   (~20 min)
mise run build-target   # wasm32-wasip2                        (long)
```

Start the target build with **`./build-target.sh ace`**, which stops after ACE.
Every wasi-libc gap this port has to close lives in ACE, and finding them there
costs minutes instead of at the end of a TAO build. Same advice, same reason,
as `plugins/libreoffice`'s `./build-lo.sh sal`.

`build-host.sh` narrows `PATH` to `/usr/bin:/bin` and names `/usr/bin/clang++`
explicitly, and that is load-bearing rather than tidiness: mise installs
wasi-sdk as a global tool, so on a developer machine here a bare `clang++` on
`PATH` **is the wasm cross compiler**. OpenDDS's configure auto-detects the
compiler by searching `PATH`, so the first host build silently configured
itself with wasi-sdk's clang and every ACE file died on `'Availability.h' file
not found`.

## Current state

**Two wk nodes exchange DDS samples over the fabric, through a DataWriter and
a DataReader, running real OpenDDS on one thread.**

```
$ wk run example/dds.wk --headless &
$ wk -f example/dds.wk up
$ wk -f example/dds.wk logs dds-pub
wk-dds: 10.0.0.157 -> dds-sub (10.0.0.212:17910), domain 42
dds-publisher: waiting for a subscriber...
dds-publisher: subscriber found, publishing
sent  #0
...
$ wk -f example/dds.wk logs dds-sub
wk-dds: 10.0.0.212 -> dds-pub (10.0.0.157:17910), domain 42
recv  #0  id=1 from=dds-publisher  "hello from a wasm node"
recv  #1  id=1 from=dds-publisher  "hello from a wasm node"
...
```

Neither address appears anywhere in `example/dds.wk`. Each node is told only
its peer's NAME; it resolves that through the fabric's DNS and works out its
own address by asking the stack which one it would use to reach it (see
`nodes/wk_dds_node.h`).

* **M1 — host tools: done.** `tao_idl` 4.0.6 and `opendds_idl` 3.34.0.
* **M2 — ACE for wasm32-wasip2: done.** 307 wasm objects, a 2.0 MB `libACE.a`,
  and `./build-smoke.sh run` passes eighteen checks under
  `wasmtime run -W exceptions`.
* **M3 — TAO and OpenDDS: done.** `make` completes with **zero errors** across
  ACE, all of TAO, and every OpenDDS library — `libOpenDDS_Dcps.a` (25 MB),
  `libOpenDDS_Rtps.a`, `libOpenDDS_Rtps_Udp.a`, and the rest. Upstream's own
  `DevGuideExamples` link and run too: its unmodified Messenger publisher and
  subscriber discover each other and exchange samples on loopback.
* **M4 — the shims: done.**
* **M5/M6 — a participant runs, and two of them discover each other** over
  unicast SPDP, with SEDP associating in both directions.
* **M7 — user endpoints match and samples flow.** DataWriter to DataReader,
  RELIABLE with KEEP_ALL history, so the reliability protocol (heartbeats,
  ACKNACKs, retransmission) is exercised rather than avoided.
* **M8 — the wk nodes: done.** `nodes/publisher.cpp` and `nodes/subscriber.cpp`
  build to `dds-publisher.wasm` and `dds-subscriber.wasm`
  (`./build-nodes.sh`), are registered in `workspace.wk`, and
  `example/dds.wk` wires them to one `Network`. Below the argument parsing
  they are ordinary DDS code — participant, type, topic, writer/reader — which
  is the point.

### What is not done

* **Multicast.** The fabric has none, so RTPS discovery runs unicast: each node
  announces straight to its peer with `SpdpSendAddrs`. That is the supported
  configuration for any network without multicast, but it means each node must
  be told one peer, and a third node would have to be told about the others.
  Carrying real IP multicast in the `Network` hub is the obvious next
  capability and would let stock RTPS discovery work with no peer list at all.
* **A node that computes.** The pump runs when something waits on a condition
  variable. `wk_dds::pump()` is provided for a node's own idle loop, but a node
  that goes away and computes for a second stalls its participant for a second.
  Making `sleep`/`nanosleep` pump would remove the footgun for good.
* **Security, shmem, InfoRepo.** Not built; none is needed for DDS over the
  fabric. `--security` in particular would pull in OpenSSL and Xerces.

### What the smoke test caught that a successful compile did not

Five bugs, all of which would otherwise have surfaced deep inside OpenDDS:

1. **`ACE::max_handles()` returned -1**, because POSIX says a `sysconf()` that
   returns -1 *without setting errno* means "indeterminate", and wasm32-wasip2
   answers exactly that for `_SC_OPEN_MAX` (while answering `_SC_PAGESIZE`,
   `_SC_NPROCESSORS_ONLN` and `_SC_CLK_TCK` normally, so `ACE_LACKS_SYSCONF`
   would have been a lie). ACE handed the -1 to `ACE_Select_Reactor`'s
   constructor, where it became a `size_t` request for four billion handles;
   the open failed, the constructor ran `close()`, and the reactor was left
   **with no timer queue**. The only symptom was `wasm trap: uninitialized
   element` inside `dispatch_timer_handlers` — an indirect call through a
   vtable pointer that was never set, several frames from the cause. Fixed by
   `ace-0002-max-handles-indeterminate.patch`, correct on every platform.
2. **The notification pipe.** `ACE_Select_Reactor` opens one in its
   constructor; `ACE_Pipe` falls back to `::pipe` without `socketpair`, and
   wasi-libc's `pipe` is `ENOSYS`. `ACE_DISABLE_NOTIFY_PIPE_DEFAULT 1` turns it
   off everywhere at once — the correct configuration, not a workaround: the
   pipe exists so another *thread* can wake a reactor blocked in `select()`,
   and there is no other thread.
3. **A timed wait that did not sleep.** The shim's `pthread_cond_timedwait`
   slept only when something was registered to pump, so before a participant
   existed every timed wait returned in zero time — the 100%-CPU idle spin the
   whole design exists to avoid.
4. **`ACE_Handle_Set` never tracked its maximum** on the array path, because
   Win32 (the only previous user) has a `select()` that ignores the width.
   `ACE_Select_Reactor_Handler_Repository::unbind_i()` rebuilds the reactor's
   `max_handlep1_` from six `max_set()` calls, so the *first* handler removal
   would have collapsed the select width to zero and left the reactor deaf for
   good — silently, with no error and no dispatch ever again.
5. **A silent overload trap in ACE's own API**, worth writing down because it
   will bite node code too. `ACE_SOCK_Dgram` has both
   `recv (void *, size_t, ACE_Addr&, int, const ACE_Time_Value *)` and
   `recv (iovec[], int, ACE_Addr&, int, ACE_INET_Addr *to_addr)`, so passing an
   `iovec*` **with a timeout** resolves to the first — reading one byte into
   the iovec array itself. It returns 1 and looks like a shim bug.

And one bug in the smoke test itself, which is worth the sentence: a tight loop
of 200 zero-timeout polls is not a test of "did the reactor dispatch", it is a
race — it finishes in well under a millisecond, which is less than loopback
delivery takes. It failed for that reason, and *unbuffered `printf` between
polls was enough to make it pass*, which is how the race was spotted. The check
now polls against a wall-clock budget.

### Diagnostics

`WK_DDS_TRACE=1` in a node's environment reports what the threadless machinery
is doing: which components registered an inline pass and how many are
registered, how often the pump runs, every condition wait with its deadline
against the clock, and every `recvmsg` with the socket's blocking mode. That
last one is the important one — a blocking `recv` with no data waits forever on
this runtime, so when a node hangs, the last line printed names the descriptor
to look at.

### Known unknowns, in the order they will bite

1. **Discovery**, above.
2. **Static service registration.** No `dlopen`, so nothing is loaded from a
   `.conf`. A node must include OpenDDS's static-link initializers or the
   transport is missing at run time, not at link time.
3. **Idle nodes.** The pump runs when something waits on a condition variable
   or when the node's own loop drives it. A node that goes away and computes
   for a second stalls its own reactor for a second. Upstream's examples happen
   to be fine (they idle in `WaitSet::wait`), but a node written for wk should
   pump explicitly; making `sleep`/`nanosleep` pump is the obvious next step
   and would make any unmodified DDS application behave.
