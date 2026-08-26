// QWkTheme — the handful of hints Qt asks the platform for.
//
// Modelled on src/plugins/platforms/wasm/qwasmtheme.{h,cpp} minus its CSS
// colour probing, since there is no host style to probe. The one hint that
// really matters is StyleNames: without a native platform style Qt would fall
// back to Windows-95 chrome, and Fusion is the only complete style guaranteed
// to be compiled into every Qt Widgets build.
#ifndef QWKTHEME_H
#define QWKTHEME_H

#include <qpa/qplatformtheme.h>

QT_BEGIN_NAMESPACE

class QWkTheme : public QPlatformTheme
{
public:
    QVariant themeHint(ThemeHint hint) const override;
    bool usePlatformNativeDialog(DialogType type) const override;
};

QT_END_NAMESPACE

#endif // QWKTHEME_H
