/* doomgeneric_wk.c — the wk platform for UNMODIFIED doomgeneric.
 *
 * This is the only non-upstream game source: doomgeneric's whole platform
 * contract is DG_Init / DG_DrawFrame / DG_GetKey / DG_GetTicksMs / DG_SleepMs
 * / DG_SetWindowTitle, and this file maps it onto the shared ../gfx-compat
 * wasi-gfx shim — the graphics equivalent of hellofuse mapping libfuse onto
 * wk:fs. build.sh fetches upstream (pinned) and compiles it verbatim.
 *
 * Pixels: DG_ScreenBuffer is 32-bit words the "rgba8888" default mode packs
 * as r<<16 | g<<8 | b (i_video.c cmap_to_fb with red.offset 16, green 8,
 * blue 0) — on little-endian wasm32 that's b,g,r,x in byte order, so each
 * word is swizzled to the RGBA bytes wasi:frame-buffer expects.
 *
 * Pacing: DG_DrawFrame presents the frame, then blocks on the compositor's
 * frame event and drains that frame's input into a small key queue that
 * DG_GetKey pops. Sound stays off (doomgeneric's default: FEATURE_SOUND is
 * never defined).
 */
#include <stdint.h>
#include <time.h>

#include "doomgeneric.h"
#include "doomkeys.h"
#include "wkgfx.h"

static uint8_t rgba[DOOMGENERIC_RESX * DOOMGENERIC_RESY * 4];

/* Key events translated to doomkeys codes, DG_DrawFrame in / DG_GetKey out. */
#define KEYQ_LEN 64
static struct {
    unsigned char key;
    unsigned char pressed;
} keyq[KEYQ_LEN];
static unsigned keyq_r, keyq_w;

static void keyq_push(unsigned char key, int pressed)
{
    if (keyq_w - keyq_r >= KEYQ_LEN)
        return; /* full: drop, doom will catch up */
    keyq[keyq_w % KEYQ_LEN].key = key;
    keyq[keyq_w % KEYQ_LEN].pressed = (unsigned char)pressed;
    keyq_w++;
}

/* wasi:surface key (+ text scalar) -> doomkeys code, 0 if unmapped. */
static unsigned char map_key(const wkgfx_event *ev)
{
    switch (ev->key) {
    case WKGFX_K_ARROW_UP:
        return KEY_UPARROW;
    case WKGFX_K_ARROW_DOWN:
        return KEY_DOWNARROW;
    case WKGFX_K_ARROW_LEFT:
        return KEY_LEFTARROW;
    case WKGFX_K_ARROW_RIGHT:
        return KEY_RIGHTARROW;
    case WKGFX_K_CONTROL_LEFT:
    case WKGFX_K_CONTROL_RIGHT:
        return KEY_FIRE;
    case WKGFX_K_SPACE:
        return KEY_USE;
    case WKGFX_K_ENTER:
    case WKGFX_K_NUMPAD_ENTER:
        return KEY_ENTER;
    case WKGFX_K_ESCAPE:
        return KEY_ESCAPE;
    case WKGFX_K_TAB:
        return KEY_TAB;
    case WKGFX_K_BACKSPACE:
        return KEY_BACKSPACE;
    case WKGFX_K_SHIFT_LEFT:
    case WKGFX_K_SHIFT_RIGHT:
        return KEY_RSHIFT;
    case WKGFX_K_ALT_LEFT:
    case WKGFX_K_ALT_RIGHT:
        return KEY_LALT;
    case WKGFX_K_MINUS:
        return KEY_MINUS;
    case WKGFX_K_EQUAL:
        return KEY_EQUALS;
    case WKGFX_K_F1:
        return KEY_F1;
    case WKGFX_K_F2:
        return KEY_F2;
    case WKGFX_K_F3:
        return KEY_F3;
    case WKGFX_K_F4:
        return KEY_F4;
    case WKGFX_K_F5:
        return KEY_F5;
    case WKGFX_K_F6:
        return KEY_F6;
    case WKGFX_K_F7:
        return KEY_F7;
    case WKGFX_K_F8:
        return KEY_F8;
    case WKGFX_K_F9:
        return KEY_F9;
    case WKGFX_K_F10:
        return KEY_F10;
    case WKGFX_K_F11:
        return KEY_F11;
    case WKGFX_K_F12:
        return KEY_F12;
    default:
        break;
    }
    if (ev->key >= WKGFX_K_DIGIT0 && ev->key <= WKGFX_K_DIGIT9)
        return (unsigned char)('0' + (ev->key - WKGFX_K_DIGIT0));
    /* Letters and the rest: fall through to the event's text — lowercase
     * ASCII is what doom's menu hotkeys and cheat parser expect. */
    if (ev->ch >= 'A' && ev->ch <= 'Z')
        return (unsigned char)(ev->ch - 'A' + 'a');
    if (ev->ch >= 0x20 && ev->ch < 0x7f)
        return (unsigned char)ev->ch;
    return 0;
}

void DG_Init(void)
{
    wkgfx_open(DOOMGENERIC_RESX, DOOMGENERIC_RESY);
}

void DG_DrawFrame(void)
{
    const uint32_t *src = (const uint32_t *)DG_ScreenBuffer;
    for (size_t i = 0; i < (size_t)DOOMGENERIC_RESX * DOOMGENERIC_RESY; i++) {
        uint32_t p = src[i];
        rgba[i * 4 + 0] = (uint8_t)(p >> 16); /* r */
        rgba[i * 4 + 1] = (uint8_t)(p >> 8);  /* g */
        rgba[i * 4 + 2] = (uint8_t)p;         /* b */
        rgba[i * 4 + 3] = 0xff;
    }
    wkgfx_present(rgba, DOOMGENERIC_RESX, DOOMGENERIC_RESY);
    wkgfx_wait_frame();

    wkgfx_event ev;
    while (wkgfx_poll_event(&ev)) {
        if (ev.type != WKGFX_KEY_DOWN && ev.type != WKGFX_KEY_UP)
            continue;
        unsigned char key = map_key(&ev);
        if (key)
            keyq_push(key, ev.type == WKGFX_KEY_DOWN);
    }
}

int DG_GetKey(int *pressed, unsigned char *doomKey)
{
    if (keyq_r == keyq_w)
        return 0;
    *pressed = keyq[keyq_r % KEYQ_LEN].pressed;
    *doomKey = keyq[keyq_r % KEYQ_LEN].key;
    keyq_r++;
    return 1;
}

uint32_t DG_GetTicksMs(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint32_t)(ts.tv_sec * 1000 + ts.tv_nsec / 1000000);
}

void DG_SleepMs(uint32_t ms)
{
    struct timespec ts = {.tv_sec = ms / 1000, .tv_nsec = (long)(ms % 1000) * 1000000};
    nanosleep(&ts, NULL);
}

void DG_SetWindowTitle(const char *title)
{
    (void)title; /* the node's title is the workspace's business */
}

int main(int argc, char **argv)
{
    doomgeneric_Create(argc, argv);
    for (;;)
        doomgeneric_Tick();
}
