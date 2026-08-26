// QWkClipboard — Qt's clipboard bridged to wk's HOST system clipboard.
//
// Before this existed, Cmd/Ctrl+V reached Qt as a QKeySequence::Paste, Qt
// asked its platform integration for a clipboard, got the DEFAULT
// QPlatformClipboard (a process-global QMimeData holder), and pasted whatever
// this same app had copied — nothing, on a fresh start. Copy and paste worked
// perfectly and privately inside one node and were invisible to the machine
// the node was running on.
//
// The bridge is wk:clipboard (../../clipboard-compat/wkclip.h), which is a
// GRANTED capability: a node sees the host clipboard only while it is wired to
// a Clipboard node on wk's canvas, and its capability token can allow read
// without write or write without read. Everything here therefore has to treat
// "no" as a normal steady state, not an error — wkclip_get() returning nothing
// forever is what an unwired node looks like, and wkclip_set() quietly doing
// nothing is what a write-denied one looks like. Neither warns: a sandboxed
// app must not be able to probe for a capability it was not given.
//
// THE MODEL IS HAIKU'S, not wasm's. `m_userMimeData` is the QMimeData this app
// itself put on the clipboard, kept verbatim and handed straight back for as
// long as the app still owns the clipboard. That is what keeps a text-only
// host bridge from being lossy in practice: copying a pixel selection or rich
// text inside one node and pasting it back into the same node round-trips
// every format, because the QMimeData never went through the host at all. Only
// the cross-application hop is text, which is the honest limit of what wk's
// host side (`arboard`) can carry.
//
// OWNERSHIP is tracked with wk:clipboard's `seq`, not with a Qt signal, since
// there is no host-side change notification to connect to and therefore
// nothing to hang one on. `seq` increments only when the host observes the
// clipboard's text actually change, so remembering the text we wrote and the
// seq at that moment answers ownsMode() without any callback.
//
// The consequence, and it is a real one: QClipboard::dataChanged() does NOT
// fire when somebody copies in ANOTHER application. Qt learns about a foreign
// copy the next time it asks — which is what a paste does, so paste is always
// current — but a widget that greys out its Paste button on dataChanged()
// will not refresh on its own. Making that work means polling the host from
// the event dispatcher; it is a deliberate omission, not an oversight.
#ifndef QWKCLIPBOARD_H
#define QWKCLIPBOARD_H

#include <QtGui/qtguiglobal.h>

#if !defined(QT_NO_CLIPBOARD)

#include <qpa/qplatformclipboard.h>

QT_BEGIN_NAMESPACE

class QMimeData;

class QWkClipboard : public QPlatformClipboard
{
public:
    QWkClipboard();
    ~QWkClipboard() override;

    QMimeData *mimeData(QClipboard::Mode mode = QClipboard::Clipboard) override;
    void setMimeData(QMimeData *data, QClipboard::Mode mode = QClipboard::Clipboard) override;
    bool supportsMode(QClipboard::Mode mode) const override;
    bool ownsMode(QClipboard::Mode mode) const override;

private:
    // Read the host clipboard once. Returns false when there is nothing to
    // read — unwired, token-denied, no host clipboard, or it holds non-text —
    // all of which are ordinary steady states, not errors.
    bool readHost(quint64 *seq, QString *text) const;
    // Whether this app still owns the clipboard. A pure predicate over the
    // host's current state; see the .cpp for its two-part test.
    bool stillOurs() const;

    // What THIS app put on the clipboard, owned by us and returned verbatim
    // while we still own the clipboard. Non-text formats in it never reach the
    // host; they are in-node only, on purpose (see the header comment).
    QMimeData *m_userMimeData = nullptr;
    // A QMimeData built from the host's text, rebuilt on demand.
    QMimeData *m_systemMimeData = nullptr;
    // The text our last setMimeData() sent to the host (empty if it sent
    // none), and the host seq at that moment. Together they answer "is the
    // host clipboard still showing what we put there?" even before the host's
    // pump has caught up with our write.
    QString m_ownedText;
    quint64 m_writeSeq = 0;
};

QT_END_NAMESPACE

#endif // QT_NO_CLIPBOARD

#endif // QWKCLIPBOARD_H
