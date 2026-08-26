// The "wk" QPA plugin's factory.
//
// There is no dlopen on wasm, so this is a STATIC plugin: Q_PLUGIN_METADATA
// under QT_STATICPLUGIN emits qt_static_plugin_QWkIntegrationPlugin(), and the
// application must name it once with
//
//     Q_IMPORT_PLUGIN(QWkIntegrationPlugin)
//
// and then select it with QT_QPA_PLATFORM=wk (or -platform wk). Forgetting the
// import produces "no Qt platform plugin could be initialized" with an empty
// available-plugins list, which reads like a broken build but is a link-time
// omission.
//
// An optional size parameter is accepted: -platform wk:1280x800.
#include <qpa/qplatformintegrationplugin.h>

#include "qwkintegration.h"

QT_BEGIN_NAMESPACE

using namespace Qt::StringLiterals;

class QWkIntegrationPlugin : public QPlatformIntegrationPlugin
{
    Q_OBJECT
    Q_PLUGIN_METADATA(IID QPlatformIntegrationFactoryInterface_iid FILE "wk.json")
public:
    QPlatformIntegration *create(const QString &system, const QStringList &paramList) override;
};

QPlatformIntegration *QWkIntegrationPlugin::create(const QString &system,
                                                   const QStringList &paramList)
{
    if (!system.compare("wk"_L1, Qt::CaseInsensitive))
        return new QWkIntegration(paramList);
    return nullptr;
}

QT_END_NAMESPACE

#include "main.moc"
