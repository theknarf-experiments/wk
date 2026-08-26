// QWkCompositor — the frame scheduler, not the compositor.
//
// The name is Qt's, kept for recognisability, and it is as misleading here as
// it is in Qt's wasm plugin: the actual compositing is QFbScreen::doRedraw()
// (see qwkscreen.h). What THIS class owns is the pairing of Qt's
// QWindow::requestUpdate() with the host's frame signal, which is the piece
// fbconvenience has no opinion about because linuxfb and vnc have no such
// signal — they redraw whenever they feel like it.
//
// wk does have one: a node presents in response to wkgfx_wait_frame(), and
// presenting faster than that just burns the guest's budget. So requestUpdate()
// coalesces per window into m_requestUpdateWindows, and the whole set is
// delivered on the next host frame in onFrame(), immediately before the
// composited image is presented. Animations therefore tick at the host's rate
// rather than QPlatformWindow's default 5ms timer.
//
// The requestUpdateWindow/deliverUpdateRequest logic is lifted from
// QWasmCompositor (qwasmcompositor.cpp:58-145) — including the
// ExposeEventDelivery -> UpdateRequestDelivery upgrade, which matters because
// QWindow subclasses require requested and delivered updateRequests to match
// exactly.
#ifndef QWKCOMPOSITOR_H
#define QWKCOMPOSITOR_H

#include <QtCore/qhash.h>
#include <QtCore/qobject.h>

QT_BEGIN_NAMESPACE

class QWkScreen;
class QWkWindow;
class QWindow;

class QWkCompositor : public QObject
{
    Q_OBJECT

public:
    enum UpdateRequestDeliveryType { ExposeEventDelivery, UpdateRequestDelivery };

    explicit QWkCompositor(QWkScreen *screen);
    ~QWkCompositor() override;

    void requestUpdateWindow(QWkWindow *window,
                             UpdateRequestDeliveryType updateType = ExposeEventDelivery);
    void handleBackingStoreFlush(QWindow *window);
    void windowDestroyed(QWkWindow *window);

    bool hasPendingUpdates() const { return !m_requestUpdateWindows.isEmpty(); }

    // Called by QWkEventDispatcher once per host frame credit: deliver the
    // pending update requests (which repaint into backing stores and mark the
    // screen dirty), let the screen composite, and present if anything moved.
    // Returns true when a frame was actually pushed to the host.
    bool onFrame();

    QWkScreen *screen() const;

private:
    void deliverUpdateRequests();
    void deliverUpdateRequest(QWkWindow *window, UpdateRequestDeliveryType updateType);

    QHash<QWkWindow *, UpdateRequestDeliveryType> m_requestUpdateWindows;
    bool m_inDeliverUpdateRequest = false;
};

QT_END_NAMESPACE

#endif // QWKCOMPOSITOR_H
