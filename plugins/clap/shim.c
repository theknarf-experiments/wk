// wk:clap shim — bridges an unmodified CLAP plugin (its `clap_entry` global) to
// the wk:clap WIT world. It implements the WIT `plugins` exports by driving the
// plugin's `clap_plugin` vtable, and presents a `clap_host` (log / thread-check
// / latency / state) that forwards to the imported wk `host` interface. Link it
// with a CLAP plugin translation unit and the wit-bindgen `gen/plugin.c`.
//
// Marshalling is by value across the boundary (events as a tagged list, audio as
// channel lists); a shared-memory fast path can replace `process` later without
// touching the WIT.

#include <stdlib.h>
#include <string.h>

#include <clap/clap.h>

#include "gen/plugin.h"

// ---------------------------------------------------------------------------
// The exported resource rep: a live CLAP plugin instance.
// ---------------------------------------------------------------------------
struct exports_wk_clap_plugins_plugin_t {
    const clap_plugin_t *clap;
};

// ---------------------------------------------------------------------------
// Host vtable presented to the plugin. Every callback forwards to the wk host
// import; `get_extension` hands back the core host extensions we support.
// ---------------------------------------------------------------------------
static wk_clap_host_log_severity_t map_severity(clap_log_severity s) {
    switch (s) {
    case CLAP_LOG_DEBUG: return WK_CLAP_TYPES_LOG_SEVERITY_DEBUG;
    case CLAP_LOG_INFO: return WK_CLAP_TYPES_LOG_SEVERITY_INFO;
    case CLAP_LOG_WARNING: return WK_CLAP_TYPES_LOG_SEVERITY_WARNING;
    case CLAP_LOG_ERROR: return WK_CLAP_TYPES_LOG_SEVERITY_ERROR;
    default: return WK_CLAP_TYPES_LOG_SEVERITY_FATAL;
    }
}

static void host_log(const clap_host_t *host, clap_log_severity sev, const char *msg) {
    (void)host;
    plugin_string_t s;
    plugin_string_set(&s, msg ? msg : "");
    wk_clap_host_log(map_severity(sev), &s);
}
static const clap_host_log_t s_host_log = {.log = host_log};

// wk drives the plugin from a single thread, so both checks answer true.
static bool host_is_main_thread(const clap_host_t *h) { (void)h; return true; }
static bool host_is_audio_thread(const clap_host_t *h) { (void)h; return true; }
static const clap_host_thread_check_t s_host_thread_check = {
    .is_main_thread = host_is_main_thread,
    .is_audio_thread = host_is_audio_thread,
};

static void host_latency_changed(const clap_host_t *h) { (void)h; wk_clap_host_latency_changed(); }
static const clap_host_latency_t s_host_latency = {.changed = host_latency_changed};

static void host_state_mark_dirty(const clap_host_t *h) { (void)h; wk_clap_host_state_mark_dirty(); }
static const clap_host_state_t s_host_state = {.mark_dirty = host_state_mark_dirty};

static void host_params_rescan(const clap_host_t *h, clap_param_rescan_flags f) {
    (void)h;
    wk_clap_host_params_rescan((uint32_t)f);
}
static void host_params_clear(const clap_host_t *h, clap_id id, clap_param_clear_flags f) {
    (void)h;
    wk_clap_host_params_clear((uint32_t)id, (uint32_t)f);
}
static void host_params_request_flush(const clap_host_t *h) { (void)h; }
static const clap_host_params_t s_host_params = {
    .rescan = host_params_rescan,
    .clear = host_params_clear,
    .request_flush = host_params_request_flush,
};

static const void *host_get_extension(const clap_host_t *host, const char *id) {
    (void)host;
    if (!strcmp(id, CLAP_EXT_LOG)) return &s_host_log;
    if (!strcmp(id, CLAP_EXT_THREAD_CHECK)) return &s_host_thread_check;
    if (!strcmp(id, CLAP_EXT_LATENCY)) return &s_host_latency;
    if (!strcmp(id, CLAP_EXT_STATE)) return &s_host_state;
    if (!strcmp(id, CLAP_EXT_PARAMS)) return &s_host_params;
    return NULL;
}
static void host_request_restart(const clap_host_t *h) { (void)h; wk_clap_host_request_restart(); }
static void host_request_process(const clap_host_t *h) { (void)h; wk_clap_host_request_process(); }
static void host_request_callback(const clap_host_t *h) { (void)h; wk_clap_host_request_callback(); }

static const clap_host_t s_host = {
    .clap_version = CLAP_VERSION_INIT,
    .host_data = NULL,
    .name = "wk",
    .vendor = "wk",
    .url = "https://github.com/theknarf/wk",
    .version = "0.1",
    .get_extension = host_get_extension,
    .request_restart = host_request_restart,
    .request_process = host_request_process,
    .request_callback = host_request_callback,
};

// ---------------------------------------------------------------------------
// The plugin's entry point (its `clap_entry` global) and factory, resolved once.
// ---------------------------------------------------------------------------
extern const clap_plugin_entry_t clap_entry;

static const clap_plugin_factory_t *g_factory = NULL;
static bool g_entry_started = false;

static const clap_plugin_factory_t *plugin_factory(void) {
    if (!g_entry_started) {
        g_entry_started = true;
        if (clap_entry.init("/")) {
            g_factory = (const clap_plugin_factory_t *)clap_entry.get_factory(CLAP_PLUGIN_FACTORY_ID);
        }
    }
    return g_factory;
}

// Small helpers for reaching a plugin's extensions.
static const void *ext(const clap_plugin_t *p, const char *id) {
    return p->get_extension ? p->get_extension(p, id) : NULL;
}

// ---------------------------------------------------------------------------
// Factory exports.
// ---------------------------------------------------------------------------
uint32_t exports_wk_clap_plugins_count(void) {
    const clap_plugin_factory_t *f = plugin_factory();
    return f ? f->get_plugin_count(f) : 0;
}

static void set_string(plugin_string_t *out, const char *s) { plugin_string_dup(out, s ? s : ""); }

bool exports_wk_clap_plugins_get(uint32_t index, exports_wk_clap_plugins_descriptor_t *ret) {
    const clap_plugin_factory_t *f = plugin_factory();
    if (!f) return false;
    const clap_plugin_descriptor_t *d = f->get_plugin_descriptor(f, index);
    if (!d) return false;
    memset(ret, 0, sizeof(*ret));
    set_string(&ret->id, d->id);
    set_string(&ret->name, d->name);
    set_string(&ret->vendor, d->vendor);
    set_string(&ret->version, d->version);
    size_t n = 0;
    if (d->features)
        while (d->features[n]) n++;
    ret->features.len = n;
    ret->features.ptr = n ? calloc(n, sizeof(plugin_string_t)) : NULL;
    for (size_t i = 0; i < n; i++) set_string(&ret->features.ptr[i], d->features[i]);
    return true;
}

bool exports_wk_clap_plugins_create(plugin_string_t *plugin_id,
                                    exports_wk_clap_plugins_own_plugin_t *ret) {
    const clap_plugin_factory_t *f = plugin_factory();
    char *id = plugin_id->len ? strndup((char *)plugin_id->ptr, plugin_id->len) : strdup("");
    if (plugin_id->ptr) free(plugin_id->ptr); // we own the lowered input string
    const clap_plugin_t *cp = f ? f->create_plugin(f, &s_host, id) : NULL;
    free(id);
    if (!cp) return false;
    exports_wk_clap_plugins_plugin_t *rep = malloc(sizeof(*rep));
    rep->clap = cp;
    *ret = exports_wk_clap_plugins_plugin_new(rep);
    return true;
}

// Destructor for the plugin resource (called on drop).
void exports_wk_clap_plugins_plugin_destructor(exports_wk_clap_plugins_plugin_t *rep) {
    if (rep->clap && rep->clap->destroy) rep->clap->destroy(rep->clap);
    free(rep);
}

// ---------------------------------------------------------------------------
// Lifecycle methods — forward to the clap_plugin vtable.
// ---------------------------------------------------------------------------
bool exports_wk_clap_plugins_method_plugin_init(exports_wk_clap_plugins_borrow_plugin_t self) {
    return self->clap->init(self->clap);
}
bool exports_wk_clap_plugins_method_plugin_activate(exports_wk_clap_plugins_borrow_plugin_t self,
                                                    double sample_rate, uint32_t min_frames,
                                                    uint32_t max_frames) {
    return self->clap->activate(self->clap, sample_rate, min_frames, max_frames);
}
void exports_wk_clap_plugins_method_plugin_deactivate(exports_wk_clap_plugins_borrow_plugin_t self) {
    self->clap->deactivate(self->clap);
}
bool exports_wk_clap_plugins_method_plugin_start_processing(
    exports_wk_clap_plugins_borrow_plugin_t self) {
    return self->clap->start_processing(self->clap);
}
void exports_wk_clap_plugins_method_plugin_stop_processing(
    exports_wk_clap_plugins_borrow_plugin_t self) {
    self->clap->stop_processing(self->clap);
}
void exports_wk_clap_plugins_method_plugin_reset(exports_wk_clap_plugins_borrow_plugin_t self) {
    self->clap->reset(self->clap);
}
void exports_wk_clap_plugins_method_plugin_on_main_thread(
    exports_wk_clap_plugins_borrow_plugin_t self) {
    self->clap->on_main_thread(self->clap);
}

// ---------------------------------------------------------------------------
// Event marshalling.
// ---------------------------------------------------------------------------
typedef union {
    clap_event_header_t hdr;
    clap_event_note_t note;
    clap_event_note_expression_t expr;
    clap_event_param_value_t pv;
    clap_event_param_mod_t pm;
    clap_event_param_gesture_t pg;
    clap_event_midi_t midi;
    clap_event_midi_sysex_t sysex;
    clap_event_midi2_t midi2;
} any_event_t;

// Fill a CLAP event union from a WIT event. Returns false if the arm has no CLAP
// equivalent to forward.
static bool wit_to_clap_event(const wk_clap_types_event_t *e, any_event_t *out) {
    memset(out, 0, sizeof(*out));
    switch (e->tag) {
    case WK_CLAP_TYPES_EVENT_NOTE_ON:
    case WK_CLAP_TYPES_EVENT_NOTE_OFF:
    case WK_CLAP_TYPES_EVENT_NOTE_CHOKE:
    case WK_CLAP_TYPES_EVENT_NOTE_END: {
        const wk_clap_types_note_t *n = &e->val.note_on; // same layout for all four
        out->note.header = (clap_event_header_t){sizeof(clap_event_note_t), n->time,
                                                 CLAP_CORE_EVENT_SPACE_ID, e->tag, n->flag_set};
        out->note.note_id = n->note_id;
        out->note.port_index = n->port_index;
        out->note.channel = n->channel;
        out->note.key = n->key;
        out->note.velocity = n->velocity;
        return true;
    }
    case WK_CLAP_TYPES_EVENT_NOTE_EXPRESSION: {
        const wk_clap_types_note_expr_t *x = &e->val.note_expression;
        out->expr.header = (clap_event_header_t){sizeof(clap_event_note_expression_t), x->time,
                                                 CLAP_CORE_EVENT_SPACE_ID,
                                                 CLAP_EVENT_NOTE_EXPRESSION, x->flag_set};
        out->expr.expression_id = (clap_note_expression)x->expression;
        out->expr.note_id = x->note_id;
        out->expr.port_index = x->port_index;
        out->expr.channel = x->channel;
        out->expr.key = x->key;
        out->expr.value = x->value;
        return true;
    }
    case WK_CLAP_TYPES_EVENT_PARAM_VALUE: {
        const wk_clap_types_param_value_t *p = &e->val.param_value;
        out->pv.header = (clap_event_header_t){sizeof(clap_event_param_value_t), p->time,
                                               CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_PARAM_VALUE,
                                               p->flag_set};
        out->pv.param_id = p->param_id;
        out->pv.cookie = NULL;
        out->pv.note_id = p->note_id;
        out->pv.port_index = p->port_index;
        out->pv.channel = p->channel;
        out->pv.key = p->key;
        out->pv.value = p->value;
        return true;
    }
    case WK_CLAP_TYPES_EVENT_PARAM_MOD: {
        const wk_clap_types_param_mod_t *p = &e->val.param_mod;
        out->pm.header = (clap_event_header_t){sizeof(clap_event_param_mod_t), p->time,
                                               CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_PARAM_MOD,
                                               p->flag_set};
        out->pm.param_id = p->param_id;
        out->pm.cookie = NULL;
        out->pm.note_id = p->note_id;
        out->pm.port_index = p->port_index;
        out->pm.channel = p->channel;
        out->pm.key = p->key;
        out->pm.amount = p->amount;
        return true;
    }
    case WK_CLAP_TYPES_EVENT_PARAM_GESTURE_BEGIN:
    case WK_CLAP_TYPES_EVENT_PARAM_GESTURE_END: {
        const wk_clap_types_param_gesture_t *g = &e->val.param_gesture_begin;
        uint16_t type = e->tag == WK_CLAP_TYPES_EVENT_PARAM_GESTURE_BEGIN
                            ? CLAP_EVENT_PARAM_GESTURE_BEGIN
                            : CLAP_EVENT_PARAM_GESTURE_END;
        out->pg.header = (clap_event_header_t){sizeof(clap_event_param_gesture_t), g->time,
                                               CLAP_CORE_EVENT_SPACE_ID, type, 0};
        out->pg.param_id = g->param_id;
        return true;
    }
    case WK_CLAP_TYPES_EVENT_MIDI: {
        const wk_clap_types_midi_t *m = &e->val.midi;
        out->midi.header = (clap_event_header_t){sizeof(clap_event_midi_t), m->time,
                                                 CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_MIDI, 0};
        out->midi.port_index = m->port_index;
        out->midi.data[0] = m->data.f0;
        out->midi.data[1] = m->data.f1;
        out->midi.data[2] = m->data.f2;
        return true;
    }
    case WK_CLAP_TYPES_EVENT_MIDI_SYSEX: {
        const wk_clap_types_midi_sysex_t *m = &e->val.midi_sysex;
        out->sysex.header = (clap_event_header_t){sizeof(clap_event_midi_sysex_t), m->time,
                                                  CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_MIDI_SYSEX,
                                                  0};
        out->sysex.port_index = m->port_index;
        out->sysex.buffer = m->buffer.ptr; // valid for the duration of process
        out->sysex.size = (uint32_t)m->buffer.len;
        return true;
    }
    case WK_CLAP_TYPES_EVENT_MIDI2: {
        const wk_clap_types_midi2_t *m = &e->val.midi2;
        out->midi2.header = (clap_event_header_t){sizeof(clap_event_midi2_t), m->time,
                                                  CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_MIDI2, 0};
        out->midi2.port_index = m->port_index;
        out->midi2.data[0] = m->data.f0;
        out->midi2.data[1] = m->data.f1;
        out->midi2.data[2] = m->data.f2;
        out->midi2.data[3] = m->data.f3;
        return true;
    }
    default:
        return false;
    }
}

// Growable list of WIT events, for output.
typedef struct {
    wk_clap_types_event_t *ptr;
    size_t len;
    size_t cap;
} event_vec_t;

static void event_vec_push(event_vec_t *v, const wk_clap_types_event_t *e) {
    if (v->len == v->cap) {
        v->cap = v->cap ? v->cap * 2 : 16;
        v->ptr = realloc(v->ptr, v->cap * sizeof(*v->ptr));
    }
    v->ptr[v->len++] = *e;
}

// Convert a CLAP event the plugin emitted into a WIT event and stash it.
static bool clap_to_wit_event(const clap_event_header_t *h, wk_clap_types_event_t *out) {
    if (h->space_id != CLAP_CORE_EVENT_SPACE_ID) return false;
    memset(out, 0, sizeof(*out));
    switch (h->type) {
    case CLAP_EVENT_NOTE_ON:
    case CLAP_EVENT_NOTE_OFF:
    case CLAP_EVENT_NOTE_CHOKE:
    case CLAP_EVENT_NOTE_END: {
        const clap_event_note_t *n = (const clap_event_note_t *)h;
        out->tag = h->type; // note-on/off/choke/end share our first four tags
        out->val.note_on = (wk_clap_types_note_t){h->time, h->flags, n->note_id, n->port_index,
                                                  n->channel, n->key, n->velocity};
        return true;
    }
    case CLAP_EVENT_NOTE_EXPRESSION: {
        const clap_event_note_expression_t *x = (const clap_event_note_expression_t *)h;
        out->tag = WK_CLAP_TYPES_EVENT_NOTE_EXPRESSION;
        out->val.note_expression = (wk_clap_types_note_expr_t){
            h->time,           h->flags,     (wk_clap_types_note_expression_t)x->expression_id,
            x->note_id,        x->port_index, x->channel,
            x->key,            x->value};
        return true;
    }
    case CLAP_EVENT_PARAM_VALUE: {
        const clap_event_param_value_t *p = (const clap_event_param_value_t *)h;
        out->tag = WK_CLAP_TYPES_EVENT_PARAM_VALUE;
        out->val.param_value = (wk_clap_types_param_value_t){h->time,      h->flags,     p->param_id,
                                                             p->note_id,   p->port_index, p->channel,
                                                             p->key,       p->value};
        return true;
    }
    case CLAP_EVENT_PARAM_GESTURE_BEGIN:
    case CLAP_EVENT_PARAM_GESTURE_END: {
        const clap_event_param_gesture_t *g = (const clap_event_param_gesture_t *)h;
        out->tag = h->type == CLAP_EVENT_PARAM_GESTURE_BEGIN
                       ? WK_CLAP_TYPES_EVENT_PARAM_GESTURE_BEGIN
                       : WK_CLAP_TYPES_EVENT_PARAM_GESTURE_END;
        out->val.param_gesture_begin = (wk_clap_types_param_gesture_t){h->time, g->param_id};
        return true;
    }
    case CLAP_EVENT_MIDI: {
        const clap_event_midi_t *m = (const clap_event_midi_t *)h;
        out->tag = WK_CLAP_TYPES_EVENT_MIDI;
        out->val.midi = (wk_clap_types_midi_t){
            h->time, m->port_index, {m->data[0], m->data[1], m->data[2]}};
        return true;
    }
    case CLAP_EVENT_MIDI2: {
        const clap_event_midi2_t *m = (const clap_event_midi2_t *)h;
        out->tag = WK_CLAP_TYPES_EVENT_MIDI2;
        out->val.midi2 = (wk_clap_types_midi2_t){
            h->time, m->port_index, {m->data[0], m->data[1], m->data[2], m->data[3]}};
        return true;
    }
    default:
        return false; // transport & sysex-out omitted for now
    }
}

// clap_input_events backed by a flat array of any_event_t.
typedef struct {
    const any_event_t *events;
    uint32_t count;
} in_events_ctx_t;

static uint32_t in_events_size(const clap_input_events_t *l) {
    return ((in_events_ctx_t *)l->ctx)->count;
}
static const clap_event_header_t *in_events_get(const clap_input_events_t *l, uint32_t i) {
    return &((in_events_ctx_t *)l->ctx)->events[i].hdr;
}

static bool out_events_try_push(const clap_output_events_t *l, const clap_event_header_t *ev) {
    wk_clap_types_event_t w;
    if (clap_to_wit_event(ev, &w)) event_vec_push((event_vec_t *)l->ctx, &w);
    return true;
}

// Build `float**` channel pointers for one WIT audio buffer (channels x frames).
static float **channel_ptrs(const plugin_list_f32_t *channels, size_t n) {
    float **p = n ? malloc(n * sizeof(float *)) : NULL;
    for (size_t c = 0; c < n; c++) p[c] = channels[c].ptr;
    return p;
}

void exports_wk_clap_plugins_method_plugin_process(
    exports_wk_clap_plugins_borrow_plugin_t self, int64_t steady_time, uint32_t frames,
    exports_wk_clap_plugins_transport_t *maybe_transport,
    exports_wk_clap_plugins_list_event_t *in_events, plugin_list_audio_buffer_t *audio_in,
    exports_wk_clap_plugins_process_result_t *ret) {
    const clap_plugin_t *cp = self->clap;

    // ---- input events -> CLAP array ----
    uint32_t nev = (uint32_t)in_events->len;
    any_event_t *evbuf = nev ? malloc(nev * sizeof(any_event_t)) : NULL;
    uint32_t nev_ok = 0;
    for (uint32_t i = 0; i < nev; i++)
        if (wit_to_clap_event(&in_events->ptr[i], &evbuf[nev_ok])) nev_ok++;
    in_events_ctx_t in_ctx = {evbuf, nev_ok};
    clap_input_events_t in_list = {&in_ctx, in_events_size, in_events_get};

    // ---- output events sink ----
    event_vec_t out_vec = {0};
    clap_output_events_t out_list = {&out_vec, out_events_try_push};

    // ---- input audio buffers ----
    uint32_t n_in = (uint32_t)audio_in->len;
    clap_audio_buffer_t *cin = n_in ? calloc(n_in, sizeof(clap_audio_buffer_t)) : NULL;
    for (uint32_t p = 0; p < n_in; p++) {
        size_t nch = audio_in->ptr[p].len;
        cin[p].data32 = channel_ptrs(audio_in->ptr[p].ptr, nch);
        cin[p].channel_count = (uint32_t)nch;
    }

    // ---- output audio buffers: allocate from the plugin's declared out ports ----
    const clap_plugin_audio_ports_t *ap =
        (const clap_plugin_audio_ports_t *)ext(cp, CLAP_EXT_AUDIO_PORTS);
    uint32_t n_out = ap ? ap->count(cp, false) : 0;
    clap_audio_buffer_t *cout = n_out ? calloc(n_out, sizeof(clap_audio_buffer_t)) : NULL;
    // The WIT result mirrors these: one audio-buffer per out port.
    plugin_list_audio_buffer_t out_bufs = {0};
    out_bufs.len = n_out;
    out_bufs.ptr = n_out ? calloc(n_out, sizeof(wk_clap_types_audio_buffer_t)) : NULL;
    for (uint32_t p = 0; p < n_out; p++) {
        clap_audio_port_info_t info = {0};
        uint32_t nch = ap && ap->get(cp, p, false, &info) ? info.channel_count : 2;
        cout[p].channel_count = nch;
        cout[p].data32 = nch ? malloc(nch * sizeof(float *)) : NULL;
        out_bufs.ptr[p].len = nch;
        out_bufs.ptr[p].ptr = nch ? calloc(nch, sizeof(plugin_list_f32_t)) : NULL;
        for (uint32_t c = 0; c < nch; c++) {
            float *chan = calloc(frames ? frames : 1, sizeof(float));
            cout[p].data32[c] = chan;
            out_bufs.ptr[p].ptr[c].ptr = chan;
            out_bufs.ptr[p].ptr[c].len = frames;
        }
    }

    // ---- transport ----
    clap_event_transport_t ctrans;
    const clap_event_transport_t *ctrans_p = NULL;
    if (maybe_transport) {
        memset(&ctrans, 0, sizeof(ctrans));
        ctrans.header = (clap_event_header_t){sizeof(ctrans), 0, CLAP_CORE_EVENT_SPACE_ID,
                                              CLAP_EVENT_TRANSPORT, 0};
        if (maybe_transport->is_playing) ctrans.flags |= CLAP_TRANSPORT_IS_PLAYING;
        if (maybe_transport->is_recording) ctrans.flags |= CLAP_TRANSPORT_IS_RECORDING;
        if (maybe_transport->is_looping) ctrans.flags |= CLAP_TRANSPORT_IS_LOOP_ACTIVE;
        ctrans.flags |= CLAP_TRANSPORT_HAS_TEMPO | CLAP_TRANSPORT_HAS_BEATS_TIMELINE |
                        CLAP_TRANSPORT_HAS_SECONDS_TIMELINE | CLAP_TRANSPORT_HAS_TIME_SIGNATURE;
        ctrans.song_pos_beats = (clap_beattime)(maybe_transport->song_pos_beats * CLAP_BEATTIME_FACTOR);
        ctrans.song_pos_seconds = (clap_sectime)(maybe_transport->song_pos_seconds * CLAP_SECTIME_FACTOR);
        ctrans.tempo = maybe_transport->tempo;
        ctrans.tempo_inc = maybe_transport->tempo_inc;
        ctrans.bar_start = (clap_beattime)(maybe_transport->bar_start_beats * CLAP_BEATTIME_FACTOR);
        ctrans.bar_number = maybe_transport->bar_number;
        ctrans.loop_start_beats = (clap_beattime)(maybe_transport->loop_start_beats * CLAP_BEATTIME_FACTOR);
        ctrans.loop_end_beats = (clap_beattime)(maybe_transport->loop_end_beats * CLAP_BEATTIME_FACTOR);
        ctrans.tsig_num = maybe_transport->tsig_num;
        ctrans.tsig_denom = maybe_transport->tsig_denom;
        ctrans_p = &ctrans;
    }

    // ---- run ----
    clap_process_t proc = {
        .steady_time = steady_time,
        .frames_count = frames,
        .transport = ctrans_p,
        .audio_inputs = cin,
        .audio_outputs = cout,
        .audio_inputs_count = n_in,
        .audio_outputs_count = n_out,
        .in_events = &in_list,
        .out_events = &out_list,
    };
    clap_process_status st = cp->process ? cp->process(cp, &proc) : CLAP_PROCESS_ERROR;

    // ---- assemble result ----
    switch (st) {
    case CLAP_PROCESS_CONTINUE: ret->status = WK_CLAP_TYPES_PROCESS_STATUS_CONTINUE; break;
    case CLAP_PROCESS_CONTINUE_IF_NOT_QUIET:
        ret->status = WK_CLAP_TYPES_PROCESS_STATUS_CONTINUE_IF_NOT_QUIET;
        break;
    case CLAP_PROCESS_TAIL: ret->status = WK_CLAP_TYPES_PROCESS_STATUS_TAIL; break;
    case CLAP_PROCESS_SLEEP: ret->status = WK_CLAP_TYPES_PROCESS_STATUS_SLEEP; break;
    default: ret->status = WK_CLAP_TYPES_PROCESS_STATUS_ERROR; break;
    }
    ret->audio_out = out_bufs;
    ret->out_events.ptr = out_vec.ptr;
    ret->out_events.len = out_vec.len;

    // ---- free scratch (not the returned buffers; post_return frees those) ----
    for (uint32_t p = 0; p < n_in; p++) free(cin[p].data32);
    free(cin);
    for (uint32_t p = 0; p < n_out; p++) free(cout[p].data32);
    free(cout);
    free(evbuf);
    // Free the lowered inputs we own.
    for (uint32_t p = 0; p < n_in; p++) {
        for (size_t c = 0; c < audio_in->ptr[p].len; c++) free(audio_in->ptr[p].ptr[c].ptr);
        free(audio_in->ptr[p].ptr);
    }
    free(audio_in->ptr);
    for (uint32_t i = 0; i < nev; i++)
        if (in_events->ptr[i].tag == WK_CLAP_TYPES_EVENT_MIDI_SYSEX)
            free(in_events->ptr[i].val.midi_sysex.buffer.ptr);
    free(in_events->ptr);
}

exports_wk_clap_plugins_supported_t
exports_wk_clap_plugins_method_plugin_features(exports_wk_clap_plugins_borrow_plugin_t self) {
    const clap_plugin_t *cp = self->clap;
    exports_wk_clap_plugins_supported_t s = 0;
    if (ext(cp, CLAP_EXT_PARAMS)) s |= WK_CLAP_TYPES_SUPPORTED_PARAMS;
    if (ext(cp, CLAP_EXT_AUDIO_PORTS)) s |= WK_CLAP_TYPES_SUPPORTED_AUDIO_PORTS;
    if (ext(cp, CLAP_EXT_NOTE_PORTS)) s |= WK_CLAP_TYPES_SUPPORTED_NOTE_PORTS;
    if (ext(cp, CLAP_EXT_STATE)) s |= WK_CLAP_TYPES_SUPPORTED_STATE;
    return s;
}

// ---------------------------------------------------------------------------
// ext: params
// ---------------------------------------------------------------------------
static const clap_plugin_params_t *params_ext(const clap_plugin_t *cp) {
    return (const clap_plugin_params_t *)ext(cp, CLAP_EXT_PARAMS);
}

uint32_t exports_wk_clap_plugins_method_plugin_param_count(
    exports_wk_clap_plugins_borrow_plugin_t self) {
    const clap_plugin_params_t *p = params_ext(self->clap);
    return p ? p->count(self->clap) : 0;
}

bool exports_wk_clap_plugins_method_plugin_param_info_at(
    exports_wk_clap_plugins_borrow_plugin_t self, uint32_t index,
    exports_wk_clap_plugins_param_info_t *ret) {
    const clap_plugin_params_t *p = params_ext(self->clap);
    clap_param_info_t info = {0};
    if (!p || !p->get_info(self->clap, index, &info)) return false;
    memset(ret, 0, sizeof(*ret));
    ret->id = info.id;
    set_string(&ret->name, info.name);
    set_string(&ret->module, info.module);
    ret->flag_set = info.flags;
    ret->min_value = info.min_value;
    ret->max_value = info.max_value;
    ret->default_value = info.default_value;
    return true;
}

bool exports_wk_clap_plugins_method_plugin_param_get(exports_wk_clap_plugins_borrow_plugin_t self,
                                                     uint32_t id, double *ret) {
    const clap_plugin_params_t *p = params_ext(self->clap);
    return p ? p->get_value(self->clap, id, ret) : false;
}

bool exports_wk_clap_plugins_method_plugin_param_value_to_text(
    exports_wk_clap_plugins_borrow_plugin_t self, uint32_t id, double value, plugin_string_t *ret) {
    const clap_plugin_params_t *p = params_ext(self->clap);
    char buf[256];
    if (!p || !p->value_to_text(self->clap, id, value, buf, sizeof(buf))) return false;
    plugin_string_dup(ret, buf);
    return true;
}

bool exports_wk_clap_plugins_method_plugin_param_text_to_value(
    exports_wk_clap_plugins_borrow_plugin_t self, uint32_t id, plugin_string_t *text, double *ret) {
    const clap_plugin_params_t *p = params_ext(self->clap);
    char *s = strndup((char *)text->ptr, text->len);
    bool ok = p && p->text_to_value(self->clap, id, s, ret);
    free(s);
    if (text->ptr) free(text->ptr);
    return ok;
}

void exports_wk_clap_plugins_method_plugin_params_flush(
    exports_wk_clap_plugins_borrow_plugin_t self, exports_wk_clap_plugins_list_event_t *in_events,
    exports_wk_clap_plugins_list_event_t *ret) {
    const clap_plugin_params_t *p = params_ext(self->clap);
    uint32_t nev = (uint32_t)in_events->len;
    any_event_t *evbuf = nev ? malloc(nev * sizeof(any_event_t)) : NULL;
    uint32_t ok = 0;
    for (uint32_t i = 0; i < nev; i++)
        if (wit_to_clap_event(&in_events->ptr[i], &evbuf[ok])) ok++;
    in_events_ctx_t in_ctx = {evbuf, ok};
    clap_input_events_t in_list = {&in_ctx, in_events_size, in_events_get};
    event_vec_t out_vec = {0};
    clap_output_events_t out_list = {&out_vec, out_events_try_push};
    if (p) p->flush(self->clap, &in_list, &out_list);
    free(evbuf);
    free(in_events->ptr);
    ret->ptr = out_vec.ptr;
    ret->len = out_vec.len;
}

// ---------------------------------------------------------------------------
// ext: audio-ports / note-ports
// ---------------------------------------------------------------------------
uint32_t exports_wk_clap_plugins_method_plugin_audio_port_count(
    exports_wk_clap_plugins_borrow_plugin_t self, bool is_input) {
    const clap_plugin_audio_ports_t *a =
        (const clap_plugin_audio_ports_t *)ext(self->clap, CLAP_EXT_AUDIO_PORTS);
    return a ? a->count(self->clap, is_input) : 0;
}

bool exports_wk_clap_plugins_method_plugin_audio_port_info_at(
    exports_wk_clap_plugins_borrow_plugin_t self, uint32_t index, bool is_input,
    exports_wk_clap_plugins_audio_port_info_t *ret) {
    const clap_plugin_audio_ports_t *a =
        (const clap_plugin_audio_ports_t *)ext(self->clap, CLAP_EXT_AUDIO_PORTS);
    clap_audio_port_info_t info = {0};
    if (!a || !a->get(self->clap, index, is_input, &info)) return false;
    memset(ret, 0, sizeof(*ret));
    ret->id = info.id;
    set_string(&ret->name, info.name);
    ret->channel_count = info.channel_count;
    ret->flag_set = info.flags;
    set_string(&ret->port_type, info.port_type ? info.port_type : "");
    return true;
}

uint32_t exports_wk_clap_plugins_method_plugin_note_port_count(
    exports_wk_clap_plugins_borrow_plugin_t self, bool is_input) {
    const clap_plugin_note_ports_t *n =
        (const clap_plugin_note_ports_t *)ext(self->clap, CLAP_EXT_NOTE_PORTS);
    return n ? n->count(self->clap, is_input) : 0;
}

bool exports_wk_clap_plugins_method_plugin_note_port_info_at(
    exports_wk_clap_plugins_borrow_plugin_t self, uint32_t index, bool is_input,
    exports_wk_clap_plugins_note_port_info_t *ret) {
    const clap_plugin_note_ports_t *np =
        (const clap_plugin_note_ports_t *)ext(self->clap, CLAP_EXT_NOTE_PORTS);
    clap_note_port_info_t info = {0};
    if (!np || !np->get(self->clap, index, is_input, &info)) return false;
    memset(ret, 0, sizeof(*ret));
    ret->id = info.id;
    set_string(&ret->name, info.name);
    ret->supported_dialects = info.supported_dialects;
    ret->preferred_dialect = info.preferred_dialect;
    return true;
}

// ---------------------------------------------------------------------------
// ext: state — bridge clap_ostream/istream to a byte list.
// ---------------------------------------------------------------------------
typedef struct {
    uint8_t *ptr;
    size_t len, cap;
} bytes_t;

static int64_t ostream_write(const clap_ostream_t *s, const void *buf, uint64_t n) {
    bytes_t *b = s->ctx;
    if (b->len + n > b->cap) {
        while (b->len + n > b->cap) b->cap = b->cap ? b->cap * 2 : 256;
        b->ptr = realloc(b->ptr, b->cap);
    }
    memcpy(b->ptr + b->len, buf, n);
    b->len += n;
    return (int64_t)n;
}

typedef struct {
    const uint8_t *ptr;
    size_t len, pos;
} rbytes_t;

static int64_t istream_read(const clap_istream_t *s, void *buf, uint64_t n) {
    rbytes_t *b = s->ctx;
    uint64_t avail = b->len - b->pos;
    if (n > avail) n = avail;
    memcpy(buf, b->ptr + b->pos, n);
    b->pos += n;
    return (int64_t)n;
}

bool exports_wk_clap_plugins_method_plugin_state_save(exports_wk_clap_plugins_borrow_plugin_t self,
                                                      plugin_list_u8_t *ret) {
    const clap_plugin_state_t *st = (const clap_plugin_state_t *)ext(self->clap, CLAP_EXT_STATE);
    if (!st) return false;
    bytes_t b = {0};
    clap_ostream_t os = {&b, ostream_write};
    if (!st->save(self->clap, &os)) {
        free(b.ptr);
        return false;
    }
    ret->ptr = b.ptr;
    ret->len = b.len;
    return true;
}

bool exports_wk_clap_plugins_method_plugin_state_load(exports_wk_clap_plugins_borrow_plugin_t self,
                                                      plugin_list_u8_t *data) {
    const clap_plugin_state_t *st = (const clap_plugin_state_t *)ext(self->clap, CLAP_EXT_STATE);
    bool ok = false;
    if (st) {
        rbytes_t b = {data->ptr, data->len, 0};
        clap_istream_t is = {&b, istream_read};
        ok = st->load(self->clap, &is);
    }
    if (data->ptr) free(data->ptr);
    return ok;
}
