// QWkBackingStore — one window's pixels, on their way to the shared surface.
//
// QFbBackingStore already does everything correctly for us: it allocates
// mImage in the SCREEN's format (so setting QWkScreen::mFormat is enough to
// make every window RGBX8888 and the final present a straight blit), it
// paints into that QImage, and its flush() calls QFbWindow::repaint() which
// marks the screen dirty. There is no RHI path to worry about because
// QWkIntegration reports RhiBasedRendering=false.
//
// Two additions:
//   * scroll(), which QFbBackingStore leaves unimplemented. On a software
//     raster stack this is a large, cheap win for QScrollArea and every
//     QAbstractItemView — without it every scrolled pixel is repainted.
//   * flush() also tells the compositor a frame is owed, so a repaint that
//     originates outside an update request (a QWidget::update() between
//     frames) still reaches the host.
#ifndef QWKBACKINGSTORE_H
#define QWKBACKINGSTORE_H

#include <private/qfbbackingstore_p.h>

QT_BEGIN_NAMESPACE

class QWkBackingStore : public QFbBackingStore
{
public:
    explicit QWkBackingStore(QWindow *window);

    void flush(QWindow *window, const QRegion &region, const QPoint &offset) override;
    bool scroll(const QRegion &area, int dx, int dy) override;
};

QT_END_NAMESPACE

#endif // QWKBACKINGSTORE_H
