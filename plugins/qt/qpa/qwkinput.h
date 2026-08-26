// QWkInput — drain the wkgfx event queues into QWindowSystemInterface.
//
// The one rule that matters here: ALWAYS pass a null QWindow. QGuiApplication
// resolves null-window input itself, and does it better than a QPA plugin can:
//   * pointers — topLevelAt() for hit-testing, plus the press grab that keeps
//     a drag begun in window A going to A even when it leaves A's rect, plus
//     mapFromGlobal for the local point (qguiapplication.cpp:2384-2399);
//   * keys — routed to focusWindow() (qguiapplication.cpp:2545-2551).
// Every evdev/libinput backend does this (qlibinputpointer.cpp:51,
// qlibinputkeyboard.cpp:87). Re-implementing it in the plugin is how you get
// menus that swallow clicks and drags that stop at a window edge.
//
// State lives here rather than in the events because wkgfx reports one button
// per event while Qt wants the whole button mask, and because wkgfx pointer
// events carry no modifier flags at all — so the modifier state has to be
// tracked from the key stream.
#ifndef QWKINPUT_H
#define QWKINPUT_H

#include <QtCore/qpoint.h>
#include <QtCore/qnamespace.h>

QT_BEGIN_NAMESPACE

class QWkScreen;

class QWkInput
{
public:
    // Pump wkgfx_poll_event() until it runs dry, dispatching as we go. Fully
    // non-blocking: every get-* call in wkgfx_poll_event returns immediately
    // (wkgfx.c drains resize, pointer, key, scroll in that fixed order).
    // Returns the number of events dispatched.
    int drain(QWkScreen *screen);

private:
    QPointF m_pos;
    Qt::MouseButtons m_buttons = Qt::NoButton;
    Qt::KeyboardModifiers m_mods = Qt::NoModifier;
};

QT_END_NAMESPACE

#endif // QWKINPUT_H
