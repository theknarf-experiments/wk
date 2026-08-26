/* gfx-smoke: a tiny C app exercising the ../gfx-compat wasi-gfx shim, the way
 * hellofuse exercises libfuse-compat. It is a plain main() (a wasi:cli
 * command) that also opens a surface: wk runs commands and provides the gfx
 * interfaces from the same linker, so the window simply appears.
 *
 * It draws an animated color gradient plus a white square that
 *   - tracks the pointer (move / click),
 *   - nudges by 8px per arrow key (key-down events, repeat included),
 *   - grows/shrinks with scroll (the wasi:surface@0.0.2 scroll-event),
 * and flashes on pointer-down — enough observable behavior for the host e2e
 * test to verify that events queued on the VirtualSurface reach C code.
 *
 * It also echoes the last TYPED character back through the top-left pixel
 * (see `typed` below). A key event carries two independent things — which key
 * (`ev.key`) and what it types (`ev.ch`, decoded from the event's text) — and
 * the second one silently reached no guest at all until the host started
 * filling it in; the arrow-key square exercises only the first.
 *
 * The frame is rendered at a fixed 320x240 and pushed with wkgfx_present():
 * if the host resizes the surface, the shim's nearest-neighbor letterboxing
 * path takes over — the DOOM model, smoke-tested.
 */
#include <stdint.h>
#include <string.h>

#include "wkgfx.h"

#define W 320
#define H 240

static uint8_t frame[W * H * 4];

int main(void) {
    if (wkgfx_open(W, H) != 0)
        return 1;

    int32_t sx = W / 2, sy = H / 2; /* square center */
    int32_t half = 12;              /* square half-size */
    uint32_t t = 0;
    uint32_t typed = 0; /* last character typed, 0 until the host sends one */

    for (;;) {
        wkgfx_wait_frame();

        int flash = 0;
        wkgfx_event ev;
        while (wkgfx_poll_event(&ev)) {
            switch (ev.type) {
            case WKGFX_POINTER_MOVE:
            case WKGFX_POINTER_DOWN:
                sx = (int32_t)ev.x;
                sy = (int32_t)ev.y;
                if (ev.type == WKGFX_POINTER_DOWN)
                    flash = 1;
                break;
            case WKGFX_KEY_DOWN:
                if (ev.ch != 0)
                    typed = ev.ch;
                switch (ev.key) {
                case WKGFX_K_ARROW_UP:
                    sy -= 8;
                    break;
                case WKGFX_K_ARROW_DOWN:
                    sy += 8;
                    break;
                case WKGFX_K_ARROW_LEFT:
                    sx -= 8;
                    break;
                case WKGFX_K_ARROW_RIGHT:
                    sx += 8;
                    break;
                default:
                    break;
                }
                break;
            case WKGFX_SCROLL:
                half += (int32_t)(ev.dy * 4.0);
                if (half < 4)
                    half = 4;
                if (half > 100)
                    half = 100;
                break;
            default:
                break;
            }
        }

        for (int32_t y = 0; y < H; y++) {
            for (int32_t x = 0; x < W; x++) {
                uint8_t *p = frame + ((size_t)y * W + x) * 4;
                uint8_t r = (uint8_t)((x * 255) / W);
                uint8_t g = (uint8_t)((y * 255) / H);
                uint8_t b = (uint8_t)((x + y + t) & 0xff);
                int32_t dx = x - sx, dy = y - sy;
                if (dx >= -half && dx <= half && dy >= -half && dy <= half) {
                    r = g = b = 255;
                } else if (flash) {
                    r = (uint8_t)(r / 2 + 128);
                    g = (uint8_t)(g / 2 + 128);
                    b = (uint8_t)(b / 2 + 128);
                }
                p[0] = r;
                p[1] = g;
                p[2] = b;
                p[3] = 255;
            }
        }
        /* The typed character, in the one pixel a printf cannot reach: this is
         * a surface, not a terminal, so pixels are the only channel back to
         * the host test. Low byte in red, high byte in green — the gradient
         * paints (0, 0, t) here, so any non-zero red is ours. */
        if (typed != 0) {
            frame[0] = (uint8_t)(typed & 0xff);
            frame[1] = (uint8_t)((typed >> 8) & 0xff);
            frame[2] = 0;
            frame[3] = 255;
        }

        wkgfx_present(frame, W, H);
        t += 3;
    }
}
