// A small polyphonic sine synth in CLAP. One CLAP note input, one stereo audio
// output, one "Gain" parameter. Each note-on starts a voice with a short
// attack; note-off releases it. Demonstrates the instrument path (note-ports +
// audio-ports + params) end to end. MIT.

#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <clap/clap.h>

#define NVOICES 16
#ifndef M_PI
#define M_PI 3.14159265358979323846
#endif

enum { ATTACK, SUSTAIN, RELEASE };

typedef struct {
    bool used;
    int32_t note_id; // -1 if unspecified
    int16_t key;
    double phase; // 0..1
    double freq;  // Hz
    double env;   // 0..1
    int stage;
} voice_t;

typedef struct {
    clap_plugin_t plugin;
    const clap_host_t *host;
    double sr;
    double gain; // 0..1
    voice_t voices[NVOICES];
} synth_t;

static const clap_plugin_descriptor_t s_desc = {
    .clap_version = CLAP_VERSION_INIT,
    .id = "wk.polysynth",
    .name = "wk PolySynth",
    .vendor = "wk",
    .url = "https://github.com/theknarf/wk",
    .version = "1.0.0",
    .description = "Polyphonic sine synth.",
    .features = (const char *[]){CLAP_PLUGIN_FEATURE_INSTRUMENT, CLAP_PLUGIN_FEATURE_SYNTHESIZER,
                                 CLAP_PLUGIN_FEATURE_STEREO, NULL},
};

// ---- params: Gain ----
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
    info->max_value = 1.0;
    info->default_value = 0.3;
    snprintf(info->name, sizeof(info->name), "Gain");
    return true;
}
static bool params_get_value(const clap_plugin_t *p, clap_id id, double *out) {
    synth_t *s = p->plugin_data;
    if (id != 0)
        return false;
    *out = s->gain;
    return true;
}
static bool params_value_to_text(const clap_plugin_t *p, clap_id id, double v, char *out,
                                 uint32_t size) {
    (void)p;
    if (id != 0)
        return false;
    snprintf(out, size, "%.2f", v);
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
    synth_t *s = p->plugin_data;
    uint32_t n = in->size(in);
    for (uint32_t i = 0; i < n; i++) {
        const clap_event_header_t *h = in->get(in, i);
        if (h->space_id == CLAP_CORE_EVENT_SPACE_ID && h->type == CLAP_EVENT_PARAM_VALUE) {
            const clap_event_param_value_t *ev = (const clap_event_param_value_t *)h;
            if (ev->param_id == 0)
                s->gain = ev->value;
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

// ---- note ports: 1 CLAP input ----
static uint32_t np_count(const clap_plugin_t *p, bool is_input) {
    (void)p;
    return is_input ? 1 : 0;
}
static bool np_get(const clap_plugin_t *p, uint32_t index, bool is_input,
                   clap_note_port_info_t *info) {
    (void)p;
    if (!is_input || index != 0)
        return false;
    memset(info, 0, sizeof(*info));
    info->id = 0;
    info->supported_dialects = CLAP_NOTE_DIALECT_CLAP | CLAP_NOTE_DIALECT_MIDI;
    info->preferred_dialect = CLAP_NOTE_DIALECT_CLAP;
    snprintf(info->name, sizeof(info->name), "Notes");
    return true;
}
static const clap_plugin_note_ports_t s_note_ports = {.count = np_count, .get = np_get};

// ---- audio ports: 1 stereo output ----
static uint32_t ap_count(const clap_plugin_t *p, bool is_input) {
    (void)p;
    return is_input ? 0 : 1;
}
static bool ap_get(const clap_plugin_t *p, uint32_t index, bool is_input,
                   clap_audio_port_info_t *info) {
    (void)p;
    if (is_input || index != 0)
        return false;
    memset(info, 0, sizeof(*info));
    info->id = 0;
    info->channel_count = 2;
    info->flags = CLAP_AUDIO_PORT_IS_MAIN;
    info->port_type = CLAP_PORT_STEREO;
    info->in_place_pair = CLAP_INVALID_ID;
    snprintf(info->name, sizeof(info->name), "Out");
    return true;
}
static const clap_plugin_audio_ports_t s_audio_ports = {.count = ap_count, .get = ap_get};

// ---- voice helpers ----
static void note_on(synth_t *s, int16_t key, int32_t note_id) {
    voice_t *v = NULL;
    for (int i = 0; i < NVOICES; i++)
        if (!s->voices[i].used) {
            v = &s->voices[i];
            break;
        }
    if (!v)
        v = &s->voices[0]; // steal
    v->used = true;
    v->key = key;
    v->note_id = note_id;
    v->phase = 0.0;
    v->freq = 440.0 * pow(2.0, (key - 69) / 12.0);
    v->env = 0.0;
    v->stage = ATTACK;
}
static void note_off(synth_t *s, int16_t key, int32_t note_id) {
    for (int i = 0; i < NVOICES; i++) {
        voice_t *v = &s->voices[i];
        if (v->used && (key < 0 || v->key == key) &&
            (note_id < 0 || v->note_id < 0 || v->note_id == note_id))
            v->stage = RELEASE;
    }
}
static void note_choke(synth_t *s, int16_t key, int32_t note_id) {
    for (int i = 0; i < NVOICES; i++) {
        voice_t *v = &s->voices[i];
        if (v->used && (key < 0 || v->key == key) &&
            (note_id < 0 || v->note_id < 0 || v->note_id == note_id))
            v->used = false;
    }
}

static void handle_event(synth_t *s, const clap_event_header_t *h) {
    if (h->space_id != CLAP_CORE_EVENT_SPACE_ID)
        return;
    switch (h->type) {
    case CLAP_EVENT_NOTE_ON: {
        const clap_event_note_t *e = (const clap_event_note_t *)h;
        note_on(s, e->key, e->note_id);
        break;
    }
    case CLAP_EVENT_NOTE_OFF: {
        const clap_event_note_t *e = (const clap_event_note_t *)h;
        note_off(s, e->key, e->note_id);
        break;
    }
    case CLAP_EVENT_NOTE_CHOKE: {
        const clap_event_note_t *e = (const clap_event_note_t *)h;
        note_choke(s, e->key, e->note_id);
        break;
    }
    case CLAP_EVENT_MIDI: {
        const clap_event_midi_t *e = (const clap_event_midi_t *)h;
        uint8_t status = e->data[0] & 0xF0, d1 = e->data[1], d2 = e->data[2];
        if (status == 0x90 && d2 > 0)
            note_on(s, d1, -1);
        else if (status == 0x80 || (status == 0x90 && d2 == 0))
            note_off(s, d1, -1);
        break;
    }
    case CLAP_EVENT_PARAM_VALUE: {
        const clap_event_param_value_t *e = (const clap_event_param_value_t *)h;
        if (e->param_id == 0)
            s->gain = e->value;
        break;
    }
    }
}

static void render(synth_t *s, float *l, float *r, uint32_t from, uint32_t to) {
    double atk = s->sr * 0.005, rel = s->sr * 0.200;
    if (atk < 1)
        atk = 1;
    if (rel < 1)
        rel = 1;
    for (uint32_t i = from; i < to; i++) {
        double mix = 0.0;
        for (int vi = 0; vi < NVOICES; vi++) {
            voice_t *v = &s->voices[vi];
            if (!v->used)
                continue;
            if (v->stage == ATTACK) {
                v->env += 1.0 / atk;
                if (v->env >= 1.0) {
                    v->env = 1.0;
                    v->stage = SUSTAIN;
                }
            } else if (v->stage == RELEASE) {
                v->env -= 1.0 / rel;
                if (v->env <= 0.0) {
                    v->env = 0.0;
                    v->used = false;
                    continue;
                }
            }
            mix += sin(v->phase * 2.0 * M_PI) * v->env;
            v->phase += v->freq / s->sr;
            if (v->phase >= 1.0)
                v->phase -= 1.0;
        }
        float out = (float)(mix * s->gain);
        l[i] = out;
        r[i] = out;
    }
}

// ---- plugin ----
static bool plug_init(const clap_plugin_t *p) {
    (void)p;
    return true;
}
static void plug_destroy(const clap_plugin_t *p) { free(p->plugin_data); }
static bool plug_activate(const clap_plugin_t *p, double sr, uint32_t mn, uint32_t mx) {
    (void)mn;
    (void)mx;
    ((synth_t *)p->plugin_data)->sr = sr;
    return true;
}
static void plug_deactivate(const clap_plugin_t *p) { (void)p; }
static bool plug_start(const clap_plugin_t *p) {
    (void)p;
    return true;
}
static void plug_stop(const clap_plugin_t *p) { (void)p; }
static void plug_reset(const clap_plugin_t *p) {
    synth_t *s = p->plugin_data;
    memset(s->voices, 0, sizeof(s->voices));
}

static clap_process_status plug_process(const clap_plugin_t *p, const clap_process_t *proc) {
    synth_t *s = p->plugin_data;
    const uint32_t nframes = proc->frames_count;
    const uint32_t nev = proc->in_events->size(proc->in_events);
    float *l = proc->audio_outputs[0].data32[0];
    float *r = proc->audio_outputs[0].data32[1];
    uint32_t ev = 0, next = nev ? 0 : nframes;

    for (uint32_t i = 0; i < nframes;) {
        while (ev < nev && next == i) {
            const clap_event_header_t *h = proc->in_events->get(proc->in_events, ev);
            if (h->time != i) {
                next = h->time;
                break;
            }
            handle_event(s, h);
            if (++ev == nev)
                next = nframes;
        }
        render(s, l, r, i, next);
        i = next;
    }
    return CLAP_PROCESS_CONTINUE;
}

static const void *plug_get_extension(const clap_plugin_t *p, const char *id) {
    (void)p;
    if (!strcmp(id, CLAP_EXT_AUDIO_PORTS))
        return &s_audio_ports;
    if (!strcmp(id, CLAP_EXT_NOTE_PORTS))
        return &s_note_ports;
    if (!strcmp(id, CLAP_EXT_PARAMS))
        return &s_params;
    return NULL;
}
static void plug_on_main_thread(const clap_plugin_t *p) { (void)p; }

static clap_plugin_t *create(const clap_host_t *host) {
    synth_t *s = calloc(1, sizeof(*s));
    s->host = host;
    s->sr = 48000.0;
    s->gain = 0.3;
    s->plugin.desc = &s_desc;
    s->plugin.plugin_data = s;
    s->plugin.init = plug_init;
    s->plugin.destroy = plug_destroy;
    s->plugin.activate = plug_activate;
    s->plugin.deactivate = plug_deactivate;
    s->plugin.start_processing = plug_start;
    s->plugin.stop_processing = plug_stop;
    s->plugin.reset = plug_reset;
    s->plugin.process = plug_process;
    s->plugin.get_extension = plug_get_extension;
    s->plugin.on_main_thread = plug_on_main_thread;
    return &s->plugin;
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
