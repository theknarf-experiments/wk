#include "qwkeventdispatcher.h"
#include "qwkcompositor.h"
#include "qwkscreen.h"

#include <QtCore/qcoreapplication.h>
#include <QtCore/qsocketnotifier.h>
#include <QtCore/private/qcoreapplication_p.h>
#include <QtCore/private/qthread_p.h>

#include <qpa/qwindowsysteminterface.h>

extern "C" {
#include "wkgfx.h"
}

QT_BEGIN_NAMESPACE

QWkEventDispatcher::QWkEventDispatcher(QWkScreen *screen, QObject *parent)
    : QAbstractEventDispatcherV2(parent), m_screen(screen)
{
}

QWkEventDispatcher::~QWkEventDispatcher()
{
    m_timers.clearTimers();
}

bool QWkEventDispatcher::processEvents(QEventLoop::ProcessEventsFlags flags)
{
    m_interrupted.storeRelaxed(0);
    emit awake();

    int nevents = 0;

    // 1. Host input. Non-blocking; every wkgfx get-* returns immediately.
    nevents += m_input.drain(m_screen);

    // 2. Qt's own posted-event queue. This is where the QEvent::UpdateRequest
    //    that QFbScreen::scheduleUpdate() posted to itself is delivered, i.e.
    //    where compositing would happen off a frame.
    QThreadData *threadData = QThreadData::current();
    QCoreApplicationPrivate::sendPostedEvents(nullptr, 0, threadData);

    // 3. The window-system queue the input above filled (we leave
    //    setSynchronousWindowSystemEvents at its default false, like linuxfb
    //    and vnc, so this is the explicit flush that makes it synchronous
    //    enough).
    if (QWindowSystemInterface::sendWindowSystemEvents(flags))
        ++nevents;

    // 4. Spend a frame credit, if we are holding one: deliver update
    //    requests, composite, present.
    if (m_frameCredit) {
        m_frameCredit = false;
        if (QWkCompositor *c = m_screen ? m_screen->compositor() : nullptr) {
            if (c->onFrame())
                ++nevents;
        }
    }

    // 5. Timers.
    if ((flags & QEventLoop::X11ExcludeTimers) == 0)
        nevents += m_timers.activateTimers();

    // 6. Block, if we are allowed to and there is nothing else to do.
    const bool wakeRequested = m_wakeUp.fetchAndStoreRelaxed(0) != 0;
    const bool canWait = threadData->canWaitLocked() && !m_interrupted.loadRelaxed()
            && !wakeRequested && (flags & QEventLoop::WaitForMoreEvents);

    // Pending update requests are NOT a reason to skip the block: the next
    // host frame is exactly when they are supposed to be delivered, so
    // blocking on it is the correct thing and busy-looping instead would just
    // burn the guest's budget.
    if (canWait) {
        emit aboutToBlock();
        std::optional<Duration> remaining;
        if ((flags & QEventLoop::X11ExcludeTimers) == 0)
            remaining = m_timers.timerWait();
        // -1 means "no deadline of our own": block until the host's frame.
        const qint64 ns = remaining ? qint64(remaining->count()) : qint64(-1);
        m_frameCredit = wkgfx_wait_frame_timeout(ns) != 0;
    } else if (m_screen && m_screen->compositor() && m_screen->compositor()->hasPendingUpdates()) {
        // Work is queued for the next frame but we are not allowed to block.
        // Take a frame if one is already sitting there, otherwise carry on —
        // never sleep here, ProcessEventsFlags said not to.
        m_frameCredit = m_frameCredit || wkgfx_wait_frame_timeout(0) != 0;
    }

    return nevents > 0;
}

void QWkEventDispatcher::registerSocketNotifier(QSocketNotifier *notifier)
{
    Q_UNUSED(notifier);
    if (!m_warnedSocketNotifier) {
        m_warnedSocketNotifier = true;
        qWarning("QWkEventDispatcher: socket notifiers are not implemented. Async socket I/O "
                 "(QLocalSocket, QTcpSocket, QNetworkAccessManager) will never become ready. "
                 "The fix is to add the fd's wasi:io pollable to the same poll() list as the "
                 "frame pollable and the timer deadline -- see qwkeventdispatcher.cpp -- not to "
                 "poll it from a QTimer.");
    }
}

void QWkEventDispatcher::unregisterSocketNotifier(QSocketNotifier *notifier)
{
    Q_UNUSED(notifier);
}

void QWkEventDispatcher::registerTimer(Qt::TimerId timerId, Duration interval,
                                       Qt::TimerType timerType, QObject *object)
{
    m_timers.registerTimer(timerId, interval, timerType, object);
}

bool QWkEventDispatcher::unregisterTimer(Qt::TimerId timerId)
{
    return m_timers.unregisterTimer(timerId);
}

bool QWkEventDispatcher::unregisterTimers(QObject *object)
{
    return m_timers.unregisterTimers(object);
}

QList<QWkEventDispatcher::TimerInfoV2> QWkEventDispatcher::timersForObject(QObject *object) const
{
    return m_timers.registeredTimers(object);
}

QWkEventDispatcher::Duration QWkEventDispatcher::remainingTime(Qt::TimerId timerId) const
{
    return m_timers.remainingDuration(timerId);
}

void QWkEventDispatcher::wakeUp()
{
    // With FEATURE_thread=OFF there is no other thread to wake, so this can
    // only ever be re-entrant: something inside our own call stack posted an
    // event while we were about to block. A flag read at the top of the block
    // decision is the whole implementation — no eventfd, no self-pipe (which
    // is just as well: pipe() on a stock wasip2 host returns ENOTSUP).
    m_wakeUp.storeRelaxed(1);
}

void QWkEventDispatcher::interrupt()
{
    m_interrupted.storeRelaxed(1);
    wakeUp();
}

QT_END_NAMESPACE
