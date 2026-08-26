// qt-net — the QSocketNotifier test asset for the wk QPA plugin.
//
// A Qt application whose event loop is doing nothing but waiting, and which is
// woken by a SOCKET rather than by a frame or a timer. That is the whole
// claim, and it is why this is a separate node from qt-smoke:
//
//   * it never shows a window and the harness never pumps a frame, so the ONLY
//     thing that can wake QWkEventDispatcher's ppoll is the fd;
//   * it registers no QTimer either, so there is no deadline to wake on and
//     no way for a poll-from-a-timer implementation to fake this;
//   * it imports wasi:sockets, which makes it a "networked" node in wk's
//     sense — one that stays idle after spawn until the harness has wired it
//     to a Network and Run it. qt-smoke must not become one of those, because
//     its own test relies on spawn starting it.
//
// It talks raw BSD sockets rather than QTcpSocket because this Qt is built
// with -DFEATURE_network=OFF: QtNetwork is not in the sysroot. That costs
// nothing here — QSocketNotifier is QtCore, it watches an int, and
// QAbstractSocket is a customer of the same mechanism this is testing.
//
// Both halves of the notifier contract are exercised, in order:
//
//   Write  a non-blocking connect() in flight. wasi-libc registers the
//          tcp-socket's own pollable for a CONNECTING socket and completes
//          finish-connect in its poll_finish, reporting POLLWRNORM — so
//          "connected" arrives as a Write activation and nothing else.
//   Read   the reply. A CONNECTED socket registers its input stream's
//          pollable, and the notifier is level-triggered: it fires again on
//          every pass until the data is read, and once more at EOF.
//
// Every state change is printed, because the harness asserts on stdout.
#include <QtCore/QCoreApplication>
#include <QtCore/QSocketNotifier>
#include <QtCore/QtPlugin>
#include <QtGui/QGuiApplication>

#include <errno.h>
#include <fcntl.h>
#include <netdb.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#include <cstdarg>
#include <cstdio>

// No dlopen on wasm: the platform plugin is linked in and named here. Without
// it QGuiApplication aborts with an empty plugin list.
Q_IMPORT_PLUGIN(QWkIntegrationPlugin)

static void say(const char *fmt, ...) __attribute__((format(printf, 1, 2)));
static void say(const char *fmt, ...)
{
    va_list ap;
    va_start(ap, fmt);
    std::vfprintf(stdout, fmt, ap);
    va_end(ap);
    std::fputc('\n', stdout);
    std::fflush(stdout);
}

int main(int argc, char **argv)
{
    if (!qEnvironmentVariableIsSet("QT_QPA_PLATFORM"))
        qputenv("QT_QPA_PLATFORM", "wk");

    // QGuiApplication, not QCoreApplication: the wk event dispatcher is
    // installed by the QPA plugin, so a core-only app would get Qt's own
    // dispatcher and test nothing.
    QGuiApplication app(argc, argv);

    const QByteArray host = argc > 1 ? QByteArray(argv[1]) : QByteArray("netserve");
    const QByteArray port = argc > 2 ? QByteArray(argv[2]) : QByteArray("80");
    say("platform=%s target=%s:%s", qPrintable(QGuiApplication::platformName()),
        host.constData(), port.constData());

    // Resolve through the fabric's own name lookup: node names are hostnames.
    addrinfo hints;
    memset(&hints, 0, sizeof hints);
    hints.ai_family = AF_INET;
    hints.ai_socktype = SOCK_STREAM;
    addrinfo *res = nullptr;
    const int gai = ::getaddrinfo(host.constData(), port.constData(), &hints, &res);
    if (gai != 0 || !res) {
        say("SOCKET FAIL getaddrinfo %d", gai);
        return 1;
    }

    const int fd = ::socket(res->ai_family, res->ai_socktype, res->ai_protocol);
    if (fd < 0) {
        say("SOCKET FAIL socket %d", errno);
        return 1;
    }
    // Non-blocking, so connect() returns immediately and the completion has to
    // come back through the notifier. A blocking connect would prove the
    // fabric works but would say nothing about the dispatcher.
    if (::fcntl(fd, F_SETFL, O_NONBLOCK) != 0) {
        say("SOCKET FAIL fcntl %d", errno);
        return 1;
    }

    const int rc = ::connect(fd, res->ai_addr, res->ai_addrlen);
    ::freeaddrinfo(res);
    if (rc != 0 && errno != EINPROGRESS && errno != EAGAIN && errno != EALREADY) {
        say("SOCKET FAIL connect %d", errno);
        return 1;
    }
    say("SOCKET CONNECTING rc=%d errno=%d", rc, rc == 0 ? 0 : errno);

    // Owned by the application object so that quitting destroys them; the
    // read notifier is created only once the socket is up.
    auto *writeNotifier = new QSocketNotifier(fd, QSocketNotifier::Write, &app);
    QSocketNotifier *readNotifier = nullptr;
    QByteArray received;

    QObject::connect(writeNotifier, &QSocketNotifier::activated, [&]() {
        // Level-triggered: this fires on every pass while the socket has send
        // capacity, so the first thing it does is take itself out of the poll
        // set. Forgetting that is how a Write notifier turns an event loop
        // into a spin.
        writeNotifier->setEnabled(false);

        int err = 0;
        socklen_t len = sizeof err;
        if (::getsockopt(fd, SOL_SOCKET, SO_ERROR, &err, &len) != 0)
            err = errno;
        if (err != 0) {
            say("SOCKET FAIL so_error %d", err);
            app.exit(1);
            return;
        }
        say("SOCKET CONNECTED");

        static const char req[] = "GET / HTTP/1.0\r\n\r\n";
        const ssize_t n = ::send(fd, req, sizeof req - 1, 0);
        if (n != ssize_t(sizeof req - 1)) {
            say("SOCKET FAIL send %zd errno=%d", n, errno);
            app.exit(1);
            return;
        }
        say("SOCKET SENT %zd", n);

        readNotifier = new QSocketNotifier(fd, QSocketNotifier::Read, &app);
        QObject::connect(readNotifier, &QSocketNotifier::activated, [&]() {
            char buf[512];
            const ssize_t got = ::read(fd, buf, sizeof buf);
            if (got > 0) {
                received.append(buf, int(got));
                say("SOCKET READ %zd", got);
                return; // still level-triggered; come back for the rest
            }
            if (got < 0 && (errno == EAGAIN || errno == EWOULDBLOCK))
                return;
            // EOF (or a hard error): netserve closes after one reply.
            readNotifier->setEnabled(false);
            if (got < 0) {
                say("SOCKET FAIL read %d", errno);
                app.exit(1);
                return;
            }
            const QByteArray body = received.mid(received.indexOf("\r\n\r\n") + 4);
            say("SOCKET RECV %d [%s]", int(received.size()),
                body.trimmed().constData());
            app.quit();
        });
    });

    // Nothing else is registered: no window, no timer. From here the process
    // sits in ppoll() with [socket fd, frame fd] and no deadline, and the only
    // thing in the world that can wake it is the server answering.
    say("SOCKET WAITING");
    return app.exec();
}
