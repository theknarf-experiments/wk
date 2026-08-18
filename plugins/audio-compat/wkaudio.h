/* wkaudio.h — a small C API over wk's wk:webaudio pcm-queue.
 *
 * The audio sibling of ../gfx-compat (wasi-gfx) and ../tty-compat (termios):
 * a C program compiled against this header pushes interleaved f32 PCM to the
 * speakers through wk's Web Audio host, without touching wit-bindgen output.
 * Link wkaudio.c plus the bindings a consumer's build.sh regenerates into
 * gen/ (see plugins/doom/build.sh):
 *
 *     wit-bindgen c --world wkaudio ../audio-compat/wit --out-dir ../audio-compat/gen
 *
 * Model (the SDL_QueueAudio analogue): open once, then keep the queue topped
 * up from the app's own pump —
 *
 *     wkaudio_open(44100.0f, 2);
 *     while (wkaudio_buffered() < 0.100)
 *         wkaudio_write(chunk, frames);       // interleaved f32 in [-1, 1]
 *
 * The host schedules chunks gaplessly on its audio clock and adds a small
 * (~30 ms) lead only when the queue has fully drained.
 */
#ifndef WKAUDIO_SHIM_H
#define WKAUDIO_SHIM_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Create the audio context and a pcm-queue at `sample_rate` Hz with
 * `channels` interleaved channels (1 or 2 — the host traps on anything
 * else), wired to the speakers. Returns 0 on success, -1 if already open.
 * The context and queue live for the guest's lifetime: dropping them would
 * kill the audio graph (documented host behavior), so they are held in
 * statics and never dropped. */
int wkaudio_open(float sample_rate, uint32_t channels);

/* Seconds of audio queued but not yet played (0.0 when drained or not
 * open). */
double wkaudio_buffered(void);

/* Queue `frames` frames of interleaved f32 samples (frames * channels
 * floats, each in [-1, 1]). No-op before wkaudio_open. */
void wkaudio_write(const float *interleaved, uint32_t frames);

#ifdef __cplusplus
}
#endif

#endif /* WKAUDIO_SHIM_H */
