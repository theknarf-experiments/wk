#include "qwkcompositor.h"
#include "qwkscreen.h"
#include "qwkwindow.h"

#include <QtGui/qwindow.h>
#include <qpa/qwindowsysteminterface.h>

extern "C" {
#include "wkgfx.h"
}

QT_BEGIN_NAMESPACE

QWkCompositor::QWkCompositor(QWkScreen *screen) : QObject(screen)
{
    // Deliberately NOT setSynchronousWindowSystemEvents(true), which is what
    // QWasmCompositor's constructor does. The wasm plugin needs it because it
    // is called from DOM callbacks with no event loop of its own underneath;
    // we own the loop, and QWkEventDispatcher::processEvents() flushes the
    // queue explicitly every pass. linuxfb and vnc drive fbconvenience the
    // same asynchronous way.
}

QWkCompositor::~QWkCompositor() = default;

QWkScreen *QWkCompositor::screen() const
{
    return static_cast<QWkScreen *>(parent());
}

void QWkCompositor::requestUpdateWindow(QWkWindow *window, UpdateRequestDeliveryType updateType)
{
    auto it = m_requestUpdateWindows.find(window);
    if (it == m_requestUpdateWindows.end()) {
        m_requestUpdateWindows.insert(window, updateType);
    } else if (it.value() == ExposeEventDelivery && updateType == UpdateRequestDelivery) {
        // Upgrade: a window that asked for an updateRequest must GET an
        // updateRequest, or QWindow subclasses that count them stall.
        it.value() = UpdateRequestDelivery;
    }
}

void QWkCompositor::windowDestroyed(QWkWindow *window)
{
    m_requestUpdateWindows.remove(window);
}

void QWkCompositor::handleBackingStoreFlush(QWindow *window)
{
    // A flush from inside deliverUpdateRequests() is already part of the frame
    // being built; anything else needs a frame of its own.
    if (m_inDeliverUpdateRequest || !window || !window->handle())
        return;
    requestUpdateWindow(static_cast<QWkWindow *>(window->handle()));
}

void QWkCompositor::deliverUpdateRequests()
{
    // Delivery can produce new requests (a paint that invalidates something
    // else); set the current set aside so those land in the next frame rather
    // than being dropped or looping forever.
    auto requestUpdateWindows = m_requestUpdateWindows;
    m_requestUpdateWindows.clear();

    m_inDeliverUpdateRequest = true;
    for (auto it = requestUpdateWindows.constBegin(); it != requestUpdateWindows.constEnd(); ++it)
        deliverUpdateRequest(it.key(), it.value());
    m_inDeliverUpdateRequest = false;
}

void QWkCompositor::deliverUpdateRequest(QWkWindow *window, UpdateRequestDeliveryType updateType)
{
    QWindow *qwindow = window->window();
    if (!qwindow)
        return;

    // No handleWindowDevicePixelRatioChanged() here, unlike the wasm plugin:
    // our DPR is pinned at 1 (see QWkScreen::devicePixelRatio).
    const QRect updateRect(QPoint(0, 0), qwindow->geometry().size());
    if (updateType == UpdateRequestDelivery) {
        // An unexposed window must be exposed first regardless, but it still
        // gets its updateRequest so its update bookkeeping stays balanced.
        if (!qwindow->isExposed())
            QWindowSystemInterface::handleExposeEvent(qwindow, updateRect);
        window->deliverUpdateRequest();
    } else {
        QWindowSystemInterface::handleExposeEvent(qwindow, updateRect);
    }
}

bool QWkCompositor::onFrame()
{
    QWkScreen *s = screen();
    if (!s)
        return false;

    // 1. Let windows repaint into their backing stores. Their flush() calls
    //    QFbWindow::repaint() -> QFbScreen::setDirty(), which is what feeds
    //    the composite below.
    deliverUpdateRequests();

    // Expose/update delivery above is synchronous only as far as QUEUEING the
    // window-system events; flush them so the paints actually happen now,
    // inside this frame, rather than on the next dispatcher pass.
    QWindowSystemInterface::sendWindowSystemEvents(QEventLoop::AllEvents);

    // 2. Composite every visible window into the single screen image. Damage
    //    may already have been consumed by a posted UpdateRequest earlier in
    //    this dispatcher pass — QWkScreen::doRedraw() accumulates it either
    //    way, which is what takeDamage() reads.
    s->doRedraw();

    if (!s->takeDamage())
        return false;

    // 3. Hand the composed surface to the host. RGBX8888 is already
    //    [r,g,b,a=255] in memory order, so this is a straight blit.
    const QImage &img = s->screenImage();
    if (img.isNull())
        return false;
    wkgfx_present(img.constBits(), uint32_t(img.width()), uint32_t(img.height()));
    return true;
}

QT_END_NAMESPACE
