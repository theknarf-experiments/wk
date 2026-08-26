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
// All this class does is arrange ONE place where the process blocks, waking on
// whichever comes first of: the host's next frame, the nearest QTimer
// deadline, and any socket a QSocketNotifier is watching.
//
// That single blocking call is ppoll(), and on wasip2 ppoll() IS
// wasi:io/poll.poll — libc's ppoll_impl asks each descriptor to contribute its
// pollables, appends a monotonic-clock.subscribe-duration for the timeout, and
// makes exactly one poll over the lot. The frame gets into that list by
// wearing a file descriptor (gfx-compat/wkgfx_poll.h); the sockets get in
// through wasi-libc's own tcp_poll_register. The inverse — pulling a pollable
// out of a socket fd so it could join wkgfx's hand-built list — is not
// possible, and wkgfx_poll.h records why at length.
//
// So the structure below is QEventDispatcherUNIX's, deliberately: a
// QHash<int, QSocketNotifierSetUNIX>, a QList<pollfd> with the frame fd LAST
// where the thread pipe would be, one qt_safe_poll, then revents mapped back
// to notifiers. Level-triggered, like poll(2) and like QSocketNotifier.
//
// Unlike the wasm port there is no QtCore-side half: QEventDispatcherWasm has
// to live in QtCore and be subclassed in the plugin only because it needs
// QtGui's sendWindowSystemEvents(). We link GuiPrivate, so one class does.
#ifndef QWKEVENTDISPATCHER_H
#define QWKEVENTDISPATCHER_H

#include <QtCore/qabstracteventdispatcher.h>
#include <QtCore/qhash.h>
#include <QtCore/qlist.h>
// QSocketNotifierSetUNIX (the three-slot Read/Write/Exception record) and its
// events() are header-only in here, and qcore_unix_p.h's qt_safe_poll /
// qt_make_pollfd come with it. Both files are already compiled into this
// port's libQt6Core.a — the UNIX dispatcher builds for wasip2, it just cannot
// be USED, because its wakeUp() needs a pipe and its poll set has no frame.
#include <QtCore/private/qeventdispatcher_unix_p.h>
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
    // The one blocking call. `timeoutNs < 0` means "no deadline of our own".
    // Returns the number of socket notifiers activated, and sets m_frameCredit
    // if the host's frame was what woke us.
    int pollOnce(qint64 timeoutNs, bool includeNotifiers);
    int activateSocketNotifiers();
    void pruneInvalidNotifiers();

    QWkScreen *m_screen;
    QWkInput m_input;
    QTimerInfoList m_timers;
    // Keyed by fd, like QEventDispatcherUNIXPrivate. m_pollfds is a member
    // rather than a local so the buffer is reused across iterations of a loop
    // that runs at frame rate.
    QHash<int, QSocketNotifierSetUNIX> m_socketNotifiers;
    QList<pollfd> m_pollfds;
    // Notifiers whose fd came back ready, waiting to be sent QEvent::SockAct.
    // Held as a member so unregisterSocketNotifier() can take a notifier back
    // out of it — which is what makes it safe to delete a QSocketNotifier from
    // inside its own activation, or from another notifier's.
    QList<QSocketNotifier *> m_pendingNotifiers;
    // Set when wkgfx_wait_frame_timeout() consumed a frame; cleared when that
    // frame is spent on a present. Presenting only against a credit is what
    // keeps the guest from outrunning the host, and it is the wkgfx contract.
    bool m_frameCredit = false;
    QAtomicInt m_interrupted = 0;
    QAtomicInt m_wakeUp = 0;
};

QT_END_NAMESPACE

#endif // QWKEVENTDISPATCHER_H
