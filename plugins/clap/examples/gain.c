// A minimal CLAP gain effect: one stereo in/out, one automatable "Gain"
// parameter. Demonstrates the params extension (info / value / text) and
// parameter automation via CLAP_EVENT_PARAM_VALUE in process(). MIT.

#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <clap/clap.h>

static const clap_plugin_descriptor_t s_desc = {
    .clap_version = CLAP_VERSION_INIT,
    .id = "wk.gain",
    .name = "wk Gain",
    .vendor = "wk",
    .url = "https://github.com/theknarf/wk",
    .version = "1.0.0",
    .description = "Stereo gain.",
    .features = (const char *[]){CLAP_PLUGIN_FEATURE_AUDIO_EFFECT, CLAP_PLUGIN_FEATURE_STEREO, NULL},
};

typedef struct {
    clap_plugin_t plugin;
    const clap_host_t *host;
    double gain; // linear, 0..2
} gain_t;

// ---- params ----
static uint32_t params_count(const clap_plugin_t *p) {
    (void)p;
    return 1;
}
static bool params_get_info(const clap_plugin_t *p, uint32_t index, clap_param_info_t *info) {
    (void)p;
    if (index != 0)
        return false;
    memset(info, 0, sizeof(*info));
    info->id = 0;
    info->flags = CLAP_PARAM_IS_AUTOMATABLE;
    info->min_value = 0.0;
    info->max_value = 2.0;
    info->default_value = 1.0;
    snprintf(info->name, sizeof(info->name), "Gain");
    return true;
}
static bool params_get_value(const clap_plugin_t *p, clap_id id, double *out) {
    gain_t *g = p->plugin_data;
    if (id != 0)
        return false;
    *out = g->gain;
    return true;
}
static bool params_value_to_text(const clap_plugin_t *p, clap_id id, double value, char *out,
                                 uint32_t size) {
    (void)p;
    if (id != 0)
        return false;
    snprintf(out, size, "%.2f", value);
    return true;
}
static bool params_text_to_value(const clap_plugin_t *p, clap_id id, const char *text, double *out) {
    (void)p;
    if (id != 0)
        return false;
    *out = atof(text);
    return true;
}
static void params_flush(const clap_plugin_t *p, const clap_input_events_t *in,
                         const clap_output_events_t *out) {
    (void)out;
    gain_t *g = p->plugin_data;
    uint32_t n = in->size(in);
    for (uint32_t i = 0; i < n; i++) {
        const clap_event_header_t *h = in->get(in, i);
        if (h->space_id == CLAP_CORE_EVENT_SPACE_ID && h->type == CLAP_EVENT_PARAM_VALUE) {
            const clap_event_param_value_t *ev = (const clap_event_param_value_t *)h;
            if (ev->param_id == 0)
                g->gain = ev->value;
        }
    }
}
static const clap_plugin_params_t s_params = {
    .count = params_count,
    .get_info = params_get_info,
    .get_value = params_get_value,
    .value_to_text = params_value_to_text,
    .text_to_value = params_text_to_value,
    .flush = params_flush,
};

// ---- audio ports (1 stereo in, 1 stereo out) ----
static uint32_t ap_count(const clap_plugin_t *p, bool is_input) {
    (void)p;
    (void)is_input;
    return 1;
}
static bool ap_get(const clap_plugin_t *p, uint32_t index, bool is_input,
                   clap_audio_port_info_t *info) {
    (void)p;
    (void)is_input;
    if (index != 0)
        return false;
    memset(info, 0, sizeof(*info));
    info->id = 0;
    info->channel_count = 2;
    info->flags = CLAP_AUDIO_PORT_IS_MAIN;
    info->port_type = CLAP_PORT_STEREO;
    info->in_place_pair = CLAP_INVALID_ID;
    snprintf(info->name, sizeof(info->name), "%s", is_input ? "In" : "Out");
    return true;
}
static const clap_plugin_audio_ports_t s_audio_ports = {.count = ap_count, .get = ap_get};

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

static void handle_event(gain_t *g, const clap_event_header_t *h) {
    if (h->space_id == CLAP_CORE_EVENT_SPACE_ID && h->type == CLAP_EVENT_PARAM_VALUE) {
        const clap_event_param_value_t *ev = (const clap_event_param_value_t *)h;
        if (ev->param_id == 0)
            g->gain = ev->value;
    }
}

static clap_process_status plug_process(const clap_plugin_t *p, const clap_process_t *proc) {
    gain_t *g = p->plugin_data;
    const uint32_t nframes = proc->frames_count;
    const uint32_t nev = proc->in_events->size(proc->in_events);
    uint32_t ev = 0, next = nev ? 0 : nframes;

    for (uint32_t i = 0; i < nframes;) {
        while (ev < nev && next == i) {
            const clap_event_header_t *h = proc->in_events->get(proc->in_events, ev);
            if (h->time != i) {
                next = h->time;
                break;
            }
            handle_event(g, h);
            if (++ev == nev)
                next = nframes;
        }
        for (; i < next; ++i) {
            for (uint32_t c = 0; c < proc->audio_outputs[0].channel_count; c++) {
                float in = proc->audio_inputs[0].data32[c][i];
                proc->audio_outputs[0].data32[c][i] = in * (float)g->gain;
            }
        }
    }
    return CLAP_PROCESS_CONTINUE;
}

static const void *plug_get_extension(const clap_plugin_t *p, const char *id) {
    (void)p;
    if (!strcmp(id, CLAP_EXT_AUDIO_PORTS))
        return &s_audio_ports;
    if (!strcmp(id, CLAP_EXT_PARAMS))
        return &s_params;
    return NULL;
}
static void plug_on_main_thread(const clap_plugin_t *p) { (void)p; }

static clap_plugin_t *create(const clap_host_t *host) {
    gain_t *g = calloc(1, sizeof(*g));
    g->host = host;
    g->gain = 1.0;
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
