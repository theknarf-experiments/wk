// A stereo feedback delay (echo) CLAP audio effect: one stereo in/out, with
// time / feedback / mix parameters. Demonstrates a CLAP *audio effect* — it
// reads its audio input and transforms it — so it only makes sound when wired
// downstream of an audio source (e.g. a CLAP synth). MIT.

#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <clap/clap.h>

enum { P_TIME, P_FEEDBACK, P_MIX, NPARAMS };

typedef struct {
    clap_plugin_t plugin;
    const clap_host_t *host;
    double sr;
    double p[NPARAMS];
    float *line[2]; // per-channel circular delay buffer
    uint32_t len;   // buffer length in samples
    uint32_t widx;  // write cursor
} delay_t;

typedef struct {
    const char *name;
    double min, max, def;
} pdesc_t;
static const pdesc_t PD[NPARAMS] = {
    [P_TIME] = {"Time", 1.0, 2000.0, 300.0}, // milliseconds
    [P_FEEDBACK] = {"Feedback", 0.0, 0.95, 0.4},
    [P_MIX] = {"Mix", 0.0, 1.0, 0.35},
};

static const clap_plugin_descriptor_t s_desc = {
    .clap_version = CLAP_VERSION_INIT,
    .id = "wk.delay",
    .name = "wk Delay",
    .vendor = "wk",
    .url = "https://github.com/theknarf/wk",
    .version = "1.0.0",
    .description = "Stereo feedback delay.",
    .features = (const char *[]){CLAP_PLUGIN_FEATURE_AUDIO_EFFECT, CLAP_PLUGIN_FEATURE_STEREO, NULL},
};

// ---- params ----
static uint32_t params_count(const clap_plugin_t *p) {
    (void)p;
    return NPARAMS;
}
static bool params_get_info(const clap_plugin_t *p, uint32_t index, clap_param_info_t *info) {
    (void)p;
    if (index >= NPARAMS)
        return false;
    memset(info, 0, sizeof(*info));
    info->id = index;
    info->flags = CLAP_PARAM_IS_AUTOMATABLE;
    info->min_value = PD[index].min;
    info->max_value = PD[index].max;
    info->default_value = PD[index].def;
    snprintf(info->name, sizeof(info->name), "%s", PD[index].name);
    return true;
}
static bool params_get_value(const clap_plugin_t *p, clap_id id, double *out) {
    delay_t *d = p->plugin_data;
    if (id >= NPARAMS)
        return false;
    *out = d->p[id];
    return true;
}
static bool params_value_to_text(const clap_plugin_t *p, clap_id id, double v, char *out,
                                 uint32_t size) {
    (void)p;
    if (id >= NPARAMS)
        return false;
    snprintf(out, size, "%.2f", v);
    return true;
}
static bool params_text_to_value(const clap_plugin_t *p, clap_id id, const char *text, double *out) {
    (void)p;
    if (id >= NPARAMS)
        return false;
    *out = atof(text);
    return true;
}
static void set_param(delay_t *d, uint32_t id, double v) {
    if (id < NPARAMS)
        d->p[id] = v < PD[id].min ? PD[id].min : (v > PD[id].max ? PD[id].max : v);
}
static void params_flush(const clap_plugin_t *p, const clap_input_events_t *in,
                         const clap_output_events_t *out) {
    (void)out;
    delay_t *d = p->plugin_data;
    uint32_t n = in->size(in);
    for (uint32_t i = 0; i < n; i++) {
        const clap_event_header_t *h = in->get(in, i);
        if (h->space_id == CLAP_CORE_EVENT_SPACE_ID && h->type == CLAP_EVENT_PARAM_VALUE) {
            const clap_event_param_value_t *e = (const clap_event_param_value_t *)h;
            set_param(d, e->param_id, e->value);
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
static void free_lines(delay_t *d) {
    free(d->line[0]);
    free(d->line[1]);
    d->line[0] = d->line[1] = NULL;
}
static void plug_destroy(const clap_plugin_t *p) {
    delay_t *d = p->plugin_data;
    free_lines(d);
    free(d);
}
static bool plug_activate(const clap_plugin_t *p, double sr, uint32_t mn, uint32_t mx) {
    (void)mn;
    (void)mx;
    delay_t *d = p->plugin_data;
    d->sr = sr;
    free_lines(d);
    d->len = (uint32_t)(sr * 2.0) + 1; // up to 2s of delay
    d->line[0] = calloc(d->len, sizeof(float));
    d->line[1] = calloc(d->len, sizeof(float));
    d->widx = 0;
    return d->line[0] && d->line[1];
}
static void plug_deactivate(const clap_plugin_t *p) { free_lines((delay_t *)p->plugin_data); }
static bool plug_start(const clap_plugin_t *p) {
    (void)p;
    return true;
}
static void plug_stop(const clap_plugin_t *p) { (void)p; }
static void plug_reset(const clap_plugin_t *p) {
    delay_t *d = p->plugin_data;
    if (d->line[0])
        memset(d->line[0], 0, d->len * sizeof(float));
    if (d->line[1])
        memset(d->line[1], 0, d->len * sizeof(float));
    d->widx = 0;
}

static void handle_event(delay_t *d, const clap_event_header_t *h) {
    if (h->space_id == CLAP_CORE_EVENT_SPACE_ID && h->type == CLAP_EVENT_PARAM_VALUE) {
        const clap_event_param_value_t *e = (const clap_event_param_value_t *)h;
        set_param(d, e->param_id, e->value);
    }
}

static clap_process_status plug_process(const clap_plugin_t *p, const clap_process_t *proc) {
    delay_t *d = p->plugin_data;
    if (!d->line[0] || proc->audio_inputs_count == 0 || proc->audio_outputs_count == 0)
        return CLAP_PROCESS_CONTINUE;
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
            handle_event(d, h);
            if (++ev == nev)
                next = nframes;
        }
        // Read params per segment so mid-block automation takes effect.
        uint32_t delay = (uint32_t)(d->p[P_TIME] * 0.001 * d->sr);
        if (delay < 1)
            delay = 1;
        if (delay >= d->len)
            delay = d->len - 1;
        const float fb = (float)d->p[P_FEEDBACK], mix = (float)d->p[P_MIX];
        for (; i < next; ++i) {
            uint32_t r = (d->widx + d->len - delay) % d->len;
            for (uint32_t c = 0; c < 2; c++) {
                float in = proc->audio_inputs[0].data32[c][i];
                float wet = d->line[c][r];
                d->line[c][d->widx] = in + wet * fb;
                float out = in * (1.0f - mix) + wet * mix;
                if (!(out == out))
                    out = 0.0f;
                proc->audio_outputs[0].data32[c][i] = out;
            }
            d->widx = (d->widx + 1) % d->len;
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
    delay_t *d = calloc(1, sizeof(*d));
    d->host = host;
    d->sr = 48000.0;
    for (int i = 0; i < NPARAMS; i++)
        d->p[i] = PD[i].def;
    d->plugin.desc = &s_desc;
    d->plugin.plugin_data = d;
    d->plugin.init = plug_init;
    d->plugin.destroy = plug_destroy;
    d->plugin.activate = plug_activate;
    d->plugin.deactivate = plug_deactivate;
    d->plugin.start_processing = plug_start;
    d->plugin.stop_processing = plug_stop;
    d->plugin.reset = plug_reset;
    d->plugin.process = plug_process;
    d->plugin.get_extension = plug_get_extension;
    d->plugin.on_main_thread = plug_on_main_thread;
    return &d->plugin;
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
