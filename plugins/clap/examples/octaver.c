// A CLAP MIDI effect: for every note it receives, it emits the note plus a copy
// an octave up. Demonstrates a note-in / note-out plugin and CLAP output events
// (the wk host forwards them to downstream nodes). No audio ports. MIT.

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <clap/clap.h>

static const clap_plugin_descriptor_t s_desc = {
    .clap_version = CLAP_VERSION_INIT,
    .id = "wk.octaver",
    .name = "wk Octaver",
    .vendor = "wk",
    .url = "https://github.com/theknarf/wk",
    .version = "1.0.0",
    .description = "Doubles each note an octave up.",
    .features = (const char *[]){CLAP_PLUGIN_FEATURE_NOTE_EFFECT, NULL},
};

typedef struct {
    clap_plugin_t plugin;
    const clap_host_t *host;
} octaver_t;

// ---- note ports: 1 in, 1 out ----
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

// ---- plugin ----
static bool plug_init(const clap_plugin_t *p) {
    (void)p;
    return true;
}
static void plug_destroy(const clap_plugin_t *p) { free(p->plugin_data); }
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
    const clap_output_events_t *out = proc->out_events;
    uint32_t nev = proc->in_events->size(proc->in_events);
    for (uint32_t i = 0; i < nev; i++) {
        const clap_event_header_t *h = proc->in_events->get(proc->in_events, i);
        if (h->space_id != CLAP_CORE_EVENT_SPACE_ID)
            continue;
        if (h->type == CLAP_EVENT_MIDI) {
            const clap_event_midi_t *m = (const clap_event_midi_t *)h;
            out->try_push(out, &m->header);
            uint8_t status = m->data[0] & 0xF0;
            if ((status == 0x90 || status == 0x80) && m->data[1] + 12 <= 127) {
                clap_event_midi_t up = *m;
                up.data[1] += 12;
                out->try_push(out, &up.header);
            }
        } else if (h->type == CLAP_EVENT_NOTE_ON || h->type == CLAP_EVENT_NOTE_OFF ||
                   h->type == CLAP_EVENT_NOTE_CHOKE || h->type == CLAP_EVENT_NOTE_END) {
            const clap_event_note_t *n = (const clap_event_note_t *)h;
            out->try_push(out, &n->header);
            if (n->key + 12 <= 127) {
                clap_event_note_t up = *n;
                up.key += 12;
                out->try_push(out, &up.header);
            }
        }
    }
    return CLAP_PROCESS_CONTINUE;
}

static const void *plug_get_extension(const clap_plugin_t *p, const char *id) {
    (void)p;
    if (!strcmp(id, CLAP_EXT_NOTE_PORTS))
        return &s_note_ports;
    return NULL;
}
static void plug_on_main_thread(const clap_plugin_t *p) { (void)p; }

static clap_plugin_t *create(const clap_host_t *host) {
    octaver_t *o = calloc(1, sizeof(*o));
    o->host = host;
    o->plugin.desc = &s_desc;
    o->plugin.plugin_data = o;
    o->plugin.init = plug_init;
    o->plugin.destroy = plug_destroy;
    o->plugin.activate = plug_activate;
    o->plugin.deactivate = plug_deactivate;
    o->plugin.start_processing = plug_start;
    o->plugin.stop_processing = plug_stop;
    o->plugin.reset = plug_reset;
    o->plugin.process = plug_process;
    o->plugin.get_extension = plug_get_extension;
    o->plugin.on_main_thread = plug_on_main_thread;
    return &o->plugin;
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
