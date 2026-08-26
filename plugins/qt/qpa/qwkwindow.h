// QWkWindow — a Qt top-level as one layer of the single wk surface.
//
// Almost all of this is QFbWindow and is deliberately inherited untouched:
// setVisible() (which forces the first top-level to fill the surface — exactly
// the shape a wk node wants), raise/lower (which re-order the compositor's
// z-stack and move focus with them), setGeometry (which emits the geometry
// and expose events), and repaint() (which turns a backing-store flush into
// screen damage).
//
// The one behaviour worth overriding is requestUpdate(): QPlatformWindow's
// default is a 5ms QTimer, which on a frame-paced surface means Qt animates
// out of step with the host and presents work that is immediately superseded.
// Routing it through QWkCompositor instead pins updates to the host frame.
#ifndef QWKWINDOW_H
#define QWKWINDOW_H

#include <private/qfbwindow_p.h>

QT_BEGIN_NAMESPACE

class QWkScreen;

class QWkWindow : public QFbWindow
{
public:
    explicit QWkWindow(QWindow *window);
    ~QWkWindow() override;

    QWkScreen *wkScreen() const;

    void requestUpdate() override;
    void requestActivateWindow() override;
};

QT_END_NAMESPACE

#endif // QWKWINDOW_H
