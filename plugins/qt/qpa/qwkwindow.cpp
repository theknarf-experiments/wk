#include "qwkwindow.h"
#include "qwkcompositor.h"
#include "qwkscreen.h"

#include <QtGui/qscreen.h>
#include <QtGui/qwindow.h>
#include <qpa/qwindowsysteminterface.h>

QT_BEGIN_NAMESPACE

QWkWindow::QWkWindow(QWindow *window) : QFbWindow(window) { }

QWkWindow::~QWkWindow()
{
    if (QWkScreen *s = wkScreen()) {
        if (QWkCompositor *c = s->compositor())
            c->windowDestroyed(this);
    }
}

QWkScreen *QWkWindow::wkScreen() const
{
    QWindow *w = window();
    if (!w || !w->screen())
        return nullptr;
    return static_cast<QWkScreen *>(w->screen()->handle());
}

void QWkWindow::requestUpdate()
{
    QWkScreen *s = wkScreen();
    QWkCompositor *c = s ? s->compositor() : nullptr;
    if (!c) {
        QFbWindow::requestUpdate();
        return;
    }
    c->requestUpdateWindow(this, QWkCompositor::UpdateRequestDelivery);
}

void QWkWindow::requestActivateWindow()
{
    if (QWkScreen *s = wkScreen())
        s->raise(this);
    QWindowSystemInterface::handleFocusWindowChanged(window(), Qt::ActiveWindowFocusReason);
}

QT_END_NAMESPACE
