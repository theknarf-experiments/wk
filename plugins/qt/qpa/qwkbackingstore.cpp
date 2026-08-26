#include "qwkbackingstore.h"
#include "qwkcompositor.h"
#include "qwkscreen.h"

#include <QtGui/qscreen.h>
#include <QtGui/qwindow.h>

QT_BEGIN_NAMESPACE

// Exported from qtbase/src/gui/painting/qbackingstore.cpp but never declared
// in an installed header — QRasterBackingStore and QPixmap's raster backend
// both reach it with a local extern exactly like this.
extern void Q_GUI_EXPORT qt_scrollRectInImage(QImage &img, const QRect &rect, const QPoint &offset);

QWkBackingStore::QWkBackingStore(QWindow *window) : QFbBackingStore(window) { }

void QWkBackingStore::flush(QWindow *window, const QRegion &region, const QPoint &offset)
{
    QFbBackingStore::flush(window, region, offset);

    if (!window || !window->screen())
        return;
    QWkScreen *s = static_cast<QWkScreen *>(window->screen()->handle());
    if (QWkCompositor *c = s ? s->compositor() : nullptr)
        c->handleBackingStoreFlush(window);
}

bool QWkBackingStore::scroll(const QRegion &area, int dx, int dy)
{
    if (mImage.isNull())
        return false;

    // Copied from QOffscreenBackingStore::scroll
    // (qtbase/src/plugins/platforms/offscreen/qoffscreencommon.cpp).
    // qt_scrollRectInImage handles the overlap direction itself.
    lock();
    qt_scrollRectInImage(mImage, area.boundingRect(), QPoint(dx, dy));
    unlock();
    return true;
}

QT_END_NAMESPACE
