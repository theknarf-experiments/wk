#include "qwkinput.h"
#include "qwkkeytranslator.h"
#include "qwkscreen.h"

#include <QtGui/qevent.h>
#include <qpa/qwindowsysteminterface.h>

extern "C" {
#include "wkgfx.h"
}

QT_BEGIN_NAMESPACE

// wkgfx button ids, from wkgfx.h: 0 left, 1 middle, 2 right, 3 back,
// 4 forward. The first three match qwasmevent.h:169-180 exactly.
static Qt::MouseButton buttonFromWkgfx(int32_t b)
{
    switch (b) {
    case 0: return Qt::LeftButton;
    case 1: return Qt::MiddleButton;
    case 2: return Qt::RightButton;
    case 3: return Qt::BackButton;
    case 4: return Qt::ForwardButton;
    default: return Qt::NoButton;
    }
}

int QWkInput::drain(QWkScreen *screen)
{
    int n = 0;
    wkgfx_event ev;
    while (wkgfx_poll_event(&ev)) {
        ++n;
        switch (ev.type) {
        case WKGFX_RESIZE:
            if (screen)
                screen->handleResize(ev.width, ev.height);
            break;

        case WKGFX_POINTER_MOVE:
            m_pos = QPointF(ev.x, ev.y);
            QWindowSystemInterface::handleMouseEvent(nullptr, m_pos, m_pos, m_buttons,
                                                     Qt::NoButton, QEvent::MouseMove, m_mods);
            break;

        case WKGFX_POINTER_DOWN:
        case WKGFX_POINTER_UP: {
            // The down/up event carries its own position; trust it over the
            // last move, because wkgfx drains moves before buttons within a
            // frame and the two can disagree.
            m_pos = QPointF(ev.x, ev.y);
            const Qt::MouseButton b = buttonFromWkgfx(ev.button);
            const bool down = ev.type == WKGFX_POINTER_DOWN;
            if (down)
                m_buttons |= b;
            else
                m_buttons &= ~b;
            QWindowSystemInterface::handleMouseEvent(
                    nullptr, m_pos, m_pos, m_buttons, b,
                    down ? QEvent::MouseButtonPress : QEvent::MouseButtonRelease, m_mods);
            break;
        }

        case WKGFX_SCROLL: {
            const QPointF p(ev.x, ev.y);
            // wkgfx deltas are in LINES (wkgfx.h). Qt's angleDelta is in
            // eighths of a degree with 15 degrees to a notch, so one notch is
            // 120 units and one line is one notch.
            const QPoint angle(int(ev.dx * 120), int(ev.dy * 120));
            QWindowSystemInterface::handleWheelEvent(nullptr, p, p, QPoint(), angle, m_mods);
            break;
        }

        case WKGFX_KEY_DOWN:
        case WKGFX_KEY_UP: {
            Qt::Key key;
            QString text;
            Qt::KeyboardModifiers mods;
            QWkKeyTranslator::translate(ev, &key, &text, &mods);
            // Remember the modifier state for pointer and wheel events, which
            // arrive with no modifier fields of their own.
            m_mods = mods & ~Qt::KeypadModifier;
            QWindowSystemInterface::handleKeyEvent(
                    nullptr, ev.type == WKGFX_KEY_DOWN ? QEvent::KeyPress : QEvent::KeyRelease,
                    int(key), mods, text, ev.repeat != 0);
            break;
        }

        case WKGFX_NONE:
            break;
        }
    }
    return n;
}

QT_END_NAMESPACE
