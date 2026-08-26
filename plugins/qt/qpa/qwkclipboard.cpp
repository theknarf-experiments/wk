// See qwkclipboard.h for the model. This file is the mechanics.
#if !defined(QT_NO_CLIPBOARD)

#include "qwkclipboard.h"

#include <QtCore/qbytearray.h>
#include <QtCore/qstring.h>
#include <QtCore/qsystemdetection.h>
#include <QtGui/qclipboard.h>

#include <QtCore/qmimedata.h>

#include <stdlib.h>

extern "C" {
#include "wkclip.h"
}

QT_BEGIN_NAMESPACE

QWkClipboard::QWkClipboard() = default;

QWkClipboard::~QWkClipboard()
{
    delete m_userMimeData;
    delete m_systemMimeData;
}

bool QWkClipboard::readHost(quint64 *seq, QString *text) const
{
    char *buf = nullptr;
    quint64 s = 0;
    if (!wkclip_get(&buf, &s))
        return false;
    if (seq)
        *seq = s;
    if (text)
        *text = QString::fromUtf8(buf);
    free(buf); // wkclip_get hands over a malloc'd copy
    return true;
}

bool QWkClipboard::stillOurs() const
{
    // Nothing of ours on it: trivially not ours.
    if (!m_userMimeData)
        return false;

    quint64 seq = 0;
    QString text;
    if (!readHost(&seq, &text)) {
        // No host clipboard reachable at all (unwired, denied, empty, or the
        // machine has none). Then nothing outside this node can have taken it
        // from us, so what we set is still what a paste should get — this is
        // the in-process clipboard Qt had before the bridge existed, and it
        // keeps working exactly as it did.
        return true;
    }

    // Two ways to still own it, and both are needed.
    //
    //   * the host is showing the text we put there. The normal steady state
    //     once the host's pump has run.
    //   * the host's seq has not moved since our write. This covers the gap
    //     between wkclip_set() (which only queues into the host's outbox) and
    //     the client's next event-loop pass, and it covers a setMimeData()
    //     that carried no text at all — an image copied inside this node
    //     leaves the host clipboard untouched, and we still own it.
    //
    // Anything else means a foreign copy landed and our QMimeData is stale.
    return text == m_ownedText || seq == m_writeSeq;
}

QMimeData *QWkClipboard::mimeData(QClipboard::Mode mode)
{
    if (mode != QClipboard::Clipboard)
        return nullptr;

    if (stillOurs())
        return m_userMimeData; // verbatim: every format survives, incl. non-text

    // Somebody else copied. Qt's contract is that the QMimeData handed out by
    // a previous mimeData() is only valid until the clipboard changes, and it
    // just did, so dropping ours here is both correct and the only place that
    // can notice.
    delete m_userMimeData;
    m_userMimeData = nullptr;
    m_ownedText.clear();

    quint64 seq = 0;
    QString text;
    if (!readHost(&seq, &text)) {
        // Not wired, token-denied, or the host clipboard holds no text. These
        // are deliberately indistinguishable (see wit-clipboard/world.wit), so
        // there is nothing to warn about and nothing to paste.
        return nullptr;
    }

    if (!m_systemMimeData)
        m_systemMimeData = new QMimeData;
    else
        m_systemMimeData->clear();
    m_systemMimeData->setText(text);
    return m_systemMimeData;
}

void QWkClipboard::setMimeData(QMimeData *data, QClipboard::Mode mode)
{
    if (mode != QClipboard::Clipboard) {
        // We advertise only QClipboard::Clipboard, but QClipboard will still
        // route a Selection/FindBuffer set here on some paths. Take ownership
        // of the object regardless — QClipboard has already handed it over,
        // and leaking it would be the alternative.
        if (data != m_userMimeData && data != m_systemMimeData)
            delete data;
        return;
    }
    if (data == m_userMimeData || (data && data == m_systemMimeData))
        return; // setting back what we already hold

    delete m_userMimeData;
    m_userMimeData = data;

    // Record the host's seq BEFORE the write, so stillOurs() can tell "our
    // write hasn't been picked up yet" from "someone else copied".
    m_writeSeq = 0;
    readHost(&m_writeSeq, nullptr);

    m_ownedText.clear();
    if (data && data->hasText()) {
        m_ownedText = data->text();
        // Text only, and only to the host. Every other format the app put in
        // `data` stays in m_userMimeData and never leaves this node — that is
        // the deliberate cut, not a stub. A denied write is dropped silently
        // by the host; there is no failure to report and nothing to warn
        // about, since a node must not learn what it was refused.
        wkclip_set(m_ownedText.toUtf8().constData());
    }

    emitChanged(QClipboard::Clipboard);
}

bool QWkClipboard::supportsMode(QClipboard::Mode mode) const
{
    // One clipboard. There is no X11 PRIMARY selection and no macOS find
    // buffer behind wk:clipboard — `arboard` exposes exactly one — so
    // claiming Selection would mean silently aliasing it onto the same
    // storage, which is worse than saying no.
    return mode == QClipboard::Clipboard;
}

bool QWkClipboard::ownsMode(QClipboard::Mode mode) const
{
    return mode == QClipboard::Clipboard && stillOurs();
}

QT_END_NAMESPACE

#endif // QT_NO_CLIPBOARD
