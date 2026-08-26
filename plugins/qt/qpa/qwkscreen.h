// QWkScreen — the one wk surface, and the N Qt windows composited onto it.
//
// This is where decision #1 lives: a wk node gets exactly ONE wasi-gfx
// surface, so every Qt top-level (the main window, its menus, its modal
// dialogs, its tooltips and combo popups) is blitted GUEST-SIDE into a single
// QImage which is then handed to wkgfx_present().
//
// We do not write that compositor. qtbase already ships it, unconditionally,
// for every platform: src/platformsupport/fbconvenience's QFbScreen walks a
// z-ordered window stack back-to-front, blits each window's backing store
// into one QImage clipped to an accumulated damage QRegion, and returns the
// region it touched. linuxfb, vnc and integrityfb are all thin subclasses of
// it; so are we. (Qt 6.8's `wasm` plugin is NOT the model to copy — despite
// the name, QWasmCompositor composites nothing: every QWasmWindow owns its
// own DOM <canvas> and the "compositor" is only an update-request scheduler.)
//
// What this subclass adds on top of QFbScreen is exactly three things:
//   * the surface's pixel format and geometry, taken from wkgfx;
//   * damage accumulation across doRedraw() calls, so QWkCompositor knows
//     whether a present is worth making;
//   * handleResize(), the host-is-authoritative resize path.
#ifndef QWKSCREEN_H
#define QWKSCREEN_H

#include <QtGui/qimage.h>
#include <QtGui/qregion.h>

#include <private/qfbscreen_p.h>

QT_BEGIN_NAMESPACE

class QWkCompositor;

class QWkScreen : public QFbScreen
{
    Q_OBJECT

public:
    QWkScreen();
    ~QWkScreen() override;

    bool initialize() override;

    QString name() const override { return QStringLiteral("wk"); }
    QDpi logicalDpi() const override { return QDpi(96, 96); }
    QDpi logicalBaseDpi() const override { return QDpi(96, 96); }
    // wk surfaces are addressed in physical pixels and the host does its own
    // scaling on present, so Qt must not apply a second factor. A user who
    // wants bigger widgets sets QT_SCALE_FACTOR.
    qreal devicePixelRatio() const override { return 1.0; }

    // The HOST draws the pointer, so we deliberately have no QPlatformCursor:
    // that also drops QFbCursor's QInputDeviceManager dependency. QFbScreen
    // guards every mCursor access (qfbscreen.cpp:167/171/201), so a null one
    // is a supported configuration rather than a hole. The cost is that
    // cursor SHAPE cannot be requested — no I-beam over a QLineEdit — until
    // wasi:surface grows a cursor-shape call.
    QPlatformCursor *cursor() const override { return nullptr; }

    Flags flags() const override;

    // The host resized the node. QFbScreen::setGeometry reallocates the
    // screen image, emits handleScreenGeometryChange and re-lays-out maximized
    // windows; all we add is marking the whole new surface dirty.
    void handleResize(uint32_t width, uint32_t height);

    // Damage accumulated since the last take. QFbScreen::doRedraw() is driven
    // from a posted QEvent::UpdateRequest, i.e. from inside sendPostedEvents,
    // NOT from our frame callback — so the compositor cannot simply use
    // doRedraw()'s return value and has to collect it here instead.
    bool takeDamage();

    const QImage &screenImage() const { return mScreenImage; }
    QWkCompositor *compositor() const { return m_compositor; }

    // Public, unlike QFbScreen's protected original: QWkCompositor drives a
    // composite explicitly once per host frame, on top of the ones QFbScreen
    // triggers itself from its posted UpdateRequest.
    QRegion doRedraw() override;

private:
    QWkCompositor *m_compositor = nullptr;
    QRegion m_damage;
};

QT_END_NAMESPACE

#endif // QWKSCREEN_H
