/* wk.c — a libnsfb surface backend for wk's wasi-gfx compositor.
 *
 * The graphics sibling of vim-on-tty-compat: NetSurf's framebuffer frontend
 * plots into libnsfb's software framebuffer, and this backend hands that
 * buffer to the wk compositor through the shared ../gfx-compat shim
 * (wkgfx_open / wkgfx_present / wkgfx_poll_event / wkgfx_wait_frame).
 * build.sh copies this file into libnsfb's src/surface/ beside sdl.c and
 * ram.c; it is modeled on both (ram.c's malloc'd buffer + sdl.c's event
 * translation).
 *
 * Pixel format: the framebuffer is kept in NSFB_FMT_XBGR8888 — the 32-bit
 * word 0xAABBGGRR, whose little-endian byte order r,g,b,a is exactly the
 * RGBA8 layout wasi:frame-buffer expects. geometry() forces this format
 * (NetSurf's frontend asks for XRGB8888 purely as a "bpp 32" proxy), so a
 * present is a straight copy — no per-pixel swizzle, just an alpha fill
 * because the plotters leave the high byte 0.
 *
 * Events: wkgfx's merged queue is translated to nsfb_event_t on demand in
 * input(). One wkgfx event can become several nsfb events (a scroll tick is
 * a MOUSE_4/MOUSE_5 press + release pair, positioned by a preceding
 * move-absolute — the sdl.c wheel convention), so translations land in a
 * small ring the input hook drains first.
 *
 * Pacing: input() with a non-zero timeout blocks on the compositor's next
 * frame and reports NSFB_CONTROL_TIMEOUT when the frame carried no input —
 * exactly what sdl.c does with its wake timer, except our timer is the
 * host's frame clock (~16ms), which keeps NetSurf's scheduler (and its 10ms
 * curl polling) running.
 *
 * Cursor: none. The default no-op cursor hook is correct here — the wk
 * compositor already draws the host pointer over the node.
 */

#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "libnsfb.h"
#include "libnsfb_plot.h"
#include "libnsfb_event.h"

#include "nsfb.h"
#include "surface.h"
#include "plot.h"

#include "wkgfx.h"

#define UNUSED(x) ((x) = (x))

/* Ring of translated-but-undelivered nsfb events. Scrolls fan out to four
 * events per tick, so give it room; overflow drops (input will catch up). */
#define WK_EVQ_LEN 256
static nsfb_event_t wk_evq[WK_EVQ_LEN];
static unsigned wk_evq_r, wk_evq_w;

/* RGBA staging buffer for presents (alpha forced opaque). */
static uint8_t *wk_stage;
static size_t wk_stage_len;

static void wk_evq_push(const nsfb_event_t *ev)
{
    if (wk_evq_w - wk_evq_r >= WK_EVQ_LEN)
        return;
    wk_evq[wk_evq_w % WK_EVQ_LEN] = *ev;
    wk_evq_w++;
}

static bool wk_evq_pop(nsfb_event_t *ev)
{
    if (wk_evq_r == wk_evq_w)
        return false;
    *ev = wk_evq[wk_evq_r % WK_EVQ_LEN];
    wk_evq_r++;
    return true;
}

/* wasi:surface key enum (WKGFX_K_*) -> nsfb keycode, NSFB_KEY_UNKNOWN if
 * unmapped. Unshifted codes: fbtk applies its own shift keymap. */
static enum nsfb_key_code_e wk_map_key(const wkgfx_event *ev)
{
    int32_t k = ev->key;

    if (k >= WKGFX_K_KEY_A && k <= WKGFX_K_KEY_Z)
        return (enum nsfb_key_code_e)(NSFB_KEY_a + (k - WKGFX_K_KEY_A));
    if (k >= WKGFX_K_DIGIT0 && k <= WKGFX_K_DIGIT9)
        return (enum nsfb_key_code_e)(NSFB_KEY_0 + (k - WKGFX_K_DIGIT0));
    if (k >= WKGFX_K_F1 && k <= WKGFX_K_F12)
        return (enum nsfb_key_code_e)(NSFB_KEY_F1 + (k - WKGFX_K_F1));
    if (k >= WKGFX_K_NUMPAD0 && k <= WKGFX_K_NUMPAD9)
        return (enum nsfb_key_code_e)(NSFB_KEY_KP0 + (k - WKGFX_K_NUMPAD0));

    switch (k) {
    case WKGFX_K_BACKQUOTE:      return NSFB_KEY_BACKQUOTE;
    case WKGFX_K_BACKSLASH:      return NSFB_KEY_BACKSLASH;
    case WKGFX_K_BRACKET_LEFT:   return NSFB_KEY_LEFTBRACKET;
    case WKGFX_K_BRACKET_RIGHT:  return NSFB_KEY_RIGHTBRACKET;
    case WKGFX_K_COMMA:          return NSFB_KEY_COMMA;
    case WKGFX_K_EQUAL:          return NSFB_KEY_EQUALS;
    case WKGFX_K_MINUS:          return NSFB_KEY_MINUS;
    case WKGFX_K_PERIOD:         return NSFB_KEY_PERIOD;
    case WKGFX_K_QUOTE:          return NSFB_KEY_QUOTE;
    case WKGFX_K_SEMICOLON:      return NSFB_KEY_SEMICOLON;
    case WKGFX_K_SLASH:          return NSFB_KEY_SLASH;
    case WKGFX_K_SPACE:          return NSFB_KEY_SPACE;
    case WKGFX_K_ENTER:          return NSFB_KEY_RETURN;
    case WKGFX_K_NUMPAD_ENTER:   return NSFB_KEY_KP_ENTER;
    case WKGFX_K_TAB:            return NSFB_KEY_TAB;
    case WKGFX_K_BACKSPACE:      return NSFB_KEY_BACKSPACE;
    case WKGFX_K_ESCAPE:         return NSFB_KEY_ESCAPE;
    case WKGFX_K_DELETE:         return NSFB_KEY_DELETE;
    case WKGFX_K_INSERT:         return NSFB_KEY_INSERT;
    case WKGFX_K_HOME:           return NSFB_KEY_HOME;
    case WKGFX_K_END:            return NSFB_KEY_END;
    case WKGFX_K_PAGE_UP:        return NSFB_KEY_PAGEUP;
    case WKGFX_K_PAGE_DOWN:      return NSFB_KEY_PAGEDOWN;
    case WKGFX_K_ARROW_UP:       return NSFB_KEY_UP;
    case WKGFX_K_ARROW_DOWN:     return NSFB_KEY_DOWN;
    case WKGFX_K_ARROW_LEFT:     return NSFB_KEY_LEFT;
    case WKGFX_K_ARROW_RIGHT:    return NSFB_KEY_RIGHT;
    case WKGFX_K_SHIFT_LEFT:     return NSFB_KEY_LSHIFT;
    case WKGFX_K_SHIFT_RIGHT:    return NSFB_KEY_RSHIFT;
    case WKGFX_K_CONTROL_LEFT:   return NSFB_KEY_LCTRL;
    case WKGFX_K_CONTROL_RIGHT:  return NSFB_KEY_RCTRL;
    case WKGFX_K_ALT_LEFT:       return NSFB_KEY_LALT;
    case WKGFX_K_ALT_RIGHT:      return NSFB_KEY_RALT;
    case WKGFX_K_META_LEFT:      return NSFB_KEY_LMETA;
    case WKGFX_K_META_RIGHT:     return NSFB_KEY_RMETA;
    case WKGFX_K_CAPS_LOCK:      return NSFB_KEY_CAPSLOCK;
    case WKGFX_K_NUM_LOCK:       return NSFB_KEY_NUMLOCK;
    case WKGFX_K_SCROLL_LOCK:    return NSFB_KEY_SCROLLOCK;
    case WKGFX_K_NUMPAD_ADD:      return NSFB_KEY_KP_PLUS;
    case WKGFX_K_NUMPAD_SUBTRACT: return NSFB_KEY_KP_MINUS;
    case WKGFX_K_NUMPAD_MULTIPLY: return NSFB_KEY_KP_MULTIPLY;
    case WKGFX_K_NUMPAD_DIVIDE:   return NSFB_KEY_KP_DIVIDE;
    case WKGFX_K_NUMPAD_DECIMAL:  return NSFB_KEY_KP_PERIOD;
    case WKGFX_K_NUMPAD_EQUAL:    return NSFB_KEY_KP_EQUALS;
    default:
        break;
    }

    /* Unmapped physical key: fall back to the event's text scalar when it
     * is printable ASCII (covers non-US layouts for the common cases). */
    if (ev->ch >= 0x20 && ev->ch < 0x7f)
        return (enum nsfb_key_code_e)ev->ch;
    return NSFB_KEY_UNKNOWN;
}

/* Translate one wkgfx event into zero or more queued nsfb events. */
static void wk_translate(const wkgfx_event *wev)
{
    nsfb_event_t ev;
    memset(&ev, 0, sizeof(ev));

    switch (wev->type) {
    case WKGFX_KEY_DOWN:
    case WKGFX_KEY_UP: {
        enum nsfb_key_code_e code = wk_map_key(wev);
        if (code == NSFB_KEY_UNKNOWN)
            break;
        ev.type = (wev->type == WKGFX_KEY_DOWN) ? NSFB_EVENT_KEY_DOWN
                                                : NSFB_EVENT_KEY_UP;
        ev.value.keycode = code;
        wk_evq_push(&ev);
        break;
    }

    case WKGFX_POINTER_MOVE:
        ev.type = NSFB_EVENT_MOVE_ABSOLUTE;
        ev.value.vector.x = (int)wev->x;
        ev.value.vector.y = (int)wev->y;
        ev.value.vector.z = 0;
        wk_evq_push(&ev);
        break;

    case WKGFX_POINTER_DOWN:
    case WKGFX_POINTER_UP: {
        enum nsfb_key_code_e code;
        switch (wev->button) {
        case 0: code = NSFB_KEY_MOUSE_1; break;
        case 1: code = NSFB_KEY_MOUSE_2; break;
        case 2: code = NSFB_KEY_MOUSE_3; break;
        default: return; /* back/forward buttons: no nsfb equivalent */
        }
        ev.type = (wev->type == WKGFX_POINTER_DOWN) ? NSFB_EVENT_KEY_DOWN
                                                    : NSFB_EVENT_KEY_UP;
        ev.value.keycode = code;
        wk_evq_push(&ev);
        break;
    }

    case WKGFX_SCROLL: {
        /* sdl.c's wheel convention: MOUSE_4 = a tick up, MOUSE_5 = a tick
         * down, delivered as press+release pairs at the pointer position
         * (fbtk dispatches them to the widget under the pointer, so warp
         * there first). */
        double dy = wev->dy;
        enum nsfb_key_code_e code = (dy > 0) ? NSFB_KEY_MOUSE_4 : NSFB_KEY_MOUSE_5;
        int ticks = (int)(dy < 0 ? -dy : dy);
        if (ticks < 1)
            ticks = 1;
        if (ticks > 10)
            ticks = 10;

        ev.type = NSFB_EVENT_MOVE_ABSOLUTE;
        ev.value.vector.x = (int)wev->x;
        ev.value.vector.y = (int)wev->y;
        ev.value.vector.z = 0;
        wk_evq_push(&ev);

        for (int i = 0; i < ticks; i++) {
            ev.type = NSFB_EVENT_KEY_DOWN;
            ev.value.keycode = code;
            wk_evq_push(&ev);
            ev.type = NSFB_EVENT_KEY_UP;
            wk_evq_push(&ev);
        }
        break;
    }

    case WKGFX_RESIZE:
        ev.type = NSFB_EVENT_RESIZE;
        ev.value.resize.w = (int)wev->width;
        ev.value.resize.h = (int)wev->height;
        wk_evq_push(&ev);
        break;

    default:
        break;
    }
}

/* Drain the shim's queues into ours. */
static void wk_pump(void)
{
    wkgfx_event wev;
    while (wkgfx_poll_event(&wev))
        wk_translate(&wev);
}

static int wk_defaults(nsfb_t *nsfb)
{
    nsfb->width = 800;
    nsfb->height = 600;
    nsfb->format = NSFB_FMT_XBGR8888;

    select_plotters(nsfb);

    return 0;
}

static int
wk_set_geometry(nsfb_t *nsfb, int width, int height, enum nsfb_format_e format)
{
    uint8_t *fbptr;

    UNUSED(format); /* always XBGR8888: RGBA bytes, what the compositor eats
                     * (the frontend's XRGB8888 is just a bpp-32 proxy) */

    if (width > 0)
        nsfb->width = width;
    if (height > 0)
        nsfb->height = height;
    nsfb->format = NSFB_FMT_XBGR8888;

    select_plotters(nsfb);

    if (nsfb->ptr != NULL) {
        fbptr = realloc(nsfb->ptr, (size_t)nsfb->width * nsfb->height * 4);
        if (fbptr == NULL)
            return -1;
        nsfb->ptr = fbptr;
    }
    nsfb->linelen = nsfb->width * 4;

    return 0;
}

static int wk_initialise(nsfb_t *nsfb)
{
    uint8_t *fbptr;

    fbptr = realloc(nsfb->ptr, (size_t)nsfb->width * nsfb->height * 4);
    if (fbptr == NULL)
        return -1;
    memset(fbptr, 0xff, (size_t)nsfb->width * nsfb->height * 4);

    nsfb->ptr = fbptr;
    nsfb->linelen = nsfb->width * 4;

    if (wkgfx_open((uint32_t)nsfb->width, (uint32_t)nsfb->height) != 0)
        return -1;

    return 0;
}

static int wk_finalise(nsfb_t *nsfb)
{
    free(nsfb->ptr);
    nsfb->ptr = NULL;
    free(wk_stage);
    wk_stage = NULL;
    wk_stage_len = 0;

    return 0;
}

static int wk_update(nsfb_t *nsfb, nsfb_bbox_t *box)
{
    size_t len = (size_t)nsfb->width * nsfb->height * 4;
    size_t i;

    UNUSED(box); /* whole-frame presents: the compositor scales/letterboxes */

    if (nsfb->ptr == NULL)
        return 0;

    if (wk_stage_len != len) {
        uint8_t *stage = realloc(wk_stage, len);
        if (stage == NULL)
            return -1;
        wk_stage = stage;
        wk_stage_len = len;
    }

    /* The plotters write netsurf colours verbatim (alpha byte 0); force it
     * opaque on the way out. */
    memcpy(wk_stage, nsfb->ptr, len);
    for (i = 3; i < len; i += 4)
        wk_stage[i] = 0xff;

    wkgfx_present(wk_stage, (uint32_t)nsfb->width, (uint32_t)nsfb->height);

    return 0;
}

static bool wk_input(nsfb_t *nsfb, nsfb_event_t *event, int timeout)
{
    UNUSED(nsfb);

    wk_pump();
    if (wk_evq_pop(event))
        return true;

    if (timeout == 0)
        return false;

    /* Block until the compositor's next frame — the host's frame clock is
     * our only timer. A frame with no input is reported as a timeout, which
     * sends the frontend back around its scheduler loop; for timeouts
     * longer than a frame this just means NetSurf re-checks its schedule
     * more often than asked, which is harmless. */
    wkgfx_wait_frame();

    wk_pump();
    if (wk_evq_pop(event))
        return true;

    event->type = NSFB_EVENT_CONTROL;
    event->value.controlcode = NSFB_CONTROL_TIMEOUT;
    return true;
}

static const nsfb_surface_rtns_t wk_rtns = {
    .defaults = wk_defaults,
    .initialise = wk_initialise,
    .finalise = wk_finalise,
    .geometry = wk_set_geometry,
    .input = wk_input,
    .update = wk_update,
    /* claim/cursor/parameters: surface.c defaults (no software cursor — the
     * compositor draws the host pointer). */
};

NSFB_SURFACE_DEF(wk, NSFB_SURFACE_WK, &wk_rtns)

/*
 * Local variables:
 *  c-basic-offset: 4
 *  tab-width: 8
 * End:
 */
