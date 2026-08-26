#include "qwkscreen.h"
#include "qwkcompositor.h"

#include <QtCore/qbytearray.h>

extern "C" {
#include "wkgfx.h"
}

QT_BEGIN_NAMESPACE

// Geometry and format are settled in the CONSTRUCTOR, not in initialize().
// QWindowSystemInterface::handleScreenAdded() snapshots the platform screen's
// geometry into QScreenPrivate there and then; a screen that is still 0x0 when
// it is added stays 0x0 as far as QScreen::geometry() and QGuiApplication are
// concerned, however correct QPlatformScreen::geometry() becomes afterwards.
// (Windows still came out the right size, because QFbWindow::setVisible asks
// the PLATFORM screen — which is exactly what makes this bug quiet.)
QWkScreen::QWkScreen()
{
    // The surface is already open: QWkIntegration's constructor calls
    // wkgfx_open() BEFORE building the screen, because the geometry that
    // matters is whatever size the host actually granted, not what we asked
    // for.
    mGeometry = QRect(0, 0, int(wkgfx_width()), int(wkgfx_height()));

    // RGBX8888 is byte-order R,G,B,X in memory with the X byte written as
    // 0xff, which is exactly wkgfx_present()'s [r,g,b,a] contract — so the
    // composed screen image goes to the host with no conversion and no
    // swizzle pass. We take the opaque variant rather than RGBA8888 on
    // purpose: with an alpha channel QFbScreen::doRedraw() clears uncovered
    // regions to TRANSPARENT (qfbscreen.cpp:184) and a wk node would show the
    // canvas through its own background. Set QT_WK_ALPHA=1 if you actually
    // want a translucent node.
    //
    // Neither format is the raster engine's fast path (that is
    // ARGB32_Premultiplied). If compositing ever shows up in a profile, the
    // fix is to composite premultiplied and swizzle once per frame in
    // QWkCompositor::onFrame() — one full-surface pass instead of a
    // per-primitive conversion. Measure first.
    mFormat = qEnvironmentVariableIntValue("QT_WK_ALPHA") == 1 ? QImage::Format_RGBA8888
                                                               : QImage::Format_RGBX8888;
    mDepth = 32;
    mPhysicalSize = QSizeF(mGeometry.width() / 96.0 * 25.4, mGeometry.height() / 96.0 * 25.4);

    m_compositor = new QWkCompositor(this);
}

QWkScreen::~QWkScreen()
{
    delete m_compositor;
}

bool QWkScreen::initialize()
{
    // Called from QWkIntegration::initialize(), i.e. once QGuiApplication is
    // far enough along to accept posted events — initializeCompositor()
    // allocates mScreenImage and posts the first UpdateRequest to ourselves.
    initializeCompositor();
    return true;
}

QFbScreen::Flags QWkScreen::flags() const
{
    // QFbWindow::setVisible() forces the FIRST top-level to fill the screen
    // unless this flag is set (qfbwindow.cpp:47-55). That default is right for
    // a wk node: one app, filling its surface, with dialogs and popups
    // floating above it — the same shape linuxfb and vnc ship. QT_WK_WINDOW_MODE
    // =windows opts into free-floating top-levels instead, which is only
    // useful once there is window chrome to drag.
    if (qgetenv("QT_WK_WINDOW_MODE") == "windows")
        return DontForceFirstWindowToFullScreen;
    return {};
}

void QWkScreen::handleResize(uint32_t width, uint32_t height)
{
    const QRect rect(0, 0, int(width), int(height));
    if (rect == mGeometry)
        return;
    setGeometry(rect);
    setPhysicalSize(QSize(int(rect.width() / 96.0 * 25.4), int(rect.height() / 96.0 * 25.4)));
    setDirty(rect);
}

QRegion QWkScreen::doRedraw()
{
    const QRegion touched = QFbScreen::doRedraw();
    m_damage += touched;
    return touched;
}

bool QWkScreen::takeDamage()
{
    if (m_damage.isEmpty())
        return false;
    m_damage = QRegion();
    return true;
}

QT_END_NAMESPACE
