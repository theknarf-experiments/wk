/* viewer_wk.c — a thin PDF reader main over MuPDF (fitz) and the shared
 * ../gfx-compat wasi-gfx shim. The library port's platform file, like
 * doomgeneric_wk.c is doom's: all rendering is mupdf's, all windowing is
 * wkgfx, nothing of mupdf's own X11/GL viewers is used.
 *
 * argv[1] is the PDF path; with no argv the viewer scans / for the first
 * *.pdf — a bindmounted document lands at /<name> under wk's mount
 * convention, so wiring a PDF file node is all it takes.
 *
 * Controls: PgDn/PgUp or Left/Right arrows turn pages, +/- zooms (anchored
 * at fit-width = zoom 1.0), scroll wheel / Up/Down arrows pan vertically,
 * Home/End jump to the first/last page. The host is resize-authoritative:
 * the surface size is re-read every frame and the page re-rendered when the
 * width changes (fit-width tracks the node's width).
 *
 * Rendering: fz_new_pixmap_from_page_number with fz_device_rgb and alpha=0 —
 * mupdf clears the pixmap to white and hands back tightly packed RGB, which
 * we expand to RGBA (alpha 0xff) while centering onto a white page-sized
 * frame. Pan recomposes from the cached page pixmap without re-rendering;
 * the composed frame is presented every frame (cheap, keeps the compositor
 * simple).
 *
 * A bad or missing PDF prints to stdout and renders a plain error screen (a
 * dark slate field with a light cross) instead of exiting — and keeps
 * re-scanning, so a document wired in later just appears. */

#include <dirent.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <strings.h>

#include <mupdf/fitz.h>

#include "wkgfx.h"

#define ZOOM_STEP 1.25f
#define ZOOM_MIN 0.125f
#define ZOOM_MAX 8.0f
#define PAN_STEP 60
#define SCROLL_LINE 40.0

static fz_context *g_ctx;
static fz_document *g_doc;
static int g_page_count;
static int g_page_no;          /* current page, 0-based */
static float g_zoom = 1.0f;    /* 1.0 == fit the page width to the surface */
static float g_page_w = 612.0f; /* current page size in points */
static float g_page_h = 792.0f;

static fz_pixmap *g_pix;    /* cached render of the current page (RGB) */
static uint8_t *g_frame;    /* composed RGBA frame, surface-sized */
static uint32_t g_fw, g_fh; /* g_frame dims */
static int g_scroll;        /* vertical pan in composed pixels */

static char g_path[512];

/* Scan / for the first *.pdf (alphabetically, for determinism). Returns 1 and
 * fills g_path on a hit. */
static int scan_for_pdf(void) {
    DIR *d = opendir("/");
    if (!d)
        return 0;
    char best[512] = "";
    struct dirent *e;
    while ((e = readdir(d)) != NULL) {
        size_t n = strlen(e->d_name);
        if (n < 4 || strcasecmp(e->d_name + n - 4, ".pdf") != 0)
            continue;
        if (n + 2 > sizeof(best))
            continue;
        if (best[0] == '\0' || strcmp(e->d_name, best) < 0)
            snprintf(best, sizeof(best), "%s", e->d_name);
    }
    closedir(d);
    if (best[0] == '\0')
        return 0;
    snprintf(g_path, sizeof(g_path), "/%s", best);
    return 1;
}

/* Open (or re-open) g_path. Returns 1 on success. */
static int open_doc(void) {
    if (g_doc) {
        fz_drop_document(g_ctx, g_doc);
        g_doc = NULL;
    }
    fz_try(g_ctx) {
        g_doc = fz_open_document(g_ctx, g_path);
        g_page_count = fz_count_pages(g_ctx, g_doc);
    }
    fz_catch(g_ctx) {
        printf("mupdf: cannot open %s: %s\n", g_path, fz_caught_message(g_ctx));
        fflush(stdout);
        if (g_doc) {
            fz_drop_document(g_ctx, g_doc);
            g_doc = NULL;
        }
        return 0;
    }
    if (g_page_count < 1) {
        printf("mupdf: %s has no pages\n", g_path);
        fflush(stdout);
        fz_drop_document(g_ctx, g_doc);
        g_doc = NULL;
        return 0;
    }
    g_page_no = 0;
    g_scroll = 0;
    printf("mupdf: opened %s (%d page%s)\n", g_path, g_page_count,
           g_page_count == 1 ? "" : "s");
    fflush(stdout);
    return 1;
}

/* Render the current page into g_pix at fit-width * g_zoom for surface width
 * `sw`. Returns 1 on success. */
static int render_page(uint32_t sw) {
    if (!g_doc)
        return 0;
    fz_page *page = NULL;
    fz_var(page);
    fz_try(g_ctx) {
        page = fz_load_page(g_ctx, g_doc, g_page_no);
        fz_rect r = fz_bound_page(g_ctx, page);
        g_page_w = r.x1 - r.x0;
        g_page_h = r.y1 - r.y0;
    }
    fz_always(g_ctx) {
        fz_drop_page(g_ctx, page);
    }
    fz_catch(g_ctx) {
        printf("mupdf: cannot load page %d: %s\n", g_page_no + 1,
               fz_caught_message(g_ctx));
        fflush(stdout);
        return 0;
    }
    if (g_page_w < 1.0f)
        g_page_w = 1.0f;
    float scale = ((float)sw / g_page_w) * g_zoom;
    if (scale < 0.02f)
        scale = 0.02f;
    if (scale > 16.0f)
        scale = 16.0f;
    if (g_pix) {
        fz_drop_pixmap(g_ctx, g_pix);
        g_pix = NULL;
    }
    fz_try(g_ctx) {
        /* alpha=0: mupdf clears to white and returns packed RGB. */
        g_pix = fz_new_pixmap_from_page_number(
            g_ctx, g_doc, g_page_no, fz_scale(scale, scale),
            fz_device_rgb(g_ctx), 0);
    }
    fz_catch(g_ctx) {
        printf("mupdf: cannot render page %d: %s\n", g_page_no + 1,
               fz_caught_message(g_ctx));
        fflush(stdout);
        g_pix = NULL;
        return 0;
    }
    return 1;
}

static void ensure_frame(uint32_t w, uint32_t h) {
    if (g_frame && g_fw == w && g_fh == h)
        return;
    free(g_frame);
    g_frame = malloc((size_t)w * h * 4);
    g_fw = w;
    g_fh = h;
}

/* Compose the cached page pixmap (or the error screen) into g_frame. */
static void compose(uint32_t sw, uint32_t sh) {
    ensure_frame(sw, sh);
    if (!g_frame)
        return;

    if (!g_pix) {
        /* Error screen: dark slate with a light cross — plainly not a page. */
        for (uint32_t y = 0; y < sh; y++) {
            for (uint32_t x = 0; x < sw; x++) {
                uint8_t *p = g_frame + ((size_t)y * sw + x) * 4;
                int on_cross = (uint64_t)x * sh > (uint64_t)y * sw
                                   ? (uint64_t)x * sh - (uint64_t)y * sw < 3 * sh
                                   : (uint64_t)y * sw - (uint64_t)x * sh < 3 * sw;
                int on_anti = (uint64_t)(sw - 1 - x) * sh > (uint64_t)y * sw
                                  ? (uint64_t)(sw - 1 - x) * sh - (uint64_t)y * sw < 3 * sh
                                  : (uint64_t)y * sw - (uint64_t)(sw - 1 - x) * sh < 3 * sw;
                if (on_cross || on_anti) {
                    p[0] = p[1] = p[2] = 200;
                } else {
                    p[0] = 40;
                    p[1] = 40;
                    p[2] = 48;
                }
                p[3] = 255;
            }
        }
        return;
    }

    int pw = fz_pixmap_width(g_ctx, g_pix);
    int ph = fz_pixmap_height(g_ctx, g_pix);
    int stride = fz_pixmap_stride(g_ctx, g_pix);
    int ncomp = fz_pixmap_components(g_ctx, g_pix);
    const unsigned char *samples = fz_pixmap_samples(g_ctx, g_pix);

    /* Clamp the pan to the page. */
    int max_scroll = ph > (int)sh ? ph - (int)sh : 0;
    if (g_scroll < 0)
        g_scroll = 0;
    if (g_scroll > max_scroll)
        g_scroll = max_scroll;

    int x0 = ((int)sw - pw) / 2; /* page's left edge on the surface */
    int y0 = ph >= (int)sh ? -g_scroll : ((int)sh - ph) / 2;

    for (uint32_t y = 0; y < sh; y++) {
        uint8_t *row = g_frame + (size_t)y * sw * 4;
        int py = (int)y - y0;
        for (uint32_t x = 0; x < sw; x++) {
            uint8_t *p = row + (size_t)x * 4;
            int px = (int)x - x0;
            if (px >= 0 && px < pw && py >= 0 && py < ph) {
                const unsigned char *s =
                    samples + (size_t)py * stride + (size_t)px * ncomp;
                p[0] = s[0];
                p[1] = ncomp > 1 ? s[1] : s[0];
                p[2] = ncomp > 2 ? s[2] : s[0];
            } else {
                p[0] = p[1] = p[2] = 255; /* white mat around the page */
            }
            p[3] = 255;
        }
    }
}

int main(int argc, char **argv) {
    if (argc > 1)
        snprintf(g_path, sizeof(g_path), "%s", argv[1]);
    else if (!scan_for_pdf())
        printf("mupdf: no *.pdf found in / (wire a PDF file node in)\n");

    g_ctx = fz_new_context(NULL, NULL, FZ_STORE_DEFAULT);
    if (!g_ctx) {
        fprintf(stderr, "mupdf: cannot create fitz context\n");
        return 1;
    }
    fz_try(g_ctx) {
        fz_register_document_handlers(g_ctx);
    }
    fz_catch(g_ctx) {
        fprintf(stderr, "mupdf: cannot register document handlers\n");
        return 1;
    }

    if (g_path[0])
        open_doc();

    if (wkgfx_open(800, 1000) != 0)
        return 1;

    int render_dirty = 1;  /* page pixmap must be re-rendered */
    int compose_dirty = 1; /* g_frame must be re-composed */
    uint32_t last_w = 0;
    uint32_t rescan_tick = 0;

    for (;;) {
        wkgfx_wait_frame();
        uint32_t sw = wkgfx_width(), sh = wkgfx_height();
        if (sw == 0 || sh == 0)
            continue;
        if (sw != last_w) {
            last_w = sw;
            render_dirty = 1; /* fit-width follows the surface width */
        }

        wkgfx_event ev;
        while (wkgfx_poll_event(&ev)) {
            if (ev.type == WKGFX_SCROLL) {
                g_scroll -= (int)(ev.dy * SCROLL_LINE);
                compose_dirty = 1;
                continue;
            }
            if (ev.type == WKGFX_RESIZE) {
                compose_dirty = 1;
                continue;
            }
            if (ev.type != WKGFX_KEY_DOWN)
                continue;
            int prev_page = g_page_no;
            switch (ev.key) {
            case WKGFX_K_PAGE_DOWN:
            case WKGFX_K_ARROW_RIGHT:
                if (g_page_no + 1 < g_page_count)
                    g_page_no++;
                break;
            case WKGFX_K_PAGE_UP:
            case WKGFX_K_ARROW_LEFT:
                if (g_page_no > 0)
                    g_page_no--;
                break;
            case WKGFX_K_HOME:
                g_page_no = 0;
                break;
            case WKGFX_K_END:
                g_page_no = g_page_count > 0 ? g_page_count - 1 : 0;
                break;
            case WKGFX_K_ARROW_DOWN:
                g_scroll += PAN_STEP;
                compose_dirty = 1;
                break;
            case WKGFX_K_ARROW_UP:
                g_scroll -= PAN_STEP;
                compose_dirty = 1;
                break;
            default:
                break;
            }
            /* +/- zoom by key or by typed character (layout-independent). */
            if (ev.key == WKGFX_K_EQUAL || ev.key == WKGFX_K_NUMPAD_ADD ||
                ev.ch == '+') {
                g_zoom *= ZOOM_STEP;
                if (g_zoom > ZOOM_MAX)
                    g_zoom = ZOOM_MAX;
                render_dirty = 1;
            } else if (ev.key == WKGFX_K_MINUS ||
                       ev.key == WKGFX_K_NUMPAD_SUBTRACT || ev.ch == '-') {
                g_zoom /= ZOOM_STEP;
                if (g_zoom < ZOOM_MIN)
                    g_zoom = ZOOM_MIN;
                render_dirty = 1;
            }
            if (g_page_no != prev_page) {
                g_scroll = 0;
                render_dirty = 1;
            }
        }

        /* No document yet: keep rescanning so a wired-in PDF appears. */
        if (!g_doc && ++rescan_tick % 120 == 0) {
            if ((g_path[0] || scan_for_pdf()) && open_doc())
                render_dirty = 1;
        }

        if (g_doc && render_dirty) {
            render_page(sw);
            render_dirty = 0;
            compose_dirty = 1;
        }
        if (compose_dirty || g_fw != sw || g_fh != sh) {
            compose(sw, sh);
            compose_dirty = 0;
        }
        if (g_frame)
            wkgfx_present(g_frame, g_fw, g_fh);
    }
}
