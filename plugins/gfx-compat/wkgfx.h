/* wkgfx.h — a small C API over wk's wasi-gfx interfaces.
 *
 * The graphics sibling of ../tty-compat (termios) and ../libfuse-compat
 * (FUSE): a C program compiled against this header gets a raw-pixel window
 * from the wk compositor through the standard wasi-gfx interfaces
 * (wasi:surface@0.0.2 + wasi:graphics-context@0.0.1 + wasi:frame-buffer@0.0.1)
 * without touching wit-bindgen output. Link wkgfx.c plus the bindings a
 * consumer's build.sh regenerates into gen/ (see plugins/gfx-smoke/build.sh).
 *
 * Model (same as plugins/paint): the app owns its frame loop —
 *
 *     wkgfx_open(w, h);
 *     for (;;) {
 *         wkgfx_wait_frame();                  // block until the host frame
 *         wkgfx_event ev;
 *         while (wkgfx_poll_event(&ev)) ...;   // drain this frame's input
 *         wkgfx_present(pixels, w, h);         // RGBA, scaled if needed
 *     }
 *
 * Pixels are RGBA8: bytes [r, g, b, a] in memory order, exactly what
 * plugins/paint writes and the compositor reads.
 */
#ifndef WKGFX_SHIM_H
#define WKGFX_SHIM_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef enum {
    WKGFX_NONE = 0,
    WKGFX_KEY_DOWN,
    WKGFX_KEY_UP,
    WKGFX_POINTER_MOVE,
    WKGFX_POINTER_DOWN,
    WKGFX_POINTER_UP,
    WKGFX_SCROLL,
    WKGFX_RESIZE,
} wkgfx_event_type;

/* One merged input event. Only the fields for the event's type are
 * meaningful; the rest are zeroed/-1.
 *
 * `key` and `ch` answer different questions and a guest usually wants both:
 * `key` is WHICH physical key (layout-independent — WASD stays WASD on AZERTY),
 * `ch` is WHAT it types (layout- and shift-aware). Prefer `ch` for text and
 * `key` for controls; see plugins/qt/qpa/qwkkeytranslator.cpp for a full
 * treatment.
 *
 * `ch` is the FIRST scalar of the event's text and the rest is dropped, which
 * costs nothing today (the host sends one character) but loses the tail of the
 * one string winit documents as longer: on Windows an uncombinable dead key
 * produces the accent followed by the letter, so "´e" arrives as U+00B4
 * alone. Widening this to a small buffer is an ABI change and has not been
 * needed yet. */
typedef struct {
    wkgfx_event_type type;
    int32_t key;        /* KEY_*: raw wasi:surface key enum value (WKGFX_K_*), -1 if none */
    uint32_t ch;        /* KEY_*: first unicode scalar of the event's text, 0 if none */
    uint8_t repeat, alt, ctrl, meta, shift; /* KEY_* modifiers/auto-repeat */
    double x, y;        /* POINTER / SCROLL: pointer position, surface-local */
    int32_t button;     /* POINTER_DOWN/UP: 0 left, 1 middle, 2 right, 3 back, 4 forward; -1 none */
    double dx, dy;      /* SCROLL: deltas, in lines */
    uint32_t width, height; /* RESIZE: the new surface size */
} wkgfx_event;

/* Create the surface (the node's window), its graphics context, and the
 * frame-buffer device, and subscribe to frame + input events. `width`/
 * `height` are a request; the host is resize-authoritative — re-read
 * wkgfx_width()/wkgfx_height() every frame. Returns 0 on success, -1 if
 * already open. */
int wkgfx_open(uint32_t width, uint32_t height);

/* Live surface size (the host may have resized us at any time). */
uint32_t wkgfx_width(void);
uint32_t wkgfx_height(void);

/* Block until the host signals the next frame (consumes get-frame). */
void wkgfx_wait_frame(void);

/* Block until the host's next frame OR until `timeout_ns` nanoseconds have
 * elapsed, whichever comes first. `timeout_ns < 0` means "no timeout" and is
 * exactly wkgfx_wait_frame(); `timeout_ns == 0` polls without blocking.
 * Returns 1 if a frame arrived and was consumed, 0 on timeout.
 *
 * For guests that own a real event loop and must also wake on their OWN
 * deadlines — a Qt QTimer, an animation tick — rather than only when the host
 * decides to paint. Built on wasi:io/poll's multi-pollable `poll` plus a
 * wasi:clocks deadline pollable.
 *
 * A guest that must ALSO wait on sockets wants ../gfx-compat/wkgfx_poll.h
 * instead: the frame gets a file descriptor there and joins libc's own
 * poll() list, rather than sockets joining this one. (A wasi:io pollable
 * cannot be extracted from a libc fd — see that header for why.) Do not mix
 * the two on one surface: both consume the same one-shot frame readiness. */
int wkgfx_wait_frame_timeout(int64_t timeout_ns);

/* Consume a pending frame event WITHOUT waiting for one, and return 1 if there
 * was one. This is the second half of wkgfx_wait_frame() for a caller that
 * learned the frame was ready from somebody else's poll — see wkgfx_poll.h.
 *
 * It is not optional bookkeeping: `get-frame` is also where the host reports a
 * CLOSED surface, by trapping, which is how a node exits when its window goes
 * away. A loop that notices frame readiness and never calls this spins forever
 * on a closed surface. */
int wkgfx_take_frame(void);

/* The raw wasi:io/poll pollable handle behind the frame, or 0 before
 * wkgfx_open(). Only for shims that need to hand this pollable to a DIFFERENT
 * poll set than wkgfx's own; ordinary guests never touch it. */
uint32_t wkgfx_frame_pollable(void);

/* Present a whole RGBA frame of `w` x `h` pixels. If (w, h) equals the live
 * surface size this is a direct blit; otherwise the frame is scaled to the
 * surface with nearest-neighbor sampling, preserving aspect ratio with black
 * letterboxing — so a fixed-resolution app (DOOM at 640x400) just presents
 * its own buffer every frame into any node size. */
void wkgfx_present(const uint8_t *rgba, uint32_t w, uint32_t h);

/* Drain one input event into `*ev`. Returns 1 if an event was written, 0 when
 * all queues are empty. Queues are drained in a fixed merge order: resize
 * first, then pointer (move, down, up), then key (down, up), then scroll. */
int wkgfx_poll_event(wkgfx_event *ev);

/* wasi:surface `key` enum values (W3C UIEvents code names), numbered in WIT
 * declaration order — exactly how wit-bindgen c numbers the enum. Generated
 * from the bindings; wkgfx.c static_asserts sentinels against gen/wkgfx.h. */
#define WKGFX_K_BACKQUOTE 0
#define WKGFX_K_BACKSLASH 1
#define WKGFX_K_BRACKET_LEFT 2
#define WKGFX_K_BRACKET_RIGHT 3
#define WKGFX_K_COMMA 4
#define WKGFX_K_DIGIT0 5
#define WKGFX_K_DIGIT1 6
#define WKGFX_K_DIGIT2 7
#define WKGFX_K_DIGIT3 8
#define WKGFX_K_DIGIT4 9
#define WKGFX_K_DIGIT5 10
#define WKGFX_K_DIGIT6 11
#define WKGFX_K_DIGIT7 12
#define WKGFX_K_DIGIT8 13
#define WKGFX_K_DIGIT9 14
#define WKGFX_K_EQUAL 15
#define WKGFX_K_INTL_BACKSLASH 16
#define WKGFX_K_INTL_RO 17
#define WKGFX_K_INTL_YEN 18
#define WKGFX_K_KEY_A 19
#define WKGFX_K_KEY_B 20
#define WKGFX_K_KEY_C 21
#define WKGFX_K_KEY_D 22
#define WKGFX_K_KEY_E 23
#define WKGFX_K_KEY_F 24
#define WKGFX_K_KEY_G 25
#define WKGFX_K_KEY_H 26
#define WKGFX_K_KEY_I 27
#define WKGFX_K_KEY_J 28
#define WKGFX_K_KEY_K 29
#define WKGFX_K_KEY_L 30
#define WKGFX_K_KEY_M 31
#define WKGFX_K_KEY_N 32
#define WKGFX_K_KEY_O 33
#define WKGFX_K_KEY_P 34
#define WKGFX_K_KEY_Q 35
#define WKGFX_K_KEY_R 36
#define WKGFX_K_KEY_S 37
#define WKGFX_K_KEY_T 38
#define WKGFX_K_KEY_U 39
#define WKGFX_K_KEY_V 40
#define WKGFX_K_KEY_W 41
#define WKGFX_K_KEY_X 42
#define WKGFX_K_KEY_Y 43
#define WKGFX_K_KEY_Z 44
#define WKGFX_K_MINUS 45
#define WKGFX_K_PERIOD 46
#define WKGFX_K_QUOTE 47
#define WKGFX_K_SEMICOLON 48
#define WKGFX_K_SLASH 49
#define WKGFX_K_ALT_LEFT 50
#define WKGFX_K_ALT_RIGHT 51
#define WKGFX_K_BACKSPACE 52
#define WKGFX_K_CAPS_LOCK 53
#define WKGFX_K_CONTEXT_MENU 54
#define WKGFX_K_CONTROL_LEFT 55
#define WKGFX_K_CONTROL_RIGHT 56
#define WKGFX_K_ENTER 57
#define WKGFX_K_META_LEFT 58
#define WKGFX_K_META_RIGHT 59
#define WKGFX_K_SHIFT_LEFT 60
#define WKGFX_K_SHIFT_RIGHT 61
#define WKGFX_K_SPACE 62
#define WKGFX_K_TAB 63
#define WKGFX_K_CONVERT 64
#define WKGFX_K_KANA_MODE 65
#define WKGFX_K_LANG1 66
#define WKGFX_K_LANG2 67
#define WKGFX_K_LANG3 68
#define WKGFX_K_LANG4 69
#define WKGFX_K_LANG5 70
#define WKGFX_K_NON_CONVERT 71
#define WKGFX_K_DELETE 72
#define WKGFX_K_END 73
#define WKGFX_K_HELP 74
#define WKGFX_K_HOME 75
#define WKGFX_K_INSERT 76
#define WKGFX_K_PAGE_DOWN 77
#define WKGFX_K_PAGE_UP 78
#define WKGFX_K_ARROW_DOWN 79
#define WKGFX_K_ARROW_LEFT 80
#define WKGFX_K_ARROW_RIGHT 81
#define WKGFX_K_ARROW_UP 82
#define WKGFX_K_NUM_LOCK 83
#define WKGFX_K_NUMPAD0 84
#define WKGFX_K_NUMPAD1 85
#define WKGFX_K_NUMPAD2 86
#define WKGFX_K_NUMPAD3 87
#define WKGFX_K_NUMPAD4 88
#define WKGFX_K_NUMPAD5 89
#define WKGFX_K_NUMPAD6 90
#define WKGFX_K_NUMPAD7 91
#define WKGFX_K_NUMPAD8 92
#define WKGFX_K_NUMPAD9 93
#define WKGFX_K_NUMPAD_ADD 94
#define WKGFX_K_NUMPAD_BACKSPACE 95
#define WKGFX_K_NUMPAD_CLEAR 96
#define WKGFX_K_NUMPAD_CLEAR_ENTRY 97
#define WKGFX_K_NUMPAD_COMMA 98
#define WKGFX_K_NUMPAD_DECIMAL 99
#define WKGFX_K_NUMPAD_DIVIDE 100
#define WKGFX_K_NUMPAD_ENTER 101
#define WKGFX_K_NUMPAD_EQUAL 102
#define WKGFX_K_NUMPAD_HASH 103
#define WKGFX_K_NUMPAD_MEMORY_ADD 104
#define WKGFX_K_NUMPAD_MEMORY_CLEAR 105
#define WKGFX_K_NUMPAD_MEMORY_RECALL 106
#define WKGFX_K_NUMPAD_MEMORY_STORE 107
#define WKGFX_K_NUMPAD_MEMORY_SUBTRACT 108
#define WKGFX_K_NUMPAD_MULTIPLY 109
#define WKGFX_K_NUMPAD_PAREN_LEFT 110
#define WKGFX_K_NUMPAD_PAREN_RIGHT 111
#define WKGFX_K_NUMPAD_STAR 112
#define WKGFX_K_NUMPAD_SUBTRACT 113
#define WKGFX_K_ESCAPE 114
#define WKGFX_K_F1 115
#define WKGFX_K_F2 116
#define WKGFX_K_F3 117
#define WKGFX_K_F4 118
#define WKGFX_K_F5 119
#define WKGFX_K_F6 120
#define WKGFX_K_F7 121
#define WKGFX_K_F8 122
#define WKGFX_K_F9 123
#define WKGFX_K_F10 124
#define WKGFX_K_F11 125
#define WKGFX_K_F12 126
#define WKGFX_K_FN 127
#define WKGFX_K_FN_LOCK 128
#define WKGFX_K_PRINT_SCREEN 129
#define WKGFX_K_SCROLL_LOCK 130
#define WKGFX_K_PAUSE 131
#define WKGFX_K_BROWSER_BACK 132
#define WKGFX_K_BROWSER_FAVORITES 133
#define WKGFX_K_BROWSER_FORWARD 134
#define WKGFX_K_BROWSER_HOME 135
#define WKGFX_K_BROWSER_REFRESH 136
#define WKGFX_K_BROWSER_SEARCH 137
#define WKGFX_K_BROWSER_STOP 138
#define WKGFX_K_EJECT 139
#define WKGFX_K_LAUNCH_APP1 140
#define WKGFX_K_LAUNCH_APP2 141
#define WKGFX_K_LAUNCH_MAIL 142
#define WKGFX_K_MEDIA_PLAY_PAUSE 143
#define WKGFX_K_MEDIA_SELECT 144
#define WKGFX_K_MEDIA_STOP 145
#define WKGFX_K_MEDIA_TRACK_NEXT 146
#define WKGFX_K_MEDIA_TRACK_PREVIOUS 147
#define WKGFX_K_POWER 148
#define WKGFX_K_SLEEP 149
#define WKGFX_K_AUDIO_VOLUME_DOWN 150
#define WKGFX_K_AUDIO_VOLUME_MUTE 151
#define WKGFX_K_AUDIO_VOLUME_UP 152
#define WKGFX_K_WAKE_UP 153
#define WKGFX_K_HYPER 154
#define WKGFX_K_SUPER 155
#define WKGFX_K_TURBO 156
#define WKGFX_K_ABORT 157
#define WKGFX_K_RESUME 158
#define WKGFX_K_SUSPEND 159
#define WKGFX_K_AGAIN 160
#define WKGFX_K_COPY 161
#define WKGFX_K_CUT 162
#define WKGFX_K_FIND 163
#define WKGFX_K_OPEN 164
#define WKGFX_K_PASTE 165
#define WKGFX_K_PROPS 166
#define WKGFX_K_SELECT 167
#define WKGFX_K_UNDO 168
#define WKGFX_K_HIRAGANA 169
#define WKGFX_K_KATAKANA 170

#ifdef __cplusplus
}
#endif

#endif /* WKGFX_SHIM_H */
