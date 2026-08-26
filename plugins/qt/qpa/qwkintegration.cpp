#include "qwkintegration.h"

#include "qwkbackingstore.h"
#include "qwkclipboard.h"
#include "qwkeventdispatcher.h"
#include "qwkfontdatabase.h"
#include "qwkscreen.h"
#include "qwktheme.h"
#include "qwkwindow.h"

#include <QtCore/qbytearray.h>
#include <QtCore/qstringlist.h>

#include <qpa/qplatforminputcontext.h>
#include <qpa/qwindowsysteminterface.h>

extern "C" {
#include "wkgfx.h"
}

QT_BEGIN_NAMESPACE

QWkIntegration *QWkIntegration::s_instance = nullptr;

// "1024x768" -> (1024, 768). Accepts the same spelling as QT_WK_SIZE and as a
// -platform wk:1024x768 parameter.
static bool parseSize(const QString &spec, uint32_t *w, uint32_t *h)
{
    const qsizetype x = spec.indexOf(u'x');
    if (x <= 0)
        return false;
    bool okW = false, okH = false;
    const uint vw = spec.left(x).toUInt(&okW);
    const uint vh = spec.mid(x + 1).toUInt(&okH);
    if (!okW || !okH || vw == 0 || vh == 0)
        return false;
    *w = vw;
    *h = vh;
    return true;
}

QWkIntegration::QWkIntegration(const QStringList &parameters)
{
    s_instance = this;

    uint32_t w = 1024, h = 768;
    if (qEnvironmentVariableIsSet("QT_WK_SIZE"))
        parseSize(qEnvironmentVariable("QT_WK_SIZE"), &w, &h);
    for (const QString &p : parameters)
        parseSize(p, &w, &h);

    // FIRST. The screen's geometry is read back from the surface, and the
    // host may well have clamped or overridden what we asked for.
    wkgfx_open(w, h);

    m_screen = new QWkScreen;
    QWindowSystemInterface::handleScreenAdded(m_screen);
}

QWkIntegration::~QWkIntegration()
{
    if (m_screen)
        QWindowSystemInterface::handleScreenRemoved(m_screen); // deletes it
    delete m_fontDatabase;
    delete m_inputContext;
#if !defined(QT_NO_CLIPBOARD)
    delete m_clipboard;
#endif
    if (s_instance == this)
        s_instance = nullptr;
}

void QWkIntegration::initialize()
{
    m_screen->initialize();
}

bool QWkIntegration::hasCapability(QPlatformIntegration::Capability cap) const
{
    switch (cap) {
    // NO THREADS on wasip2: wasi-libc's pthread_create is a stub returning
    // ENOTSUP and this Qt is built FEATURE_thread=OFF. Claiming threaded
    // pixmaps would be an invitation to deadlock.
    case ThreadedPixmaps:
    case ThreadedOpenGL:
    case OpenGL:
    case BufferQueueingOpenGL:
    case RhiBasedRendering:
    case RasterGLSurface:
    case ScreenWindowGrabbing:
        return false;
    case MultipleWindows:      // decision #1: many QWindows, one wk surface
    case WindowManagement:
    case NonFullScreenWindows:
    case WindowActivation:
        return true;
    default:
        return QPlatformIntegration::hasCapability(cap);
    }
}

QPlatformWindow *QWkIntegration::createPlatformWindow(QWindow *window) const
{
    // No requestActivateWindow() here: QFbScreen::addWindow() already emits
    // handleFocusWindowChanged when the window becomes visible.
    return new QWkWindow(window);
}

QPlatformBackingStore *QWkIntegration::createPlatformBackingStore(QWindow *window) const
{
    return new QWkBackingStore(window);
}

QAbstractEventDispatcher *QWkIntegration::createEventDispatcher() const
{
    return new QWkEventDispatcher(m_screen);
}

QPlatformFontDatabase *QWkIntegration::fontDatabase() const
{
    if (!m_fontDatabase)
        m_fontDatabase = new QWkFontDatabase;
    return m_fontDatabase;
}

QPlatformInputContext *QWkIntegration::inputContext() const
{
    // A bare QPlatformInputContext, so QInputMethod has something to talk to
    // and QLineEdit's cursor-rectangle plumbing does not assert. There is no
    // host IME to bridge to; composed characters can only ever arrive
    // pre-composed in wkgfx_event.ch.
    if (!m_inputContext)
        m_inputContext = new QPlatformInputContext;
    return m_inputContext;
}

#if !defined(QT_NO_CLIPBOARD)
QPlatformClipboard *QWkIntegration::clipboard() const
{
    // Lazily, like fontDatabase() and inputContext(), and deliberately NOT in
    // the constructor: wkgfx_open() has to be the first host call this plugin
    // makes (the screen geometry is read back from it), and a clipboard
    // constructed alongside it would put a wk:clipboard call ahead of that.
    //
    // Without this override QPlatformIntegration hands back a default
    // QPlatformClipboard — a process-global QMimeData holder — so copy/paste
    // works perfectly inside the node while being invisible to the machine it
    // runs on. That is the "No clipboard" bullet in PORTING.md.
    if (!m_clipboard)
        m_clipboard = new QWkClipboard;
    return m_clipboard;
}
#endif

QStringList QWkIntegration::themeNames() const
{
    return QStringList{ QStringLiteral("wk") };
}

QPlatformTheme *QWkIntegration::createPlatformTheme(const QString &name) const
{
    if (name == QLatin1String("wk"))
        return new QWkTheme;
    return QPlatformIntegration::createPlatformTheme(name);
}

QT_END_NAMESPACE
