// QWkFontDatabase — where a wk node's fonts come from.
//
// Qt 6 ships no fonts. Without one the app still runs, QFontDatabase is empty
// and every string renders as nothing at all, which reads like a paint bug
// rather than a deployment mistake — so this class looks in three places, in
// order, and says so loudly if all three are empty:
//
//   1. QT_QPA_FONTDIR, or QLibraryInfo::LibrariesPath/fonts. This is
//      QPlatformFontDatabase::fontDir() and QFreeTypeFontDatabase scans it for
//      us; a node with a real font directory in its VFS (a wk BindMount, a
//      container layer) needs no code here at all, only the env var.
//   2. A few conventional absolute paths, for nodes whose image puts fonts
//      where a Linux distribution would.
//   3. Qt resources under :/fonts — fonts COMPILED INTO the component. This
//      is the one that always works, because a wk node may have no writable
//      or mounted filesystem whatsoever, and it is the same fallback Qt's own
//      wasm plugin uses (qwasmfontdatabase.cpp:265-278).
//
// None of QWasmFontDatabase's startup-task / refFontFileLoading machinery is
// copied: that exists because a browser fetches fonts asynchronously. Reads
// here are synchronous.
#ifndef QWKFONTDATABASE_H
#define QWKFONTDATABASE_H

#include <QtGui/private/qfreetypefontdatabase_p.h>

QT_BEGIN_NAMESPACE

class QWkFontDatabase : public QFreeTypeFontDatabase
{
public:
    void populateFontDatabase() override;
    QFont defaultFont() const override;
    QStringList fallbacksForFamily(const QString &family, QFont::Style style,
                                   QFont::StyleHint styleHint, QChar::Script script) const override;

private:
    QString m_defaultFamily;
};

QT_END_NAMESPACE

#endif // QWKFONTDATABASE_H
