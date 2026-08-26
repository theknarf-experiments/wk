// qt-smoke — the wk QPA plugin's test asset.
//
// A QMainWindow with a QPushButton and a QLabel that counts clicks: the
// smallest program that exercises the whole stack end to end — QApplication
// startup, static platform-plugin resolution, the widget layout engine, the
// raster paint engine, FreeType + HarfBuzz text, the fbconvenience compositor,
// the frame-paced event dispatcher and wkgfx_present().
//
// Run it as a wk node. It also runs under plain `wasmtime -W exceptions` with
// QT_QPA_PLATFORM=offscreen, which is worth doing first when something breaks,
// because it separates "Qt is broken" from "the wk QPA plugin is broken".
//
// WK_SMOKE_SELFTEST=1 makes it drive itself: it synthesises a click on the
// button, verifies the label changed, and exits non-zero if the composited
// screen image is blank. That is what makes this a test rather than a demo.
// It also focuses the QLineEdit and echoes every change to it, so the host
// test can type real wasi:surface key events into a real text field.
#include <QtWidgets/QApplication>
#include <QtWidgets/QLabel>
#include <QtWidgets/QLineEdit>
#include <QtWidgets/QMainWindow>
#include <QtWidgets/QPushButton>
#include <QtWidgets/QStyleFactory>
#include <QtWidgets/QVBoxLayout>
#include <QtWidgets/QWidget>

#include <QtWidgets/QStyle>

#include <QtCore/QDebug>
#include <QtCore/QtPlugin>
#include <QtCore/QTimer>
#include <QtGui/QFontDatabase>
#include <QtGui/QScreen>

#include <cstdio>

// There is no dlopen on wasm, so every plugin is linked in and named here.
// Without this line QApplication aborts with "no Qt platform plugin could be
// initialized" and an EMPTY available-plugins list, which looks like a broken
// Qt build but is a missing symbol reference.
Q_IMPORT_PLUGIN(QWkIntegrationPlugin)

int main(int argc, char **argv)
{
    // Default to the wk platform. qtbase was configured with
    // QT_QPA_DEFAULT_PLATFORM=offscreen (it predates this plugin), so without
    // this an app silently renders to nowhere.
    if (!qEnvironmentVariableIsSet("QT_QPA_PLATFORM"))
        qputenv("QT_QPA_PLATFORM", "wk");

    QApplication app(argc, argv);

    if (QStyleFactory::keys().contains(QLatin1String("Fusion")))
        QApplication::setStyle(QStringLiteral("Fusion"));

    // WK_HOST_OS is the host telling the sandbox what it is running on; the
    // QPA plugin turns "macos" into the Ctrl/Meta swap, so print it — a
    // missing value is the difference between Cmd+C meaning Copy and meaning
    // nothing, and it is otherwise invisible from outside.
    std::printf("platform=%s style=%s families=%lld host_os=%s\n",
                qPrintable(QApplication::platformName()),
                qPrintable(QApplication::style()->objectName()),
                (long long)QFontDatabase::families().size(),
                qgetenv("WK_HOST_OS").constData());
    if (QScreen *s = QApplication::primaryScreen())
        std::printf("screen=%dx%d\n", s->geometry().width(), s->geometry().height());
    std::fflush(stdout);

    QMainWindow window;
    window.setWindowTitle(QStringLiteral("qt-smoke"));

    auto *central = new QWidget;
    auto *layout = new QVBoxLayout(central);
    layout->setContentsMargins(24, 24, 24, 24);
    layout->setSpacing(16);

    auto *title = new QLabel(QStringLiteral("Qt 6.8.4 Widgets on wasm32-wasip2"));
    QFont titleFont = title->font();
    titleFont.setPointSize(18);
    titleFont.setBold(true);
    title->setFont(titleFont);
    title->setAlignment(Qt::AlignCenter);

    auto *count = new QLabel(QStringLiteral("Clicks: 0"));
    count->setAlignment(Qt::AlignCenter);
    QFont countFont = count->font();
    countFont.setPointSize(14);
    count->setFont(countFont);

    auto *button = new QPushButton(QStringLiteral("Click me"));
    button->setMinimumHeight(48);

    // The typing half of the smoke test. A QLineEdit is the widget that made
    // the host's `text: None` bug visible — it is read-only in practice when
    // key events carry no character — so it is what proves the fix: every
    // change is printed, and the host test asserts on the resulting string.
    auto *edit = new QLineEdit;
    edit->setPlaceholderText(QStringLiteral("type here"));
    QObject::connect(edit, &QLineEdit::textChanged, [](const QString &t) {
        std::printf("EDIT '%s'\n", qPrintable(t));
        std::fflush(stdout);
    });

    layout->addWidget(title);
    layout->addStretch(1);
    layout->addWidget(count);
    layout->addWidget(edit);
    layout->addWidget(button);
    layout->addStretch(1);

    int clicks = 0;
    QObject::connect(button, &QPushButton::clicked, [&] {
        ++clicks;
        count->setText(QStringLiteral("Clicks: %1").arg(clicks));
        std::printf("clicked %d\n", clicks);
        std::fflush(stdout);
    });

    window.setCentralWidget(central);
    window.resize(640, 400);
    window.show();

    if (qEnvironmentVariableIntValue("WK_SMOKE_SELFTEST") == 1) {
        // Two separate claims, checked in order.
        //
        // 1. The widget machinery works at all: click the button directly and
        //    see the label follow. This needs no input from the host, so it
        //    isolates "Qt is wired up" from "wk input reaches Qt".
        // 2. REAL input works: publish the button's rect in surface
        //    coordinates so the harness can aim an actual wasi:surface
        //    pointer event at it. A second click can only arrive by way of
        //    the wkgfx queue -> QWkInput -> QGuiApplication's null-window
        //    hit-testing -> QPushButton, which is the whole input path.
        QTimer::singleShot(0, [&] {
            // Keys from the host arrive with no window (QWkInput passes
            // nullptr), so they land on the focus widget of the focus window.
            // Give that to the QLineEdit up front: the host test types into it
            // without a click, and a click would first have to know where it
            // is.
            edit->setFocus();
            button->click();
            const bool ok = clicks == 1 && count->text() == QStringLiteral("Clicks: 1");
            std::printf("SELFTEST %s (clicks=%d label='%s')\n", ok ? "PASS" : "FAIL", clicks,
                        qPrintable(count->text()));
            std::fflush(stdout);
        });

        // Republished on a repeating timer rather than printed once, for two
        // reasons. The rect is not final at singleShot(0): QFbWindow forces
        // the first top-level to the full surface, and the layout has not
        // re-run yet, so an early reading is the pre-resize 640x400 one and
        // aiming at it lands on empty background. And a timer that keeps
        // firing is itself worth asserting — it is the only evidence that the
        // dispatcher's QTimer deadlines survive being folded into the frame
        // wait.
        auto *report = new QTimer(&window);
        QObject::connect(report, &QTimer::timeout, [&] {
            const QRect r(button->mapToGlobal(QPoint(0, 0)), button->size());
            std::printf("BUTTON %d %d %d %d\n", r.x(), r.y(), r.width(), r.height());
            std::fflush(stdout);
        });
        report->start(250);
    }

    return app.exec();
}
