// QWkEventDispatcher — Qt's event loop, blocking on the wk frame.
//
// THE POINT: a wasip2 component is an ordinary program that owns its own
// stack. wkgfx_wait_frame() is not a callback the host invokes; it is a
// blocking call the guest makes. So Qt's loop needs no inversion, no
// trampolining and no asyncify — nested QDialog::exec(), QMenu popup loops
// and QDrag loops all just work, because processEvents() is re-entrant and
// its only state is a plain bool frame credit. That whole class of pain in
// Qt's emscripten port (qeventdispatcher_wasm_p.h:74-76's handleDialogExec)
// simply does not exist here.
//
// All this class does is replace select()/poll() with
// wkgfx_wait_frame_timeout(): ONE place where the process blocks, waking on
// either the host's next frame or the nearest QTimer deadline, whichever comes
// first. When socket notifiers are needed, they join the same wasi:io/poll
// list — which is precisely why the shim takes a timeout instead of blocking
// on a single pollable.
//
// Unlike the wasm port there is no QtCore-side half: QEventDispatcherWasm has
// to live in QtCore and be subclassed in the plugin only because it needs
// QtGui's sendWindowSystemEvents(). We link GuiPrivate, so one class does.
#ifndef QWKEVENTDISPATCHER_H
#define QWKEVENTDISPATCHER_H

#include <QtCore/qabstracteventdispatcher.h>
#include <QtCore/private/qtimerinfo_unix_p.h>

#include "qwkinput.h"

QT_BEGIN_NAMESPACE

class QWkScreen;

class QWkEventDispatcher : public QAbstractEventDispatcherV2
{
    Q_OBJECT

public:
    explicit QWkEventDispatcher(QWkScreen *screen, QObject *parent = nullptr);
    ~QWkEventDispatcher() override;

    bool processEvents(QEventLoop::ProcessEventsFlags flags) override;

    void registerSocketNotifier(QSocketNotifier *notifier) override;
    void unregisterSocketNotifier(QSocketNotifier *notifier) override;

    void registerTimer(Qt::TimerId timerId, Duration interval, Qt::TimerType timerType,
                       QObject *object) final;
    bool unregisterTimer(Qt::TimerId timerId) final;
    bool unregisterTimers(QObject *object) final;
    QList<TimerInfoV2> timersForObject(QObject *object) const final;
    Duration remainingTime(Qt::TimerId timerId) const final;

    void wakeUp() override;
    void interrupt() override;

private:
    QWkScreen *m_screen;
    QWkInput m_input;
    QTimerInfoList m_timers;
    // Set when wkgfx_wait_frame_timeout() consumed a frame; cleared when that
    // frame is spent on a present. Presenting only against a credit is what
    // keeps the guest from outrunning the host, and it is the wkgfx contract.
    bool m_frameCredit = false;
    QAtomicInt m_interrupted = 0;
    QAtomicInt m_wakeUp = 0;
    bool m_warnedSocketNotifier = false;
};

QT_END_NAMESPACE

#endif // QWKEVENTDISPATCHER_H
