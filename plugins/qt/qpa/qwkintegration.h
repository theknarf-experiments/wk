// QWkIntegration — the QPA entry point for a wk node.
//
// Ordering in the constructor is the one thing here that is not boilerplate:
// wkgfx_open() must run BEFORE the screen is constructed, because the host is
// resize-authoritative and the geometry Qt is told about has to be the size we
// were actually granted, not the size we asked for.
#ifndef QWKINTEGRATION_H
#define QWKINTEGRATION_H

#include <qpa/qplatformintegration.h>

QT_BEGIN_NAMESPACE

class QWkScreen;
class QPlatformFontDatabase;
class QPlatformInputContext;

class QWkIntegration : public QPlatformIntegration
{
public:
    explicit QWkIntegration(const QStringList &parameters);
    ~QWkIntegration() override;

    static QWkIntegration *instance() { return s_instance; }

    void initialize() override;
    bool hasCapability(QPlatformIntegration::Capability cap) const override;

    QPlatformWindow *createPlatformWindow(QWindow *window) const override;
    QPlatformBackingStore *createPlatformBackingStore(QWindow *window) const override;
    QAbstractEventDispatcher *createEventDispatcher() const override;

    QPlatformFontDatabase *fontDatabase() const override;
    QPlatformInputContext *inputContext() const override;
    QStringList themeNames() const override;
    QPlatformTheme *createPlatformTheme(const QString &name) const override;

    QWkScreen *screen() const { return m_screen; }

private:
    static QWkIntegration *s_instance;

    QWkScreen *m_screen = nullptr;
    mutable QPlatformFontDatabase *m_fontDatabase = nullptr;
    mutable QPlatformInputContext *m_inputContext = nullptr;
};

QT_END_NAMESPACE

#endif // QWKINTEGRATION_H
