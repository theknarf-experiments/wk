// The wk half of the Slate node: everything the upstream app must not have to
// know about.
//
// It is a separate translation unit, added to upstream's `app` target from
// ../node/CMakeLists.txt, rather than a patch to app/main.cpp. Three reasons:
// it keeps patches/ for changes that are genuinely about Slate; a patch to
// main.cpp would conflict with every upstream change to it; and this file has
// to run BEFORE main() anyway (see below), which a patch to main() cannot do
// as cleanly.
//
// TWO JOBS.
//
// 1. NAME THE STATIC PLUGINS. There is no dlopen on wasm, so Qt cannot find a
//    platform plugin by scanning a directory -- every plugin is linked in and
//    must be named once with Q_IMPORT_PLUGIN. Without the line below,
//    QApplication aborts with "no Qt platform plugin could be initialized" and
//    an EMPTY list of available plugins, which reads like a broken Qt build
//    but is a missing symbol reference. Qt's CMake generates these
//    automatically for plugins it built itself; libqwk.a is ours and out of
//    tree, so it does not.
//
// 2. PICK THE wk DEFAULTS BEFORE QApplication EXISTS. All three of these are
//    read during QGuiApplication construction, i.e. before the first line of
//    Slate's main() body could set them:
//
//      QT_QPA_PLATFORM=wk     plugins/qt/sysroot was configured with
//                             QT_QPA_DEFAULT_PLATFORM=offscreen (it predates
//                             the wk QPA plugin), so without this the app runs
//                             perfectly and renders to nowhere.
//      QT_QUICK_BACKEND=software
//      QSG_RENDER_LOOP=basic  Qt Quick's default scenegraph is the RHI, which
//                             wants OpenGL/Vulkan/Metal. A wk node has an RGBA8
//                             framebuffer and nothing else. The `software`
//                             adaptation renders the scene with QPainter -- and
//                             renders QQuickPaintedItem, which is what Slate's
//                             canvas, rulers, cursors and selection overlays
//                             are. `basic` keeps the render loop on the main
//                             thread; there are no threads here.
//      QT_QPA_FONTDIR=:/wkfonts
//                             Qt 6 ships no fonts and a wk node has no host
//                             font directory, so with none of this the app
//                             runs and every string is invisible. This points
//                             QWkFontDatabase at a Qt RESOURCE directory
//                             compiled into the component (see
//                             node/CMakeLists.txt), which works because
//                             QWkFontDatabase reads ":..." paths through QFile
//                             rather than handing them to FreeType. A node
//                             that mounts real fonts can override it with a
//                             VFS path and this resource is never read.
//
//                             It has to be a directory of ITS OWN and not
//                             Slate's :/fonts, because QWkFontDatabase takes
//                             the FIRST family it registers as the default
//                             font -- and :/fonts's first entry is
//                             FontAwesome.otf, which would make every
//                             unstyled label render as icon glyphs.
//
// A file-scope object is used rather than a function call because it must run
// before main(), and Q_COREAPP_STARTUP_FUNCTION is not available this early
// either (it also runs after QCoreApplication's constructor has begun).
// qputenv() is just setenv(); it needs no QCoreApplication.

#include <QtCore/qglobal.h>
#include <QtCore/qplugin.h>
#include <QtCore/qtenvironmentvariables.h>

Q_IMPORT_PLUGIN(QWkIntegrationPlugin)

namespace {

struct WkSlateDefaults
{
    WkSlateDefaults()
    {
        setDefault("QT_QPA_PLATFORM", "wk");
        setDefault("QT_QUICK_BACKEND", "software");
        setDefault("QSG_RENDER_LOOP", "basic");
        setDefault("QT_QPA_FONTDIR", ":/wkfonts");
    }

    // Never overwrite: the whole point is that a workspace file, a Dockerfile
    // ENV or a shell can still say otherwise -- QT_QPA_PLATFORM=offscreen in
    // particular is how you tell "Qt is broken" from "the wk QPA is broken".
    static void setDefault(const char *name, const char *value)
    {
        if (!qEnvironmentVariableIsSet(name))
            qputenv(name, value);
    }
};

const WkSlateDefaults wkSlateDefaults;

} // namespace
