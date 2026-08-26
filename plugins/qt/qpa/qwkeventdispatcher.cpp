#include "qwkeventdispatcher.h"
#include "qwkcompositor.h"
#include "qwkscreen.h"

#include <QtCore/qcoreapplication.h>
#include <QtCore/qdeadlinetimer.h>
#include <QtCore/qsocketnotifier.h>
#include <QtCore/private/qcoreapplication_p.h>
#include <QtCore/private/qcore_unix_p.h>
#include <QtCore/private/qthread_p.h>

#include <qpa/qwindowsysteminterface.h>

#include <chrono>
#include <utility>

#include <errno.h>
#include <string.h>

extern "C" {
#include "wkgfx.h"
#include "wkgfx_poll.h"
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
    const bool includeNotifiers = (flags & QEventLoop::ExcludeSocketNotifiers) == 0;

    // Pending update requests are NOT a reason to skip the block: the next
    // host frame is exactly when they are supposed to be delivered, so
    // blocking on it is the correct thing and busy-looping instead would just
    // burn the guest's budget.
    if (canWait) {
        emit aboutToBlock();
        std::optional<Duration> remaining;
        if ((flags & QEventLoop::X11ExcludeTimers) == 0)
            remaining = m_timers.timerWait();
        // -1 means "no deadline of our own": block until the host's frame or
        // until a watched socket moves.
        const qint64 ns = remaining ? qint64(remaining->count()) : qint64(-1);
        nevents += pollOnce(ns, includeNotifiers);
    } else if ((includeNotifiers && !m_socketNotifiers.isEmpty())
               || (m_screen && m_screen->compositor()
                   && m_screen->compositor()->hasPendingUpdates())) {
        // Not allowed to sleep, but there is something whose readiness we
        // still owe an answer about: a frame already sitting there, or a
        // socket that may already be readable. A zero deadline gives
        // QSocketNotifier its level-triggered semantics under
        // processEvents(0) — which is what QAbstractSocket::waitFor*() and
        // any spin loop rely on. Never sleep here; ProcessEventsFlags said no.
        nevents += pollOnce(0, includeNotifiers);
    }

    return nevents > 0;
}

// The one place this process blocks.
//
// `timeoutNs < 0` is "no deadline of our own". The pollfd list is the socket
// notifiers plus the frame fd LAST, which is where QEventDispatcherUNIX puts
// its thread pipe and is why it is popped off the end below.
int QWkEventDispatcher::pollOnce(qint64 timeoutNs, bool includeNotifiers)
{
    // A descriptor wrapping the wk frame pollable. Cheap and cached; see
    // gfx-compat/wkgfx_poll.h for why the frame becomes an fd rather than the
    // sockets becoming pollables.
    const int frameFd = wkgfx_frame_fd();
    if (frameFd < 0) {
        // Only reachable before wkgfx_open(), which QWkIntegration does in its
        // constructor — so in practice, never. Degrade to the frame-only wait
        // rather than hand ppoll a list it cannot block on.
        m_frameCredit = (wkgfx_wait_frame_timeout(timeoutNs) != 0) || m_frameCredit;
        return 0;
    }

    m_pollfds.clear();
    m_pollfds.reserve(1 + (includeNotifiers ? m_socketNotifiers.size() : 0));
    if (includeNotifiers) {
        for (auto it = m_socketNotifiers.cbegin(); it != m_socketNotifiers.cend(); ++it)
            m_pollfds.append(qt_make_pollfd(it.key(), it.value().events()));
    }
    // ExcludeSocketNotifiers drops the notifiers but never the frame: the
    // frame is this dispatcher's wakeup channel, not a socket.
    m_pollfds.append(qt_make_pollfd(frameFd, POLLIN));

    const QDeadlineTimer deadline = timeoutNs < 0
            ? QDeadlineTimer(QDeadlineTimer::Forever)
            : QDeadlineTimer(std::chrono::nanoseconds(timeoutNs));

    const int ready = qt_safe_poll(m_pollfds.data(), nfds_t(m_pollfds.size()), deadline);
    if (ready < 0) {
        // On this platform one unpollable descriptor fails the WHOLE poll,
        // frame fd included — see pruneInvalidNotifiers for why, and for the
        // one-pass repair that stops it from freezing the UI.
        qErrnoWarning("QWkEventDispatcher: poll");
        m_pollfds.clear();
        pruneInvalidNotifiers();
        return 0;
    }

    const pollfd framePfd = m_pollfds.takeLast();
    if (ready == 0) {
        m_pollfds.clear();
        return 0; // the timer deadline expired; step 5 will run the timers
    }

    if (framePfd.revents != 0) {
        // The host's frame readiness was already consumed by the poll itself
        // (wk's frame pollable is one-shot). What is still owed is get-frame,
        // which is where a CLOSED surface is reported — by trapping, which is
        // how the node exits. Skipping it would spin forever on a closed
        // window.
        if (wkgfx_take_frame())
            m_frameCredit = true;
    }

    return includeNotifiers ? activateSocketNotifiers() : 0;
}

// QEventDispatcherUNIXPrivate::markPendingSocketNotifiers +
// activateSocketNotifiers, fused. Level-triggered, exactly like poll(2): a
// notifier is activated on every pass where its fd is ready, and stays ready
// until the application consumes the condition (reads the data, drains the
// write buffer) or disables the notifier. Qt's own socket code relies on that
// — QAbstractSocketPrivate turns its write notifier off when the buffer
// drains, which is the only thing standing between a writable socket and a
// busy loop.
int QWkEventDispatcher::activateSocketNotifiers()
{
    for (const pollfd &pfd : std::as_const(m_pollfds)) {
        if (pfd.fd < 0 || pfd.revents == 0)
            continue;
        auto it = m_socketNotifiers.constFind(pfd.fd);
        if (it == m_socketNotifiers.cend())
            continue;

        // POLLHUP/POLLERR wake every type: a hung-up socket is readable
        // (EOF), writable (EPIPE) and exceptional all at once, and a notifier
        // that slept through it would hang. POLLPRI is wasi's one dead letter
        // — the platform never reports out-of-band data, so an Exception
        // notifier only ever fires on hangup or error. Qt's networking never
        // uses Exception, so nothing here depends on that.
        static const struct {
            QSocketNotifier::Type type;
            short flags;
        } kinds[] = {
            { QSocketNotifier::Read,      POLLIN  | POLLHUP | POLLERR },
            { QSocketNotifier::Write,     POLLOUT | POLLHUP | POLLERR },
            { QSocketNotifier::Exception, POLLPRI | POLLHUP | POLLERR },
        };

        for (const auto &kind : kinds) {
            QSocketNotifier *notifier = it.value().notifiers[kind.type];
            if (!notifier)
                continue;
            if (pfd.revents & POLLNVAL) {
                qWarning("QWkEventDispatcher: invalid socket %d, disabling its notifier", pfd.fd);
                notifier->setEnabled(false);
                continue;
            }
            if ((pfd.revents & kind.flags) && !m_pendingNotifiers.contains(notifier))
                m_pendingNotifiers.append(notifier);
        }
    }

    // Clear BEFORE dispatching: sendEvent() runs application code that may
    // register, unregister or delete notifiers, and this list must not be
    // walked again afterwards.
    m_pollfds.clear();

    int activated = 0;
    QEvent event(QEvent::SockAct);
    while (!m_pendingNotifiers.isEmpty()) {
        // takeFirst() before sendEvent(), and unregisterSocketNotifier()
        // removes from this same list — so a notifier deleted from inside its
        // own activation, or from a sibling's, is gone from here before it
        // could be dereferenced.
        QSocketNotifier *notifier = m_pendingNotifiers.takeFirst();
        QCoreApplication::sendEvent(notifier, &event);
        ++activated;
    }
    return activated;
}

// Damage control for a poll that failed outright. Only reached from pollOnce.
//
// On Linux one unpollable descriptor comes back as POLLNVAL on its own entry
// and the rest of the poll is unaffected. On wasip2 it is fatal to the WHOLE
// call: libc's ppoll_impl bails the moment descriptor_table_get misses (a
// closed fd) or a descriptor's poll_register refuses (wasi-libc's
// tcp_poll_register answers ENOTSUP for a socket that is neither connecting,
// listening, connected nor failed — an unbound one, say). Either way the frame
// fd goes down with it and the UI freezes while the loop spins on the error.
//
// So poll each notifier's fd on its own, with a zero deadline, and disable the
// notifiers of every fd that cannot be polled at all. One pass, no retry, and
// the surviving notifiers keep working.
void QWkEventDispatcher::pruneInvalidNotifiers()
{
    QList<QSocketNotifier *> doomed;
    for (auto it = m_socketNotifiers.cbegin(); it != m_socketNotifiers.cend(); ++it) {
        pollfd probe = qt_make_pollfd(it.key(), it.value().events());
        // A default-constructed QDeadlineTimer has already expired: poll, do
        // not wait.
        if (qt_safe_poll(&probe, 1, QDeadlineTimer()) >= 0)
            continue;
        qWarning("QWkEventDispatcher: socket %d cannot be polled (%s); disabling its notifiers",
                 it.key(), strerror(errno));
        for (QSocketNotifier *n : it.value().notifiers) {
            if (n)
                doomed.append(n);
        }
    }
    // setEnabled(false) calls back into unregisterSocketNotifier, which
    // mutates m_socketNotifiers — hence the two passes.
    for (QSocketNotifier *n : std::as_const(doomed))
        n->setEnabled(false);
}

void QWkEventDispatcher::registerSocketNotifier(QSocketNotifier *notifier)
{
    Q_ASSERT(notifier);
    const int sockfd = notifier->socket();
    const QSocketNotifier::Type type = notifier->type();

    QSocketNotifierSetUNIX &set = m_socketNotifiers[sockfd];
    if (set.notifiers[type] && set.notifiers[type] != notifier) {
        qWarning("QWkEventDispatcher: multiple socket notifiers for the same socket %d and type %d",
                 sockfd, int(type));
    }
    set.notifiers[type] = notifier;
}

void QWkEventDispatcher::unregisterSocketNotifier(QSocketNotifier *notifier)
{
    Q_ASSERT(notifier);

    // This runs from ~QSocketNotifier() (via setEnabled(false)), possibly from
    // inside the very QEvent::SockAct delivery above, so dropping it from the
    // pending list is not tidiness — it is what stops activateSocketNotifiers
    // from sending an event to a destroyed object.
    m_pendingNotifiers.removeOne(notifier);

    const auto it = m_socketNotifiers.find(notifier->socket());
    if (it == m_socketNotifiers.end())
        return;
    QSocketNotifierSetUNIX &set = it.value();
    if (set.notifiers[notifier->type()] != notifier)
        return;
    set.notifiers[notifier->type()] = nullptr;
    if (set.isEmpty())
        m_socketNotifiers.erase(it);
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
