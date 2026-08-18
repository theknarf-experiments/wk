/* fluidsynth_wk.c — a real SoundFont synthesizer as a wk node.
 *
 * The engine is FluidLite (divideconcept/FluidLite, fetched pinned by
 * build.sh): the FluidSynth synthesis core carved out as a dependency-free
 * library. Real FluidSynth itself hard-requires glib + pthreads
 * (CMakeLists.txt: "Mandatory libraries: glib and gthread"), neither of which
 * exists on wasi — FluidLite is the established glib-free fork of the same
 * engine, API-compatible for everything a synth node needs.
 *
 * This file is ours, the way a viewer shell wraps a library port: load the
 * SoundFont named by argv (default /soundfont.sf2, where the Dockerfile puts
 * TimGM6mb), then pump —
 *
 *     drain wkmidi_recv           (wk:midi via ../midi-compat)
 *       -> noteon/noteoff/cc/program-change/pitch-bend into the synth
 *     render interleaved f32 stereo blocks
 *       -> wkaudio_write          (wk:webaudio via ../audio-compat),
 *          keeping ~0.1 s queued ahead of the audio clock
 *
 * Wire a piano node (or a hardware MidiIn node) into this node on the canvas
 * and play.
 *
 * `--dry-run` skips wkaudio entirely — the synth still consumes MIDI and
 * renders, but nothing opens an audio device. That is what the headless e2e
 * test runs (the same reason the doom test boots with -nosound: tests must
 * never open real output devices). Events are logged to stdout either way;
 * a non-surface node's stdout is its terminal, which is the observable.
 */
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <time.h>

#include "fluidlite.h"
#include "wkaudio.h"
#include "wkmidi.h"

#define RATE 44100
#define BLOCK 512 /* frames per render: ~11.6 ms at 44.1 kHz */
#define LEAD 0.100 /* seconds to keep queued ahead of the audio clock */

/* One raw MIDI message into the synth, logged to the node terminal. Running
 * status never reaches us: wk:midi delivers whole messages. */
static void dispatch(fluid_synth_t *synth, const uint8_t *msg, int n)
{
    if (n < 1)
        return;
    int chan = msg[0] & 0x0f;
    switch (msg[0] & 0xf0) {
    case 0x90: /* note-on (velocity 0 is note-off by convention) */
        if (n < 3)
            return;
        if (msg[2] > 0) {
            fluid_synth_noteon(synth, chan, msg[1], msg[2]);
            printf("note-on ch=%d key=%d vel=%d\n", chan, msg[1], msg[2]);
        } else {
            fluid_synth_noteoff(synth, chan, msg[1]);
            printf("note-off ch=%d key=%d\n", chan, msg[1]);
        }
        break;
    case 0x80:
        if (n < 3)
            return;
        fluid_synth_noteoff(synth, chan, msg[1]);
        printf("note-off ch=%d key=%d\n", chan, msg[1]);
        break;
    case 0xb0:
        if (n < 3)
            return;
        fluid_synth_cc(synth, chan, msg[1], msg[2]);
        printf("cc ch=%d ctrl=%d val=%d\n", chan, msg[1], msg[2]);
        break;
    case 0xc0:
        if (n < 2)
            return;
        fluid_synth_program_change(synth, chan, msg[1]);
        printf("program ch=%d prog=%d\n", chan, msg[1]);
        break;
    case 0xe0: /* pitch bend: 14-bit lsb+msb, 0x2000 is center */
        if (n < 3)
            return;
        fluid_synth_pitch_bend(synth, chan, msg[1] | (msg[2] << 7));
        break;
    default:
        break; /* aftertouch, sysex, realtime: not a synth concern here */
    }
    fflush(stdout);
}

static void snooze(long ms)
{
    struct timespec ts = { .tv_sec = 0, .tv_nsec = ms * 1000000L };
    nanosleep(&ts, NULL);
}

int main(int argc, char **argv)
{
    const char *sf2 = "/soundfont.sf2";
    int dry = 0;
    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--dry-run") == 0)
            dry = 1;
        else
            sf2 = argv[i];
    }

    fluid_settings_t *settings = new_fluid_settings();
    fluid_settings_setnum(settings, "synth.sample-rate", (double)RATE);
    fluid_synth_t *synth = new_fluid_synth(settings);
    if (!synth) {
        fprintf(stderr, "fluidsynth: failed to create synth\n");
        return 1;
    }
    if (fluid_synth_sfload(synth, sf2, 1) == -1) {
        fprintf(stderr, "fluidsynth: failed to load soundfont %s\n", sf2);
        return 1;
    }
    printf("soundfont loaded: %s\n", sf2);
    if (dry)
        printf("dry-run: consuming MIDI without an audio device\n");
    fflush(stdout);

    wkmidi_open();
    if (!dry)
        wkaudio_open((float)RATE, 2);

    static float buf[BLOCK * 2]; /* interleaved stereo */
    uint8_t msg[16];
    for (;;) {
        int n;
        while ((n = wkmidi_recv(msg, sizeof msg)) > 0)
            dispatch(synth, msg, n);

        if (dry) {
            /* Render and discard at roughly real-time pace: the synth's
             * voices still run, only the device write is skipped. */
            fluid_synth_write_float(synth, BLOCK, buf, 0, 2, buf, 1, 2);
            snooze(1000L * BLOCK / RATE);
            continue;
        }
        /* Top the queue up to LEAD seconds ahead, then let the audio clock
         * drain it while we nap (~half a block). */
        while (wkaudio_buffered() < LEAD) {
            fluid_synth_write_float(synth, BLOCK, buf, 0, 2, buf, 1, 2);
            wkaudio_write(buf, BLOCK);
        }
        snooze(5);
    }
}
