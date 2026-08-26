#include "qwkfontdatabase.h"

#include <QtCore/qdir.h>
#include <QtCore/qfile.h>
#include <QtCore/qfileinfo.h>
#include <QtCore/qloggingcategory.h>

QT_BEGIN_NAMESPACE

static const QStringList &fontNameFilters()
{
    static const QStringList filters{ QStringLiteral("*.ttf"), QStringLiteral("*.otf"),
                                      QStringLiteral("*.ttc"), QStringLiteral("*.pfa"),
                                      QStringLiteral("*.pfb") };
    return filters;
}

// Register every font file in `path`. Returns the families registered.
static QStringList addFontsFrom(const QString &path)
{
    QStringList families;
    QDir dir(path);
    if (!dir.exists())
        return families;
    const auto entries = dir.entryInfoList(fontNameFilters(), QDir::Files);
    for (const QFileInfo &fi : entries) {
        // Resource paths (":/fonts/x.ttf") cannot be handed to FreeType as a
        // filename, so read those into memory; real files are passed by name
        // so FreeType can mmap/stream them.
        //
        // The second argument must stay the FULL resource path, and it is not
        // cosmetic. addTTFile() stores it as FontFile::fileName, and
        // QFreetypeFace::getFace() (qfontengine_ft.cpp) branches on the
        // FILENAME FIRST and ignores the QByteArray it was handed whenever the
        // name is non-empty: a non-native path (":...") it re-reads through
        // QFile, anything else it gives straight to FT_New_Face. Pass a bare
        // basename here and the font registers fine — families=1, correct
        // metrics — but every engine creation then fails with "QFontEngineFT:
        // Failed to create FreeType font engine" because FT_New_Face cannot
        // find "DejaVuSans.ttf" on disk, and Qt silently falls back to
        // QFontEngineBox, which draws a hollow rectangle for every character.
        if (fi.absoluteFilePath().startsWith(u':')) {
            QFile f(fi.absoluteFilePath());
            if (!f.open(QIODevice::ReadOnly))
                continue;
            families += QFreeTypeFontDatabase::addTTFile(
                    f.readAll(), QFile::encodeName(fi.absoluteFilePath()));
        } else {
            families += QFreeTypeFontDatabase::addTTFile(
                    QByteArray(), QFile::encodeName(fi.absoluteFilePath()));
        }
    }
    return families;
}

void QWkFontDatabase::populateFontDatabase()
{
    QStringList families;

    // 1. The platform font dir (QT_QPA_FONTDIR, else LibrariesPath/fonts).
    //    Not QFreeTypeFontDatabase::populateFontDatabase(), because that
    //    qWarning()s about a missing directory before we have had a chance to
    //    fall back — and a missing directory is the NORMAL case for a wk node.
    families += addFontsFrom(fontDir());

    // 2. Conventional locations, for node images laid out like a distro.
    if (families.isEmpty()) {
        for (const char *p : { "/fonts", "/usr/share/fonts", "/usr/share/fonts/truetype",
                               "/usr/local/share/fonts" })
            families += addFontsFrom(QString::fromLatin1(p));
    }

    // 3. Fonts compiled into the component. Always available, even to a node
    //    with no filesystem at all.
    if (families.isEmpty())
        families += addFontsFrom(QStringLiteral(":/fonts"));

    if (families.isEmpty()) {
        qWarning("QWkFontDatabase: no fonts found -- ALL TEXT WILL BE INVISIBLE.\n"
                 "  Qt 6 ships no fonts. Give this node one of:\n"
                 "    * QT_QPA_FONTDIR pointing at a directory of .ttf/.otf in its VFS,\n"
                 "    * a font at /fonts or /usr/share/fonts, or\n"
                 "    * a Qt resource under :/fonts compiled into the component\n"
                 "      (qt_add_resources(... PREFIX \"/fonts\" FILES DejaVuSans.ttf)).");
        return;
    }

    m_defaultFamily = families.constFirst();
}

QFont QWkFontDatabase::defaultFont() const
{
    if (!m_defaultFamily.isEmpty())
        return QFont(m_defaultFamily);
    return QFreeTypeFontDatabase::defaultFont();
}

QStringList QWkFontDatabase::fallbacksForFamily(const QString &family, QFont::Style style,
                                                QFont::StyleHint styleHint,
                                                QChar::Script script) const
{
    QStringList result = QFreeTypeFontDatabase::fallbacksForFamily(family, style, styleHint, script);
    // Whatever we found first is the only font guaranteed to exist, so make it
    // the last resort for every request (qwasmfontdatabase.cpp:320-336 does
    // the same with DejaVu Sans).
    if (!m_defaultFamily.isEmpty() && family != m_defaultFamily
        && !result.contains(m_defaultFamily)) {
        result.append(m_defaultFamily);
    }
    return result;
}

QT_END_NAMESPACE
