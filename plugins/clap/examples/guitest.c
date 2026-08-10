// A minimal CLAP plugin with a wk GUI: it draws to a wasi:surface (composited by
// wk like any other node) and toggles its colour when clicked. Proves the wk.gui
// path — surface creation, per-frame host-driven rendering, and surface input —
// for a CLAP plugin running on the unified engine. It also passes notes through
// (note-in → note-out), so it wires like any MIDI node. MIT.

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <clap/clap.h>

#include "gen/plugin.h" // wasi:surface / graphics-context / frame-buffer bindings
#include "wk_gui.h"

typedef struct {
    clap_plugin_t plugin;
    const clap_host_t *host;
    // GUI state.
    bool up;
    wasi_surface_surface_own_surface_t surface;
    wasi_graphics_context_graphics_context_own_context_t ctx;
    wasi_frame_buffer_frame_buffer_own_device_t dev;
    uint8_t *px;
    size_t px_cap;
    unsigned frame;
    bool pressed;
} gt_t;

static const clap_plugin_descriptor_t s_desc = {
    .clap_version = CLAP_VERSION_INIT,
    .id = "wk.guitest",
    .name = "wk GUI Test",
    .vendor = "wk",
    .url = "https://github.com/theknarf/wk",
    .version = "1.0.0",
    .description = "A CLAP plugin with a wk GUI.",
    .features = (const char *[]){CLAP_PLUGIN_FEATURE_NOTE_EFFECT, NULL},
};

// ---- note ports (in + out, passthrough) ----
static uint32_t np_count(const clap_plugin_t *p, bool is_input) {
    (void)p;
    (void)is_input;
    return 1;
}
static bool np_get(const clap_plugin_t *p, uint32_t index, bool is_input,
                   clap_note_port_info_t *info) {
    (void)p;
    if (index != 0)
        return false;
    memset(info, 0, sizeof(*info));
    info->id = 0;
    info->supported_dialects = CLAP_NOTE_DIALECT_CLAP | CLAP_NOTE_DIALECT_MIDI;
    info->preferred_dialect = CLAP_NOTE_DIALECT_MIDI;
    snprintf(info->name, sizeof(info->name), "%s", is_input ? "In" : "Out");
    return true;
}
static const clap_plugin_note_ports_t s_note_ports = {.count = np_count, .get = np_get};

// ---- wk.gui ----
static bool gui_create(const clap_plugin_t *p) {
    gt_t *g = p->plugin_data;
    wasi_surface_surface_create_desc_t desc = {.height = {true, 180}, .width = {true, 320}};
    g->surface = wasi_surface_surface_constructor_surface(&desc);
    g->ctx = wasi_graphics_context_graphics_context_constructor_context();
    wasi_surface_surface_method_surface_connect_graphics_context(
        wasi_surface_surface_borrow_surface(g->surface),
        wasi_graphics_context_graphics_context_borrow_context(g->ctx));
    g->dev = wasi_frame_buffer_frame_buffer_constructor_device();
    wasi_frame_buffer_frame_buffer_method_device_connect_graphics_context(
        wasi_frame_buffer_frame_buffer_borrow_device(g->dev),
        wasi_graphics_context_graphics_context_borrow_context(g->ctx));
    g->up = true;
    return true;
}

static void gui_render(const clap_plugin_t *p) {
    gt_t *g = p->plugin_data;
    if (!g->up)
        return;
    wasi_surface_surface_borrow_surface_t s = wasi_surface_surface_borrow_surface(g->surface);
    // Consume the frame event once (it also traps if the host closed the surface).
    wasi_surface_surface_frame_event_t fe;
    wasi_surface_surface_method_surface_get_frame(s, &fe);
    wasi_surface_surface_pointer_event_t pe;
    while (wasi_surface_surface_method_surface_get_pointer_down(s, &pe))
        g->pressed = !g->pressed;
    while (wasi_surface_surface_method_surface_get_pointer_up(s, &pe)) {
    }
    while (wasi_surface_surface_method_surface_get_pointer_move(s, &pe)) {
    }

    uint32_t w = wasi_surface_surface_method_surface_width(s);
    uint32_t h = wasi_surface_surface_method_surface_height(s);
    if (w == 0 || h == 0)
        return;
    size_t n = (size_t)w * h * 4;
    if (n > g->px_cap) {
        g->px = realloc(g->px, n);
        g->px_cap = n;
    }
    g->frame++;
    uint8_t r = g->pressed ? 210 : 40;
    uint8_t gg = (uint8_t)(60 + (g->frame & 63));
    for (size_t i = 0; i < n; i += 4) {
        g->px[i] = r;
        g->px[i + 1] = gg;
        g->px[i + 2] = 130;
        g->px[i + 3] = 255;
    }

    wasi_graphics_context_graphics_context_borrow_context_t cx =
        wasi_graphics_context_graphics_context_borrow_context(g->ctx);
    wasi_graphics_context_graphics_context_own_abstract_buffer_t ab =
        wasi_graphics_context_graphics_context_method_context_get_current_buffer(cx);
    wasi_frame_buffer_frame_buffer_own_buffer_t buf =
        wasi_frame_buffer_frame_buffer_static_buffer_from_graphics_buffer(ab);
    plugin_list_u8_t pl = {g->px, n};
    wasi_frame_buffer_frame_buffer_method_buffer_set(
        wasi_frame_buffer_frame_buffer_borrow_buffer(buf), &pl);
    wasi_frame_buffer_frame_buffer_buffer_drop_own(buf);
    wasi_graphics_context_graphics_context_method_context_present(cx);
}
static const wk_gui_t s_gui = {.create = gui_create, .render = gui_render};

// ---- plugin ----
static bool plug_init(const clap_plugin_t *p) {
    (void)p;
    return true;
}
static void plug_destroy(const clap_plugin_t *p) {
    gt_t *g = p->plugin_data;
    free(g->px);
    free(g);
}
static bool plug_activate(const clap_plugin_t *p, double sr, uint32_t mn, uint32_t mx) {
    (void)p;
    (void)sr;
    (void)mn;
    (void)mx;
    return true;
}
static void plug_deactivate(const clap_plugin_t *p) { (void)p; }
static bool plug_start(const clap_plugin_t *p) {
    (void)p;
    return true;
}
static void plug_stop(const clap_plugin_t *p) { (void)p; }
static void plug_reset(const clap_plugin_t *p) { (void)p; }

static clap_process_status plug_process(const clap_plugin_t *p, const clap_process_t *proc) {
    (void)p;
    // Pass notes straight through.
    const clap_output_events_t *out = proc->out_events;
    uint32_t nev = proc->in_events->size(proc->in_events);
    for (uint32_t i = 0; i < nev; i++)
        out->try_push(out, proc->in_events->get(proc->in_events, i));
    return CLAP_PROCESS_CONTINUE;
}

static const void *plug_get_extension(const clap_plugin_t *p, const char *id) {
    (void)p;
    if (!strcmp(id, CLAP_EXT_NOTE_PORTS))
        return &s_note_ports;
    if (!strcmp(id, WK_EXT_GUI))
        return &s_gui;
    return NULL;
}
static void plug_on_main_thread(const clap_plugin_t *p) { (void)p; }

static clap_plugin_t *create(const clap_host_t *host) {
    gt_t *g = calloc(1, sizeof(*g));
    g->host = host;
    g->plugin.desc = &s_desc;
    g->plugin.plugin_data = g;
    g->plugin.init = plug_init;
    g->plugin.destroy = plug_destroy;
    g->plugin.activate = plug_activate;
    g->plugin.deactivate = plug_deactivate;
    g->plugin.start_processing = plug_start;
    g->plugin.stop_processing = plug_stop;
    g->plugin.reset = plug_reset;
    g->plugin.process = plug_process;
    g->plugin.get_extension = plug_get_extension;
    g->plugin.on_main_thread = plug_on_main_thread;
    return &g->plugin;
}

// ---- factory + entry ----
static uint32_t factory_count(const clap_plugin_factory_t *f) {
    (void)f;
    return 1;
}
static const clap_plugin_descriptor_t *factory_desc(const clap_plugin_factory_t *f, uint32_t i) {
    (void)f;
    return i == 0 ? &s_desc : NULL;
}
static const clap_plugin_t *factory_create(const clap_plugin_factory_t *f, const clap_host_t *host,
                                           const char *id) {
    (void)f;
    if (!clap_version_is_compatible(host->clap_version) || strcmp(id, s_desc.id))
        return NULL;
    return create(host);
}
static const clap_plugin_factory_t s_factory = {
    .get_plugin_count = factory_count,
    .get_plugin_descriptor = factory_desc,
    .create_plugin = factory_create,
};

static bool entry_init(const char *path) {
    (void)path;
    return true;
}
static void entry_deinit(void) {}
static const void *entry_get_factory(const char *id) {
    return strcmp(id, CLAP_PLUGIN_FACTORY_ID) ? NULL : &s_factory;
}
CLAP_EXPORT const clap_plugin_entry_t clap_entry = {
    .clap_version = CLAP_VERSION_INIT,
    .init = entry_init,
    .deinit = entry_deinit,
    .get_factory = entry_get_factory,
};
