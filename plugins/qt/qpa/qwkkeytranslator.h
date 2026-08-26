// wkgfx key events -> Qt::Key + text + modifiers.
//
// wasi:surface's `key` enum is the W3C UIEvents *code* set (the physical key),
// which is the same vocabulary Qt's wasm plugin translates from, so the shape
// of this file is qwasmevent.cpp's — but the table is written directly against
// WKGFX_K_* rather than reusing Qt's, because Qt's lives in an anonymous
// namespace keyed by DOM code *strings* and would have to be copied wholesale
// and then string-matched 171 times.
//
// The important borrowed behaviour is the ORDER of resolution: the layout-
// produced character (wkgfx_event.ch, the moral equivalent of the DOM `key`
// value) wins over the physical code, so an AZERTY or Dvorak user gets the key
// their keyboard actually produced instead of the QWERTY position.
#ifndef QWKKEYTRANSLATOR_H
#define QWKKEYTRANSLATOR_H

#include <QtCore/qstring.h>
#include <QtCore/qnamespace.h>

extern "C" {
#include "wkgfx.h"
}

QT_BEGIN_NAMESPACE

namespace QWkKeyTranslator {

// The physical-key fallback: WKGFX_K_* -> Qt::Key, Qt::Key_unknown if the key
// has no Qt equivalent.
Qt::Key fromWkgfxCode(int32_t code);

// Full translation of one wkgfx key event.
void translate(const wkgfx_event &ev, Qt::Key *key, QString *text,
               Qt::KeyboardModifiers *mods);

// Modifier flags alone (also used for pointer and wheel events, which carry
// no key of their own but must report the modifier state).
Qt::KeyboardModifiers modifiers(const wkgfx_event &ev);

// Whether this node follows the Mac convention of swapping Control and Meta,
// so that Cmd+C is QKeySequence::Copy. Decided once from the environment; see
// the comment on the definition. QWkTheme's KeyboardScheme must agree with it.
bool macModifiers();

} // namespace QWkKeyTranslator

QT_END_NAMESPACE

#endif // QWKKEYTRANSLATOR_H
