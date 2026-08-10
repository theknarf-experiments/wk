// wk-native GUI extension for CLAP plugins running in wk. A plugin that wants a
// UI implements this and returns it from `clap_plugin.get_extension("wk.gui")`.
// Unlike CLAP's clap.gui (native window handles), the plugin draws to a
// `wasi:surface` it creates — the host composites it and feeds it input, exactly
// like every other wk node. Headless plugins simply don't implement it.
#pragma once

#include <clap/clap.h>

static const char WK_EXT_GUI[] = "wk.gui";

typedef struct wk_gui {
    // Bring the GUI up: create the wasi:surface + graphics context. Called once
    // before the first render. Return false on failure.
    // [main-thread]
    bool(CLAP_ABI *create)(const clap_plugin_t *plugin);

    // Render one frame: poll the surface's pointer/key input and paint. The host
    // calls this once per UI frame.
    // [main-thread]
    void(CLAP_ABI *render)(const clap_plugin_t *plugin);
} wk_gui_t;
