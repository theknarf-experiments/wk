// qt-qtnetwork — QtNetwork itself, talking to another wk node over the fabric.
//
// The sibling node qt-net (../net/main.cpp) proves the LOWER half: that a raw
// fd can wake QWkEventDispatcher through QSocketNotifier. This one proves the
// UPPER half — that Qt's own networking classes, built for a genuine WASI
// platform with -DFEATURE_network=ON, are customers of that mechanism and work
// end to end against a real peer. Four stages, in ascending order of how much
// of Qt they drag in, so a failure names its own layer:
//
//   DNS   QHostInfo::lookupHost(). A wk node's peers are addressed by NODE
//         NAME, and that name is answered by the fabric's own resolver
//         through wasi:sockets ip-name-lookup (plugins/fetch/fetch.c is the
//         no-Qt reference for the same call). With FEATURE_thread=OFF,
//         qhostinfo.cpp's QThreadPool path compiles out and the lookup runs
//         inline -- but the RESULT still arrives as a posted event, so this
//         already requires a working event loop.
//   TCP   QTcpSocket, driven purely by connected/readyRead/disconnected. No
//         waitForConnected(), no waitForReadyRead(): those would block inside
//         qt_safe_poll() and would prove only that ppoll works. Going through
//         the signals is what puts QAbstractSocket's read and write notifiers
//         in the dispatcher's poll set.
//   HTTP  QNetworkAccessManager::get(). Upstream this whole stack is excluded
//         when QT_FEATURE_thread is 0 -- Qt 6.8 runs QHttpThreadDelegate on a
//         QThread the manager creates. patches/qtbase-0009 makes the delegate
//         live on the calling thread instead, which is the CORRECT semantics
//         for a single-threaded process rather than an approximation: with
//         QT_CONFIG(thread) off, qobject.cpp compiles the
//         BlockingQueuedConnection arm out entirely, so those emits become
//         direct calls and the QueuedConnections go through this event loop.
//         If that reasoning is wrong, it fails HERE.
//   TLS   ...is a NEGATIVE stage. QT_FEATURE_ssl is 0 here (no
//         SecureTransport off-Apple, no Schannel, no OpenSSL cross-built for
//         wasm32-wasip2), and the question that matters is not "is TLS
//         missing" -- the build says so -- but whether https:// then silently
//         DOWNGRADES to cleartext, which would be a security hole rather than
//         a missing feature. It does not: qnetworkaccessmanager.cpp lists
//         u"https" in its httpScheme table only `#ifndef QT_NO_SSL`, so the
//         URL falls through to the generic backend factory and errors. This
//         stage aims that https:// at the *plaintext* peer, so a downgrade
//         would SUCCEED and fail the stage.
//
// Every line is asserted on by the wk-server test
// `qt_network_speaks_to_a_wk_node`, so the prefixes are a contract.
#include <QtCore/QCoreApplication>
#include <QtCore/QTimer>
#include <QtGui/QGuiApplication>
#include <QtNetwork/QHostInfo>
#include <QtNetwork/QDnsLookup>
#include <QtNetwork/QNetworkAccessManager>
#include <QtNetwork/QNetworkReply>
#include <QtNetwork/QNetworkRequest>
#include <QtNetwork/QTcpSocket>
#include <QtCore/QtPlugin>

#include <cstdarg>
#include <cstdio>

// No dlopen on wasm: the platform plugin is linked in and named here.
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

    // QGuiApplication, not QCoreApplication: the dispatcher under test is
    // installed by the wk QPA plugin. A core-only app would get Qt's own
    // QEventDispatcherUNIX and would prove nothing about this port.
    QGuiApplication app(argc, argv);

    const QString host = argc > 1 ? QString::fromLocal8Bit(argv[1]) : QStringLiteral("netserve");
    const quint16 port = argc > 2 ? quint16(QByteArray(argv[2]).toUShort()) : quint16(80);
    say("NET START platform=%s peer=%s:%u", qPrintable(QGuiApplication::platformName()),
        qPrintable(host), unsigned(port));

#if QT_CONFIG(ssl)
    say("TLS BUILT");
#else
    // Not a failure — a fact. QT_FEATURE_ssl is 0 in this sysroot.
    say("TLS ABSENT");
#endif

    // A watchdog, so a stall is reported as a stall instead of as the harness
    // timing out with no diagnosis. It is a QTimer, which means this node --
    // unlike qt-net -- does wake periodically; that is fine, because what is
    // under test here is QtNetwork, not whether the dispatcher can sleep.
    QTimer watchdog;
    watchdog.setSingleShot(true);
    QObject::connect(&watchdog, &QTimer::timeout, [&app]() {
        say("NET FAIL watchdog: no progress in 120s");
        app.exit(1);
    });
    watchdog.start(120'000);

    // --- stage 5 (declared first so stage 4 can call it) ------------------
    // QDnsLookup: the RECORD types getaddrinfo cannot express. Stage 1 already
    // resolved a name to an address, and that path never touches this one --
    // QHostInfo goes through getaddrinfo and so through wk's fabric name
    // service, whereas QDnsLookup builds a DNS query, puts it on the wire, and
    // parses the response. On this port that means Qt's own
    // qdnslookup_unix.cpp over plugins/resolv-compat, and it is only reachable
    // at all because QDnsLookup is no longer gated on QT_FEATURE_thread (see
    // plugins/qt/patches/qtbase-0010-dnslookup-without-threads.patch).
    //
    // The peer is plugins/dnsstub, which is authoritative for wk.test and
    // nothing else, so every field asserted here is one this repo wrote --
    // no internet, no records somebody else controls.
    const QString dnsHost = argc > 3 ? QString::fromLocal8Bit(argv[3]) : QStringLiteral("dnsstub");
    auto dnsStage = [&app, dnsHost]() {
        auto *mx = new QDnsLookup(QDnsLookup::MX, QStringLiteral("wk.test"), &app);
        QObject::connect(mx, &QDnsLookup::finished, [&app, mx]() {
            mx->deleteLater();
            if (mx->error() != QDnsLookup::NoError) {
                say("DNSREC FAIL %d %s", int(mx->error()), qPrintable(mx->errorString()));
                app.exit(1);
                return;
            }
            const auto records = mx->mailExchangeRecords();
            if (records.isEmpty()) {
                say("DNSREC FAIL no MX records");
                app.exit(1);
                return;
            }
            // Both fields matter: the exchange proves dn_expand walked the
            // name, the preference proves the RDATA offset was right.
            say("DNSREC MX %s pref=%u", qPrintable(records.first().exchange()),
                unsigned(records.first().preference()));
            app.quit();
        });
        say("DNSREC LOOKUP MX wk.test via %s", qPrintable(dnsHost));
        // Resolve the stub's own address through the fabric first: QDnsLookup
        // wants a nameserver ADDRESS, and the node is known by name.
        const QHostInfo ns = QHostInfo::fromName(dnsHost);
        if (ns.error() != QHostInfo::NoError || ns.addresses().isEmpty()) {
            say("DNSREC FAIL cannot resolve nameserver %s", qPrintable(dnsHost));
            app.exit(1);
            return;
        }
        say("DNSREC NAMESERVER %s", qPrintable(ns.addresses().first().toString()));
        mx->setNameserver(ns.addresses().first());
        mx->lookup();
    };

    // --- stage 4 (declared first so stage 3 can call it) ------------------
    // Does https:// silently DOWNGRADE to cleartext when there is no TLS
    // backend? That is the failure mode worth checking, because it would be
    // silent and it would be a security hole rather than a missing feature.
    // It does not: qnetworkaccessmanager.cpp lists u"https" in its httpScheme
    // table only `#ifndef QT_NO_SSL`, so with QT_FEATURE_ssl 0 the URL falls
    // through to the generic backend factory, finds nothing, and errors. This
    // stage proves that rather than trusting the reading. The port is the
    // *plaintext* peer on purpose: if the request were downgraded it would
    // succeed, and this stage would fail.
    auto *nam = new QNetworkAccessManager(&app);
    auto tlsStage = [&app, nam, host, port, dnsStage]() {
        const QUrl url(QStringLiteral("https://%1:%2/").arg(host).arg(port));
        say("TLS GET %s", qPrintable(url.toString()));
        QNetworkReply *reply = nam->get(QNetworkRequest(url));
        QObject::connect(reply, &QNetworkReply::finished, [&app, reply, dnsStage]() {
            reply->deleteLater();
            if (reply->error() == QNetworkReply::NoError) {
                say("TLS FAIL https succeeded with no TLS backend -- downgrade?");
                app.exit(1);
                return;
            }
            say("TLS REJECTED %d %s", int(reply->error()),
                qPrintable(reply->errorString()));
            dnsStage();
        });
    };

    // --- stage 3 ----------------------------------------------------------
    auto httpStage = [&app, nam, host, port, tlsStage]() {
        const QUrl url(QStringLiteral("http://%1:%2/").arg(host).arg(port));
        say("HTTP GET %s", qPrintable(url.toString()));
        QNetworkReply *reply = nam->get(QNetworkRequest(url));
        QObject::connect(reply, &QNetworkReply::finished, [&app, reply, tlsStage]() {
            reply->deleteLater();
            if (reply->error() != QNetworkReply::NoError) {
                say("HTTP FAIL %d %s", int(reply->error()),
                    qPrintable(reply->errorString()));
                app.exit(1);
                return;
            }
            const int status =
                reply->attribute(QNetworkRequest::HttpStatusCodeAttribute).toInt();
            const QByteArray body = reply->readAll();
            say("HTTP STATUS %d", status);
            say("HTTP RECV %d [%s]", int(body.size()),
                body.trimmed().constData());
            tlsStage();
        });
    };

    // --- stage 2 ----------------------------------------------------------
    auto *sock = new QTcpSocket(&app);
    auto *received = new QByteArray;
    auto tcpStage = [&app, sock, received, httpStage, host, port]() {
        QObject::connect(sock, &QTcpSocket::connected, [sock]() {
            say("TCP CONNECTED peer=%s:%u", qPrintable(sock->peerAddress().toString()),
                unsigned(sock->peerPort()));
            // A write with no flush and no waitForBytesWritten: it leaves the
            // socket's WRITE notifier armed and drains through the dispatcher.
            sock->write("GET / HTTP/1.0\r\n\r\n");
        });
        QObject::connect(sock, &QTcpSocket::readyRead, [sock, received]() {
            const QByteArray chunk = sock->readAll();
            received->append(chunk);
            say("TCP READ %d", int(chunk.size()));
        });
        QObject::connect(sock, &QTcpSocket::disconnected, [received, httpStage]() {
            const QByteArray body = received->mid(received->indexOf("\r\n\r\n") + 4);
            say("TCP RECV %d [%s]", int(received->size()), body.trimmed().constData());
            httpStage();
        });
        QObject::connect(sock, &QTcpSocket::errorOccurred,
                         [&app, sock](QAbstractSocket::SocketError e) {
            // netserve answers HTTP/1.0 and hangs up, which QAbstractSocket
            // reports as RemoteHostClosedError alongside disconnected(). That
            // is the expected end of this exchange, not a failure.
            if (e == QAbstractSocket::RemoteHostClosedError)
                return;
            say("TCP FAIL %d %s", int(e), qPrintable(sock->errorString()));
            app.exit(1);
        });
        say("TCP CONNECTING");
        // By NAME. QAbstractSocket runs this through QHostInfo, so the
        // fabric's resolver is on the critical path of the socket too.
        sock->connectToHost(host, port);
    };

    // --- stage 1 ----------------------------------------------------------
    say("DNS LOOKUP %s", qPrintable(host));
    QHostInfo::lookupHost(host, &app, [&app, tcpStage](const QHostInfo &info) {
        if (info.error() != QHostInfo::NoError || info.addresses().isEmpty()) {
            say("DNS FAIL %d %s", int(info.error()), qPrintable(info.errorString()));
            app.exit(1);
            return;
        }
        say("DNS OK %s", qPrintable(info.addresses().first().toString()));
        tcpStage();
    });

    return app.exec();
}
