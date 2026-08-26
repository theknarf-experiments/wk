/* wkmidiio.h — a C API over wk's wk:midi port, BOTH directions.
 *
 * This is the MIDI half of what plugins/gfx-compat is for pixels: the thin
 * C layer a guest calls so that no C++ in the app ever sees wit-bindgen
 * output. The Qt side of it is the pair of Drumstick RT backends this port
 * adds (library/rt-backends/wk-in and wk-out, from
 * patches/drumstick-0002-wk-rt-backend.patch); nothing else in the component
 * touches wk:midi.
 *
 * RELATIONSHIP TO plugins/midi-compat. That shim is the same idea and the
 * same WIT — this file reads `../midi-compat/wit` and generates its bindings
 * from it, so there is exactly one definition of wk:midi in the tree. What is
 * different is that midi-compat is INPUT ONLY (wkmidi_open + wkmidi_recv);
 * it was written for plugins/fluidsynth, which only ever consumes MIDI. A
 * virtual piano is the first guest that has to SEND, and `wk:midi/midi`'s
 * `resource output { send: func(data: list<u8>) }` has been there all along
 * with no C wrapper over it. So the output half below is new code, not a new
 * capability, and it belongs upstream in plugins/midi-compat as
 * wkmidi_open_out/wkmidi_send once several Qt ports are not being written
 * against that shared directory at the same time. See PORTING.md.
 *
 * MODEL: poll-based, because the interface is. `wk:midi/midi` has no
 * `subscribe: func() -> pollable` — `input.receive()` is a non-blocking pop
 * off a host-side queue (crates/wk-server/src/midi.rs) — so there is no fd
 * and nothing for the Qt event dispatcher's one wasi:io/poll call to wait on.
 * The caller drains from its own pump:
 *
 *     wkmidi_open_in();
 *     wkmidi_open_out();
 *     uint8_t msg[WKMIDI_MAX_MSG];
 *     int n;
 *     while ((n = wkmidi_recv(msg, sizeof msg)) > 0)
 *         parse(msg, n);
 *     uint8_t noteon[3] = { 0x90, 60, 100 };
 *     wkmidi_send(noteon, 3);
 *
 * Both resources are created lazily, held in statics for the guest's
 * lifetime and never dropped: a wk node has exactly one MIDI inbox and one
 * MIDI outbox, and their lifetime is the node's.
 */
#ifndef WKMIDIIO_H
#define WKMIDIIO_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* A comfortable upper bound for one drained message. Channel messages are
 * three bytes; the only thing that can exceed this is a SysEx dump, which
 * `wkmidi_recv` truncates rather than splitting (a split would desynchronise
 * the parser on the far side, which is worse than a dropped dump). */
#define WKMIDI_MAX_MSG 1024

/* Create this node's MIDI input resource — the one its inbox drains through.
 * Returns 0 on success, -1 if already open. Safe to call before anything is
 * wired to the node: messages queue in the host-side inbox either way. */
int wkmidi_open_in(void);

/* Create this node's MIDI output resource. Returns 0 on success, -1 if
 * already open. Sending to an unwired node is not an error — the host router
 * simply has no destinations for it (crates/wk-server/src/midi.rs, Routes::
 * send). */
int wkmidi_open_out(void);

/* Pop the next pending message into `buf` (raw status + data bytes, e.g. a
 * note-on `90 3c 64`). Returns the number of bytes written, or 0 when the
 * queue is empty — and also 0 before wkmidi_open_in(), which is deliberate:
 * a pump that starts early should idle, not fail. A message longer than `cap`
 * is truncated to `cap` bytes. */
int wkmidi_recv(uint8_t *buf, size_t cap);

/* Send one raw MIDI message to whatever this node is wired to. A no-op
 * before wkmidi_open_out(), and a no-op for len == 0. */
void wkmidi_send(const uint8_t *buf, size_t len);

#ifdef __cplusplus
}
#endif

#endif /* WKMIDIIO_H */
