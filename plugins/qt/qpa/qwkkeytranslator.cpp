#include "qwkkeytranslator.h"

#include <QtCore/qbytearray.h>
#include <QtCore/qchar.h>

QT_BEGIN_NAMESPACE

namespace QWkKeyTranslator {

Qt::Key fromWkgfxCode(int32_t code)
{
    switch (code) {
    // --- writing-system keys (W3C "Writing System Keys") ---
    case WKGFX_K_BACKQUOTE:      return Qt::Key_QuoteLeft;
    case WKGFX_K_BACKSLASH:      return Qt::Key_Backslash;
    case WKGFX_K_BRACKET_LEFT:   return Qt::Key_BracketLeft;
    case WKGFX_K_BRACKET_RIGHT:  return Qt::Key_BracketRight;
    case WKGFX_K_COMMA:          return Qt::Key_Comma;
    case WKGFX_K_DIGIT0:         return Qt::Key_0;
    case WKGFX_K_DIGIT1:         return Qt::Key_1;
    case WKGFX_K_DIGIT2:         return Qt::Key_2;
    case WKGFX_K_DIGIT3:         return Qt::Key_3;
    case WKGFX_K_DIGIT4:         return Qt::Key_4;
    case WKGFX_K_DIGIT5:         return Qt::Key_5;
    case WKGFX_K_DIGIT6:         return Qt::Key_6;
    case WKGFX_K_DIGIT7:         return Qt::Key_7;
    case WKGFX_K_DIGIT8:         return Qt::Key_8;
    case WKGFX_K_DIGIT9:         return Qt::Key_9;
    case WKGFX_K_EQUAL:          return Qt::Key_Equal;
    case WKGFX_K_INTL_BACKSLASH: return Qt::Key_Backslash;
    case WKGFX_K_INTL_RO:        return Qt::Key_Backslash;
    case WKGFX_K_INTL_YEN:       return Qt::Key_yen;
    case WKGFX_K_KEY_A:          return Qt::Key_A;
    case WKGFX_K_KEY_B:          return Qt::Key_B;
    case WKGFX_K_KEY_C:          return Qt::Key_C;
    case WKGFX_K_KEY_D:          return Qt::Key_D;
    case WKGFX_K_KEY_E:          return Qt::Key_E;
    case WKGFX_K_KEY_F:          return Qt::Key_F;
    case WKGFX_K_KEY_G:          return Qt::Key_G;
    case WKGFX_K_KEY_H:          return Qt::Key_H;
    case WKGFX_K_KEY_I:          return Qt::Key_I;
    case WKGFX_K_KEY_J:          return Qt::Key_J;
    case WKGFX_K_KEY_K:          return Qt::Key_K;
    case WKGFX_K_KEY_L:          return Qt::Key_L;
    case WKGFX_K_KEY_M:          return Qt::Key_M;
    case WKGFX_K_KEY_N:          return Qt::Key_N;
    case WKGFX_K_KEY_O:          return Qt::Key_O;
    case WKGFX_K_KEY_P:          return Qt::Key_P;
    case WKGFX_K_KEY_Q:          return Qt::Key_Q;
    case WKGFX_K_KEY_R:          return Qt::Key_R;
    case WKGFX_K_KEY_S:          return Qt::Key_S;
    case WKGFX_K_KEY_T:          return Qt::Key_T;
    case WKGFX_K_KEY_U:          return Qt::Key_U;
    case WKGFX_K_KEY_V:          return Qt::Key_V;
    case WKGFX_K_KEY_W:          return Qt::Key_W;
    case WKGFX_K_KEY_X:          return Qt::Key_X;
    case WKGFX_K_KEY_Y:          return Qt::Key_Y;
    case WKGFX_K_KEY_Z:          return Qt::Key_Z;
    case WKGFX_K_MINUS:          return Qt::Key_Minus;
    case WKGFX_K_PERIOD:         return Qt::Key_Period;
    case WKGFX_K_QUOTE:          return Qt::Key_Apostrophe;
    case WKGFX_K_SEMICOLON:      return Qt::Key_Semicolon;
    case WKGFX_K_SLASH:          return Qt::Key_Slash;

    // --- functional keys ---
    case WKGFX_K_ALT_LEFT:
    case WKGFX_K_ALT_RIGHT:      return Qt::Key_Alt;
    case WKGFX_K_BACKSPACE:      return Qt::Key_Backspace;
    case WKGFX_K_CAPS_LOCK:      return Qt::Key_CapsLock;
    case WKGFX_K_CONTEXT_MENU:   return Qt::Key_Menu;
    case WKGFX_K_CONTROL_LEFT:
    case WKGFX_K_CONTROL_RIGHT:  return Qt::Key_Control;
    case WKGFX_K_ENTER:          return Qt::Key_Return;
    case WKGFX_K_META_LEFT:
    case WKGFX_K_META_RIGHT:     return Qt::Key_Meta;
    case WKGFX_K_SHIFT_LEFT:
    case WKGFX_K_SHIFT_RIGHT:    return Qt::Key_Shift;
    case WKGFX_K_SPACE:          return Qt::Key_Space;
    case WKGFX_K_TAB:            return Qt::Key_Tab;

    // --- IME / CJK ---
    case WKGFX_K_CONVERT:        return Qt::Key_Henkan;
    case WKGFX_K_KANA_MODE:      return Qt::Key_Kana_Shift;
    case WKGFX_K_LANG1:          return Qt::Key_Hangul;
    case WKGFX_K_LANG2:          return Qt::Key_Hangul_Hanja;
    case WKGFX_K_LANG3:          return Qt::Key_Katakana;
    case WKGFX_K_LANG4:          return Qt::Key_Hiragana;
    case WKGFX_K_LANG5:          return Qt::Key_Zenkaku_Hankaku;
    case WKGFX_K_NON_CONVERT:    return Qt::Key_Muhenkan;
    case WKGFX_K_HIRAGANA:       return Qt::Key_Hiragana;
    case WKGFX_K_KATAKANA:       return Qt::Key_Katakana;

    // --- control pad ---
    case WKGFX_K_DELETE:         return Qt::Key_Delete;
    case WKGFX_K_END:            return Qt::Key_End;
    case WKGFX_K_HELP:           return Qt::Key_Help;
    case WKGFX_K_HOME:           return Qt::Key_Home;
    case WKGFX_K_INSERT:         return Qt::Key_Insert;
    case WKGFX_K_PAGE_DOWN:      return Qt::Key_PageDown;
    case WKGFX_K_PAGE_UP:        return Qt::Key_PageUp;

    // --- arrow pad ---
    case WKGFX_K_ARROW_DOWN:     return Qt::Key_Down;
    case WKGFX_K_ARROW_LEFT:     return Qt::Key_Left;
    case WKGFX_K_ARROW_RIGHT:    return Qt::Key_Right;
    case WKGFX_K_ARROW_UP:       return Qt::Key_Up;

    // --- numpad. Qt reports these as the plain key plus KeypadModifier,
    //     which translate() adds; see the WKGFX_K_NUMPAD* range test there.
    case WKGFX_K_NUM_LOCK:       return Qt::Key_NumLock;
    case WKGFX_K_NUMPAD0:        return Qt::Key_0;
    case WKGFX_K_NUMPAD1:        return Qt::Key_1;
    case WKGFX_K_NUMPAD2:        return Qt::Key_2;
    case WKGFX_K_NUMPAD3:        return Qt::Key_3;
    case WKGFX_K_NUMPAD4:        return Qt::Key_4;
    case WKGFX_K_NUMPAD5:        return Qt::Key_5;
    case WKGFX_K_NUMPAD6:        return Qt::Key_6;
    case WKGFX_K_NUMPAD7:        return Qt::Key_7;
    case WKGFX_K_NUMPAD8:        return Qt::Key_8;
    case WKGFX_K_NUMPAD9:        return Qt::Key_9;
    case WKGFX_K_NUMPAD_ADD:     return Qt::Key_Plus;
    case WKGFX_K_NUMPAD_BACKSPACE: return Qt::Key_Backspace;
    case WKGFX_K_NUMPAD_CLEAR:
    case WKGFX_K_NUMPAD_CLEAR_ENTRY: return Qt::Key_Clear;
    case WKGFX_K_NUMPAD_COMMA:   return Qt::Key_Comma;
    case WKGFX_K_NUMPAD_DECIMAL: return Qt::Key_Period;
    case WKGFX_K_NUMPAD_DIVIDE:  return Qt::Key_Slash;
    case WKGFX_K_NUMPAD_ENTER:   return Qt::Key_Enter;
    case WKGFX_K_NUMPAD_EQUAL:   return Qt::Key_Equal;
    case WKGFX_K_NUMPAD_HASH:    return Qt::Key_NumberSign;
    case WKGFX_K_NUMPAD_MULTIPLY:
    case WKGFX_K_NUMPAD_STAR:    return Qt::Key_Asterisk;
    case WKGFX_K_NUMPAD_PAREN_LEFT:  return Qt::Key_ParenLeft;
    case WKGFX_K_NUMPAD_PAREN_RIGHT: return Qt::Key_ParenRight;
    case WKGFX_K_NUMPAD_SUBTRACT: return Qt::Key_Minus;

    // --- function section ---
    case WKGFX_K_ESCAPE:         return Qt::Key_Escape;
    case WKGFX_K_F1:             return Qt::Key_F1;
    case WKGFX_K_F2:             return Qt::Key_F2;
    case WKGFX_K_F3:             return Qt::Key_F3;
    case WKGFX_K_F4:             return Qt::Key_F4;
    case WKGFX_K_F5:             return Qt::Key_F5;
    case WKGFX_K_F6:             return Qt::Key_F6;
    case WKGFX_K_F7:             return Qt::Key_F7;
    case WKGFX_K_F8:             return Qt::Key_F8;
    case WKGFX_K_F9:             return Qt::Key_F9;
    case WKGFX_K_F10:            return Qt::Key_F10;
    case WKGFX_K_F11:            return Qt::Key_F11;
    case WKGFX_K_F12:            return Qt::Key_F12;
    case WKGFX_K_PRINT_SCREEN:   return Qt::Key_Print;
    case WKGFX_K_SCROLL_LOCK:    return Qt::Key_ScrollLock;
    case WKGFX_K_PAUSE:          return Qt::Key_Pause;

    // --- media / browser ---
    case WKGFX_K_BROWSER_BACK:      return Qt::Key_Back;
    case WKGFX_K_BROWSER_FAVORITES: return Qt::Key_Favorites;
    case WKGFX_K_BROWSER_FORWARD:   return Qt::Key_Forward;
    case WKGFX_K_BROWSER_HOME:      return Qt::Key_HomePage;
    case WKGFX_K_BROWSER_REFRESH:   return Qt::Key_Refresh;
    case WKGFX_K_BROWSER_SEARCH:    return Qt::Key_Search;
    case WKGFX_K_BROWSER_STOP:      return Qt::Key_Stop;
    case WKGFX_K_EJECT:             return Qt::Key_Eject;
    case WKGFX_K_LAUNCH_APP1:       return Qt::Key_Launch0;
    case WKGFX_K_LAUNCH_APP2:       return Qt::Key_Launch1;
    case WKGFX_K_LAUNCH_MAIL:       return Qt::Key_LaunchMail;
    case WKGFX_K_MEDIA_PLAY_PAUSE:  return Qt::Key_MediaTogglePlayPause;
    case WKGFX_K_MEDIA_SELECT:      return Qt::Key_LaunchMedia;
    case WKGFX_K_MEDIA_STOP:        return Qt::Key_MediaStop;
    case WKGFX_K_MEDIA_TRACK_NEXT:  return Qt::Key_MediaNext;
    case WKGFX_K_MEDIA_TRACK_PREVIOUS: return Qt::Key_MediaPrevious;
    case WKGFX_K_POWER:             return Qt::Key_PowerOff;
    case WKGFX_K_SLEEP:             return Qt::Key_Sleep;
    case WKGFX_K_AUDIO_VOLUME_DOWN: return Qt::Key_VolumeDown;
    case WKGFX_K_AUDIO_VOLUME_MUTE: return Qt::Key_VolumeMute;
    case WKGFX_K_AUDIO_VOLUME_UP:   return Qt::Key_VolumeUp;
    case WKGFX_K_WAKE_UP:           return Qt::Key_WakeUp;

    // --- legacy / editing ---
    case WKGFX_K_HYPER:          return Qt::Key_Hyper_L;
    case WKGFX_K_SUPER:          return Qt::Key_Super_L;
    case WKGFX_K_ABORT:          return Qt::Key_Cancel;
    case WKGFX_K_AGAIN:          return Qt::Key_Redo;
    case WKGFX_K_COPY:           return Qt::Key_Copy;
    case WKGFX_K_CUT:            return Qt::Key_Cut;
    case WKGFX_K_FIND:           return Qt::Key_Find;
    case WKGFX_K_OPEN:           return Qt::Key_Open;
    case WKGFX_K_PASTE:          return Qt::Key_Paste;
    case WKGFX_K_SELECT:         return Qt::Key_Select;
    case WKGFX_K_UNDO:           return Qt::Key_Undo;

    // Fn, FnLock, Props, Resume, Suspend, Turbo and the numpad memory keys
    // have no Qt equivalent at all.
    default:                     return Qt::Key_unknown;
    }
}

// wk's host fills meta_key from winit's super_key, which on a macOS host is
// Command. Qt's own Mac convention swaps Control and Meta so that
// QKeySequence::Copy (Ctrl+C, i.e. Cmd+C on a Mac) matches — the wasm plugin
// does exactly this for Platform::MacOS (qwasmevent.h:82-86), where it reads
// the platform off navigator. We are in a sandbox with no such window, so the
// host tells us instead: wk sets WK_HOST_OS on every node from
// std::env::consts::OS (crates/wk-server/src/plugin.rs). QT_WK_MAC_MODIFIERS
// stays as an explicit override for running a node against a host that does
// not set it, or for forcing the other convention by hand.
bool macModifiers()
{
    static const bool mac = qEnvironmentVariableIsSet("QT_WK_MAC_MODIFIERS")
            ? qEnvironmentVariableIntValue("QT_WK_MAC_MODIFIERS") == 1
            : qgetenv("WK_HOST_OS") == "macos";
    return mac;
}

Qt::KeyboardModifiers modifiers(const wkgfx_event &ev)
{
    Qt::KeyboardModifiers m;
    if (ev.shift)
        m |= Qt::ShiftModifier;
    if (ev.alt)
        m |= Qt::AltModifier;

    const bool swap = macModifiers();
    const bool ctrl = ev.ctrl, meta = ev.meta;
    if (swap) {
        if (meta)
            m |= Qt::ControlModifier;
        if (ctrl)
            m |= Qt::MetaModifier;
    } else {
        if (ctrl)
            m |= Qt::ControlModifier;
        if (meta)
            m |= Qt::MetaModifier;
    }

    if (ev.key >= WKGFX_K_NUMPAD0 && ev.key <= WKGFX_K_NUMPAD_SUBTRACT)
        m |= Qt::KeypadModifier;
    return m;
}

void translate(const wkgfx_event &ev, Qt::Key *key, QString *text, Qt::KeyboardModifiers *mods)
{
    *mods = modifiers(ev);
    *text = QString();

    // The layout-produced character wins, exactly as qwasmevent.cpp:70-76
    // prefers the DOM `key` value over `code`: it is the only thing that knows
    // the user's layout, so an AZERTY or Dvorak user gets the character their
    // keyboard actually produced rather than the QWERTY position. This is the
    // primary path — the host fills the event's `text` from winit's resolved
    // character — and the physical table below is the fallback for the keys
    // that produce no text at all (arrows, modifiers, F-keys).
    if (ev.ch != 0 && QChar::isPrint(ev.ch)) {
        const char32_t c = ev.ch;
        *text = QString::fromUcs4(&c, 1);
        // Qt::Key is a BMP-shaped value for character keys, so an astral
        // scalar (an emoji off an IME or a compose key) has no sensible key
        // code: leave it unknown and let the text carry the character.
        // QInputControl accepts a surrogate pair as input on its own
        // (qinputcontrol.cpp:45), so nothing is lost by having no key.
        *key = c <= 0xffff ? Qt::Key(QChar::toUpper(ev.ch)) : Qt::Key_unknown;
    } else {
        *key = fromWkgfxCode(ev.key);
    }

    // Fixups, from qwasmevent.cpp:117-128.
    if ((*mods & Qt::AltModifier) && (*mods & Qt::KeypadModifier))
        *text = QString();
    // qwasmevent drops any text longer than one QChar because the DOM hands it
    // whole key NAMES ("ArrowLeft"). Ours is one scalar or nothing, so the only
    // string that can be longer is a surrogate pair, which is legitimate input
    // — keep it.
    //
    // Editing widgets refuse to insert a character typed with the command
    // chord, but QInputControl's QTBUG-35734 guard tests for an EXACT
    // Qt::ControlModifier (or Shift+Control) match, which assumes a platform
    // with only one such chord key. wk delivers ctrl AND meta, and whichever of
    // the two did not become ControlModifier above slips straight past that
    // guard: on a Mac host without the swap, Cmd+A arrives as MetaModifier,
    // no shortcut claims it, and the QLineEdit types "a". Drop the text for
    // that chord too, in the same exact-match shape — AltGr is Alt+Ctrl and
    // must keep typing.
    if (*mods == Qt::MetaModifier || *mods == (Qt::ShiftModifier | Qt::MetaModifier))
        *text = QString();

    // The wasm plugin gets these from the DOM for free; a code-only path has
    // to supply them, and QLineEdit/QTextEdit rely on them.
    switch (*key) {
    case Qt::Key_Tab:
        *text = QStringLiteral("\t");
        if (*mods & Qt::ShiftModifier)
            *key = Qt::Key_Backtab; // qwasmevent.cpp:65-67
        break;
    case Qt::Key_Return:
    case Qt::Key_Enter:
        *text = QStringLiteral("\r");
        break;
    case Qt::Key_Backspace:
        *text = QStringLiteral("\b");
        break;
    case Qt::Key_Escape:
        *text = QStringLiteral("\x1b");
        break;
    default:
        break;
    }
}

} // namespace QWkKeyTranslator

QT_END_NAMESPACE
