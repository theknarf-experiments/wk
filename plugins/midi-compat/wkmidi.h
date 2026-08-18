/* wkmidi.h — a small C API over wk's wk:midi input port.
 *
 * The MIDI sibling of ../audio-compat (pcm-queue) and ../tty-compat
 * (termios): a C program compiled against this header drains the raw MIDI
 * messages wk routes to this node (from a piano node, a hardware MidiIn node,
 * a sequencer — whatever is wired to it on the canvas), without touching
 * wit-bindgen output. Link wkmidi.c plus the bindings a consumer's build.sh
 * regenerates into gen/ (see plugins/fluidsynth/build.sh):
 *
 *     wit-bindgen c --world wkmidi ../midi-compat/wit --out-dir ../midi-compat/gen
 *
 * Model (poll-based, like the wk:midi interface itself — no pollable): open
 * once, then drain from the app's own pump —
 *
 *     wkmidi_open();
 *     uint8_t msg[8];
 *     int n;
 *     while ((n = wkmidi_recv(msg, sizeof msg)) > 0)
 *         handle(msg, n);                  // e.g. note-on 90 3c 64
 */
#ifndef WKMIDI_SHIM_H
#define WKMIDI_SHIM_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Create the MIDI input resource this node's inbox drains through. Returns 0
 * on success, -1 if already open. The resource lives for the guest's lifetime
 * (held in a static, never dropped) — one node has one MIDI inbox. */
int wkmidi_open(void);

/* Pop the next pending MIDI message into `buf` (raw status + data bytes,
 * e.g. note-on `90 3c 64`). Returns the number of bytes written, 0 when the
 * queue is empty (or before wkmidi_open). A message longer than `cap` is
 * truncated to `cap` bytes. */
int wkmidi_recv(uint8_t *buf, size_t cap);

#ifdef __cplusplus
}
#endif

#endif /* WKMIDI_SHIM_H */
