/* quakegeneric_wk.c — the wk platform for UNMODIFIED quakegeneric.
 *
 * The only non-upstream game source: quakegeneric's whole platform contract
 * is QG_Init / QG_Quit / QG_DrawFrame / QG_SetPalette / QG_GetKey /
 * QG_GetMouseMove / QG_GetJoyAxes (see quakegeneric.h), and this file maps it
 * onto the shared ../gfx-compat wasi-gfx shim — the Quake sibling of
 * ../doom/doomgeneric_wk.c. build.sh fetches upstream (pinned) and compiles
 * it verbatim.
 *
 * Pixels: Quake's software renderer hands QG_DrawFrame a 320x240 buffer of
 * 8-bit palette indices; QG_SetPalette supplies the 256-entry RGB palette
 * (768 bytes). Each frame is expanded through the palette to the RGBA bytes
 * wasi:frame-buffer expects, and wkgfx scales it into whatever size the node
 * is.
 *
 * Pacing: QG_DrawFrame presents, blocks on the compositor's frame event, and
 * drains that frame's input — key events into a small queue QG_GetKey pops
 * (quakekeys codes: lowercased ascii for normal keys, K_* for the rest, mouse
 * buttons as K_MOUSE1..3, wheel as MWHEELUP/DOWN taps), and pointer motion
 * into accumulated deltas for QG_GetMouseMove (wkgfx reports absolute
 * positions; Quake wants relative motion, so deltas are synthesized from
 * successive positions). Joysticks don't exist here: QG_GetJoyAxes is all
 * zeros. Sound: upstream compiles snd_null.c — quakegeneric has no audio
 * hook, so the port is silent by design.
 */
#include <stdint.h>
#include <string.h>
#include <time.h>

#include "quakegeneric.h"
#include "wkgfx.h"

static uint8_t pal[768];
static uint8_t rgba[QUAKEGENERIC_RES_X * QUAKEGENERIC_RES_Y * 4];

/* Key events translated to quakekeys codes, QG_DrawFrame in / QG_GetKey out. */
#define KEYQ_LEN 64
static struct {
    short key;
    unsigned char down;
} keyq[KEYQ_LEN];
static unsigned keyq_r, keyq_w;

static void keyq_push(int key, int down)
{
    if (keyq_w - keyq_r >= KEYQ_LEN)
        return; /* full: drop, quake will catch up */
    keyq[keyq_w % KEYQ_LEN].key = (short)key;
    keyq[keyq_w % KEYQ_LEN].down = (unsigned char)down;
    keyq_w++;
}

/* Mouse deltas accumulated between QG_GetMouseMove polls. wkgfx pointer
 * events carry absolute surface-local positions; Quake turns relative motion
 * into view angles, so successive positions are differenced. */
static int mouse_dx, mouse_dy;
static double mouse_last_x, mouse_last_y;
static int mouse_tracking;

static void mouse_track(double x, double y)
{
    if (mouse_tracking) {
        mouse_dx += (int)(x - mouse_last_x);
        mouse_dy += (int)(y - mouse_last_y);
    }
    mouse_last_x = x;
    mouse_last_y = y;
    mouse_tracking = 1;
}

/* wasi:surface key (+ text scalar) -> quakekeys code, 0 if unmapped. Mapped
 * by physical key (not text) wherever possible so a key-up always matches
 * its key-down; "normal keys should be passed as lowercased ascii"
 * (quakekeys.h), which is also what the console and menu hotkeys expect. */
static int map_key(const wkgfx_event *ev)
{
    switch (ev->key) {
    case WKGFX_K_ARROW_UP:
        return K_UPARROW;
    case WKGFX_K_ARROW_DOWN:
        return K_DOWNARROW;
    case WKGFX_K_ARROW_LEFT:
        return K_LEFTARROW;
    case WKGFX_K_ARROW_RIGHT:
        return K_RIGHTARROW;
    case WKGFX_K_CONTROL_LEFT:
    case WKGFX_K_CONTROL_RIGHT:
        return K_CTRL;
    case WKGFX_K_SHIFT_LEFT:
    case WKGFX_K_SHIFT_RIGHT:
        return K_SHIFT;
    case WKGFX_K_ALT_LEFT:
    case WKGFX_K_ALT_RIGHT:
        return K_ALT;
    case WKGFX_K_ENTER:
    case WKGFX_K_NUMPAD_ENTER:
        return K_ENTER;
    case WKGFX_K_ESCAPE:
        return K_ESCAPE;
    case WKGFX_K_TAB:
        return K_TAB;
    case WKGFX_K_BACKSPACE:
        return K_BACKSPACE;
    case WKGFX_K_SPACE:
        return K_SPACE;
    case WKGFX_K_INSERT:
        return K_INS;
    case WKGFX_K_DELETE:
        return K_DEL;
    case WKGFX_K_PAGE_UP:
        return K_PGUP;
    case WKGFX_K_PAGE_DOWN:
        return K_PGDN;
    case WKGFX_K_HOME:
        return K_HOME;
    case WKGFX_K_END:
        return K_END;
    case WKGFX_K_PAUSE:
        return K_PAUSE;
    case WKGFX_K_F1:
        return K_F1;
    case WKGFX_K_F2:
        return K_F2;
    case WKGFX_K_F3:
        return K_F3;
    case WKGFX_K_F4:
        return K_F4;
    case WKGFX_K_F5:
        return K_F5;
    case WKGFX_K_F6:
        return K_F6;
    case WKGFX_K_F7:
        return K_F7;
    case WKGFX_K_F8:
        return K_F8;
    case WKGFX_K_F9:
        return K_F9;
    case WKGFX_K_F10:
        return K_F10;
    case WKGFX_K_F11:
        return K_F11;
    case WKGFX_K_F12:
        return K_F12;
    /* Punctuation by position, shift state ignored — the console's key
     * bindings and cheats key off the unshifted character. */
    case WKGFX_K_BACKQUOTE:
        return '`'; /* console toggle */
    case WKGFX_K_MINUS:
        return '-';
    case WKGFX_K_EQUAL:
        return '=';
    case WKGFX_K_BRACKET_LEFT:
        return '[';
    case WKGFX_K_BRACKET_RIGHT:
        return ']';
    case WKGFX_K_BACKSLASH:
        return '\\';
    case WKGFX_K_SEMICOLON:
        return ';';
    case WKGFX_K_QUOTE:
        return '\'';
    case WKGFX_K_COMMA:
        return ',';
    case WKGFX_K_PERIOD:
        return '.';
    case WKGFX_K_SLASH:
        return '/';
    default:
        break;
    }
    if (ev->key >= WKGFX_K_KEY_A && ev->key <= WKGFX_K_KEY_Z)
        return 'a' + (ev->key - WKGFX_K_KEY_A);
    if (ev->key >= WKGFX_K_DIGIT0 && ev->key <= WKGFX_K_DIGIT9)
        return '0' + (ev->key - WKGFX_K_DIGIT0);
    /* Anything else: fall through to the event's text. */
    if (ev->ch >= 'A' && ev->ch <= 'Z')
        return (int)(ev->ch - 'A' + 'a');
    if (ev->ch >= 0x20 && ev->ch < 0x7f)
        return (int)ev->ch;
    return 0;
}

/* wkgfx button (0 left, 1 middle, 2 right) -> quake's virtual mouse keys,
 * numbered as in quakegeneric_sdl2.c: left K_MOUSE1, right K_MOUSE2, middle
 * K_MOUSE3. */
static int map_button(int32_t button)
{
    switch (button) {
    case 0:
        return K_MOUSE1;
    case 1:
        return K_MOUSE3;
    case 2:
        return K_MOUSE2;
    default:
        return 0;
    }
}

void QG_Init(void)
{
    wkgfx_open(QUAKEGENERIC_RES_X, QUAKEGENERIC_RES_Y);
}

void QG_Quit(void)
{
    /* Nothing to tear down: the surface lives until the node exits. */
}

void QG_SetPalette(unsigned char palette[768])
{
    memcpy(pal, palette, sizeof(pal));
}

void QG_DrawFrame(void *pixels)
{
    const uint8_t *src = (const uint8_t *)pixels;
    for (size_t i = 0; i < (size_t)QUAKEGENERIC_RES_X * QUAKEGENERIC_RES_Y; i++) {
        const uint8_t *entry = &pal[(size_t)src[i] * 3];
        rgba[i * 4 + 0] = entry[0];
        rgba[i * 4 + 1] = entry[1];
        rgba[i * 4 + 2] = entry[2];
        rgba[i * 4 + 3] = 0xff;
    }
    wkgfx_present(rgba, QUAKEGENERIC_RES_X, QUAKEGENERIC_RES_Y);
    wkgfx_wait_frame();

    wkgfx_event ev;
    while (wkgfx_poll_event(&ev)) {
        int key;
        switch (ev.type) {
        case WKGFX_KEY_DOWN:
        case WKGFX_KEY_UP:
            key = map_key(&ev);
            if (key)
                keyq_push(key, ev.type == WKGFX_KEY_DOWN);
            break;
        case WKGFX_POINTER_MOVE:
            mouse_track(ev.x, ev.y);
            break;
        case WKGFX_POINTER_DOWN:
        case WKGFX_POINTER_UP:
            mouse_track(ev.x, ev.y);
            key = map_button(ev.button);
            if (key)
                keyq_push(key, ev.type == WKGFX_POINTER_DOWN);
            break;
        case WKGFX_SCROLL:
            if (ev.dy > 0) {
                keyq_push(K_MWHEELUP, 1);
                keyq_push(K_MWHEELUP, 0);
            } else if (ev.dy < 0) {
                keyq_push(K_MWHEELDOWN, 1);
                keyq_push(K_MWHEELDOWN, 0);
            }
            break;
        default:
            break;
        }
    }
}

int QG_GetKey(int *down, int *key)
{
    if (keyq_r == keyq_w)
        return 0;
    *down = keyq[keyq_r % KEYQ_LEN].down;
    *key = keyq[keyq_r % KEYQ_LEN].key;
    keyq_r++;
    return 1;
}

void QG_GetMouseMove(int *x, int *y)
{
    *x = mouse_dx;
    *y = mouse_dy;
    mouse_dx = mouse_dy = 0;
}

void QG_GetJoyAxes(float *axes)
{
    memset(axes, 0, QUAKEGENERIC_JOY_MAX_AXES * sizeof(float));
}

static double now_seconds(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec + (double)ts.tv_nsec / 1e9;
}

int main(int argc, char **argv)
{
    QG_Create(argc, argv);

    double oldtime = now_seconds() - 0.1;
    for (;;) {
        double newtime = now_seconds();
        QG_Tick(newtime - oldtime);
        oldtime = newtime;
        /* Host_FilterTime caps the sim at 72 fps: ticks it rejects return
         * without drawing (so without QG_DrawFrame's blocking wait on the
         * compositor frame) — yield briefly instead of spinning hot. */
        struct timespec ts = {.tv_sec = 0, .tv_nsec = 1000000};
        nanosleep(&ts, NULL);
    }
}
