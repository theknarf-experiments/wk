#include "qwktheme.h"
#include "qwkkeytranslator.h"

#include <QtCore/qstringlist.h>
#include <QtCore/qvariant.h>

QT_BEGIN_NAMESPACE

QVariant QWkTheme::themeHint(ThemeHint hint) const
{
    switch (hint) {
    case StyleNames:
        return QStringList{ QStringLiteral("Fusion") };
    case UseFullScreenForPopupMenu:
        return false;
    case MouseDoubleClickInterval:
        return 400;
    case CursorFlashTime:
        return 1000;
    case KeyboardScheme:
        // Must agree with QWkKeyTranslator's modifier policy: with the Mac
        // swap on, Cmd arrives as ControlModifier and Qt should also use Mac
        // shortcut conventions, or Ctrl+C matches but Cmd+Left does not. Ask
        // it rather than re-reading the environment, so the two cannot drift.
        return QWkKeyTranslator::macModifiers() ? int(MacKeyboardScheme) : int(X11KeyboardScheme);
    case ShowShortcutsInContextMenus:
        return true;
    default:
        return QPlatformTheme::themeHint(hint);
    }
}

bool QWkTheme::usePlatformNativeDialog(DialogType type) const
{
    Q_UNUSED(type);
    return false; // there is no host dialog to defer to
}

QT_END_NAMESPACE
