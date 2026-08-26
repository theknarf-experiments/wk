//! Keyboard mapping: winit key events → terminal stdin bytes and wasi-gfx key
//! codes. Split out of the compositor so the input translation lives on its own.
//!
//! Two things travel to a guest for one keystroke and they answer different
//! questions: the `key` code says *which* physical key (see `map_key`), the
//! `text` says *what it types* (see `KeyText` and `key_event`). Toolkits
//! want both — Qt's QPA prefers the character and only falls back to the code —
//! so dropping either one leaves text fields unusable even when keys "work".

use super::*;
use winit::keyboard::SmolStr;

/// Encode a key press as the bytes a terminal app expects on stdin. `text` is
/// winit's resolved character(s) for the key (handles shift/layout).
pub(super) fn encode_term_key(
    code: KeyCode,
    text: Option<&str>,
    mods: ModifiersState,
) -> Option<Vec<u8>> {
    use KeyCode as C;
    // Ctrl+letter -> control byte (Ctrl-A = 0x01 ... Ctrl-Z = 0x1a).
    if mods.control_key() {
        if let Some(n) = letter_index(code) {
            return Some(vec![n + 1]);
        }
    }
    Some(match code {
        C::Enter | C::NumpadEnter => vec![b'\r'],
        C::Backspace => vec![0x7f],
        C::Tab => vec![b'\t'],
        C::Escape => vec![0x1b],
        C::ArrowUp => vec![0x1b, b'[', b'A'],
        C::ArrowDown => vec![0x1b, b'[', b'B'],
        C::ArrowRight => vec![0x1b, b'[', b'C'],
        C::ArrowLeft => vec![0x1b, b'[', b'D'],
        C::Home => vec![0x1b, b'[', b'H'],
        C::End => vec![0x1b, b'[', b'F'],
        _ => match text {
            Some(t) if !t.is_empty() => t.as_bytes().to_vec(),
            _ => return None,
        },
    })
}

fn letter_index(code: KeyCode) -> Option<u8> {
    use KeyCode as C;
    let n = match code {
        C::KeyA => 0,
        C::KeyB => 1,
        C::KeyC => 2,
        C::KeyD => 3,
        C::KeyE => 4,
        C::KeyF => 5,
        C::KeyG => 6,
        C::KeyH => 7,
        C::KeyI => 8,
        C::KeyJ => 9,
        C::KeyK => 10,
        C::KeyL => 11,
        C::KeyM => 12,
        C::KeyN => 13,
        C::KeyO => 14,
        C::KeyP => 15,
        C::KeyQ => 16,
        C::KeyR => 17,
        C::KeyS => 18,
        C::KeyT => 19,
        C::KeyU => 20,
        C::KeyV => 21,
        C::KeyW => 22,
        C::KeyX => 23,
        C::KeyY => 24,
        C::KeyZ => 25,
        _ => return None,
    };
    Some(n)
}

/// Map a winit physical key to the wasi-gfx W3C `key` code.
///
/// The two enums are the *same* vocabulary: winit's `KeyCode` is literally the
/// W3C UIEvents `code` set, and wasi:surface's `key` is that set again in
/// kebab-case (`plugins/gfx-compat/wit/deps/wasi-surface/surface.wit`, whose
/// `enum key` block is byte-identical to the copy the host binds against under
/// `crates/wk-server/wit/`, so guest and host agree on names *and* discriminant
/// order). wit-bindgen's kebab → UpperCamel round-trip lands exactly on winit's
/// PascalCase, so 168 of the 171 arms below are a spelling identity — this table
/// is mechanical, not a judgement call. It is still written out in full: a guest
/// handed `key: none` cannot tell "wk does not know this key" from "the key was
/// not pressed", and for two thirds of the keyboard that silence was the answer.
///
/// Arms are in winit declaration order — which is W3C's own grouping (writing
/// system, functional, control pad, arrow pad, numpad, function, media, legacy)
/// — so the table can be diffed against `KeyCode` when winit is bumped.
///
/// Three arms are *not* a spelling identity, and one group has no counterpart:
///
/// * `SuperLeft`/`SuperRight` → `MetaLeft`/`MetaRight`: different words, same
///   physical key. winit renamed W3C's `MetaLeft`/`MetaRight` (the Command /
///   Windows key) to `Super*`; wasi kept the W3C spelling. winit's own web
///   backend parses the code string "MetaLeft" back into `KeyCode::SuperLeft`.
/// * `Meta` → `Super`: the one pairing made on meaning rather than on name. Both
///   enums carry W3C's legacy trio — winit spells it `Meta, Hyper, Turbo` (its
///   doc comment: "Also called `Super` in certain places"), wasi spells it
///   `hyper, super, turbo`. `Hyper` and `Turbo` pair by name, leaving exactly one
///   unmatched variant on each side. Inert in practice (no winit backend emits
///   `KeyCode::Meta`), but do not "fix" it to `MetaLeft`/`MetaRight`: that is the
///   Command key, a different and very much live one.
/// * `F13`–`F35` have no counterpart at all — wasi:surface stops at `f12`. They
///   are listed explicitly rather than swept into the wildcard because macOS
///   emits them from real scancodes: this is a gap in the wasi:surface enum that
///   guests will feel, not an oversight here.
///
/// Many of the arms below are dead on any one host — winit only emits a subset
/// per platform, and macOS's scancode table never reaches `NumpadMemoryAdd` or
/// `Props` — but they cost nothing and make the map right on Linux and Windows.
fn map_key(code: KeyCode) -> Option<Key> {
    use KeyCode as C;
    Some(match code {
        // Writing system
        C::Backquote => Key::Backquote,
        C::Backslash => Key::Backslash,
        C::BracketLeft => Key::BracketLeft,
        C::BracketRight => Key::BracketRight,
        C::Comma => Key::Comma,
        C::Digit0 => Key::Digit0,
        C::Digit1 => Key::Digit1,
        C::Digit2 => Key::Digit2,
        C::Digit3 => Key::Digit3,
        C::Digit4 => Key::Digit4,
        C::Digit5 => Key::Digit5,
        C::Digit6 => Key::Digit6,
        C::Digit7 => Key::Digit7,
        C::Digit8 => Key::Digit8,
        C::Digit9 => Key::Digit9,
        C::Equal => Key::Equal,
        C::IntlBackslash => Key::IntlBackslash,
        C::IntlRo => Key::IntlRo,
        C::IntlYen => Key::IntlYen,
        C::KeyA => Key::KeyA,
        C::KeyB => Key::KeyB,
        C::KeyC => Key::KeyC,
        C::KeyD => Key::KeyD,
        C::KeyE => Key::KeyE,
        C::KeyF => Key::KeyF,
        C::KeyG => Key::KeyG,
        C::KeyH => Key::KeyH,
        C::KeyI => Key::KeyI,
        C::KeyJ => Key::KeyJ,
        C::KeyK => Key::KeyK,
        C::KeyL => Key::KeyL,
        C::KeyM => Key::KeyM,
        C::KeyN => Key::KeyN,
        C::KeyO => Key::KeyO,
        C::KeyP => Key::KeyP,
        C::KeyQ => Key::KeyQ,
        C::KeyR => Key::KeyR,
        C::KeyS => Key::KeyS,
        C::KeyT => Key::KeyT,
        C::KeyU => Key::KeyU,
        C::KeyV => Key::KeyV,
        C::KeyW => Key::KeyW,
        C::KeyX => Key::KeyX,
        C::KeyY => Key::KeyY,
        C::KeyZ => Key::KeyZ,
        C::Minus => Key::Minus,
        C::Period => Key::Period,
        C::Quote => Key::Quote,
        C::Semicolon => Key::Semicolon,
        C::Slash => Key::Slash,
        // Functional
        C::AltLeft => Key::AltLeft,
        C::AltRight => Key::AltRight,
        C::Backspace => Key::Backspace,
        C::CapsLock => Key::CapsLock,
        C::ContextMenu => Key::ContextMenu,
        C::ControlLeft => Key::ControlLeft,
        C::ControlRight => Key::ControlRight,
        C::Enter => Key::Enter,
        C::SuperLeft => Key::MetaLeft,
        C::SuperRight => Key::MetaRight,
        C::ShiftLeft => Key::ShiftLeft,
        C::ShiftRight => Key::ShiftRight,
        C::Space => Key::Space,
        C::Tab => Key::Tab,
        C::Convert => Key::Convert,
        C::KanaMode => Key::KanaMode,
        C::Lang1 => Key::Lang1,
        C::Lang2 => Key::Lang2,
        C::Lang3 => Key::Lang3,
        C::Lang4 => Key::Lang4,
        C::Lang5 => Key::Lang5,
        C::NonConvert => Key::NonConvert,
        // Control pad
        C::Delete => Key::Delete,
        C::End => Key::End,
        C::Help => Key::Help,
        C::Home => Key::Home,
        C::Insert => Key::Insert,
        C::PageDown => Key::PageDown,
        C::PageUp => Key::PageUp,
        // Arrow pad
        C::ArrowDown => Key::ArrowDown,
        C::ArrowLeft => Key::ArrowLeft,
        C::ArrowRight => Key::ArrowRight,
        C::ArrowUp => Key::ArrowUp,
        // Numpad
        C::NumLock => Key::NumLock,
        C::Numpad0 => Key::Numpad0,
        C::Numpad1 => Key::Numpad1,
        C::Numpad2 => Key::Numpad2,
        C::Numpad3 => Key::Numpad3,
        C::Numpad4 => Key::Numpad4,
        C::Numpad5 => Key::Numpad5,
        C::Numpad6 => Key::Numpad6,
        C::Numpad7 => Key::Numpad7,
        C::Numpad8 => Key::Numpad8,
        C::Numpad9 => Key::Numpad9,
        C::NumpadAdd => Key::NumpadAdd,
        C::NumpadBackspace => Key::NumpadBackspace,
        C::NumpadClear => Key::NumpadClear,
        C::NumpadClearEntry => Key::NumpadClearEntry,
        C::NumpadComma => Key::NumpadComma,
        C::NumpadDecimal => Key::NumpadDecimal,
        C::NumpadDivide => Key::NumpadDivide,
        C::NumpadEnter => Key::NumpadEnter,
        C::NumpadEqual => Key::NumpadEqual,
        C::NumpadHash => Key::NumpadHash,
        C::NumpadMemoryAdd => Key::NumpadMemoryAdd,
        C::NumpadMemoryClear => Key::NumpadMemoryClear,
        C::NumpadMemoryRecall => Key::NumpadMemoryRecall,
        C::NumpadMemoryStore => Key::NumpadMemoryStore,
        C::NumpadMemorySubtract => Key::NumpadMemorySubtract,
        C::NumpadMultiply => Key::NumpadMultiply,
        C::NumpadParenLeft => Key::NumpadParenLeft,
        C::NumpadParenRight => Key::NumpadParenRight,
        C::NumpadStar => Key::NumpadStar,
        C::NumpadSubtract => Key::NumpadSubtract,
        // Function section
        C::Escape => Key::Escape,
        C::Fn => Key::Fn,
        C::FnLock => Key::FnLock,
        C::PrintScreen => Key::PrintScreen,
        C::ScrollLock => Key::ScrollLock,
        C::Pause => Key::Pause,
        // Media
        C::BrowserBack => Key::BrowserBack,
        C::BrowserFavorites => Key::BrowserFavorites,
        C::BrowserForward => Key::BrowserForward,
        C::BrowserHome => Key::BrowserHome,
        C::BrowserRefresh => Key::BrowserRefresh,
        C::BrowserSearch => Key::BrowserSearch,
        C::BrowserStop => Key::BrowserStop,
        C::Eject => Key::Eject,
        C::LaunchApp1 => Key::LaunchApp1,
        C::LaunchApp2 => Key::LaunchApp2,
        C::LaunchMail => Key::LaunchMail,
        C::MediaPlayPause => Key::MediaPlayPause,
        C::MediaSelect => Key::MediaSelect,
        C::MediaStop => Key::MediaStop,
        C::MediaTrackNext => Key::MediaTrackNext,
        C::MediaTrackPrevious => Key::MediaTrackPrevious,
        C::Power => Key::Power,
        C::Sleep => Key::Sleep,
        C::AudioVolumeDown => Key::AudioVolumeDown,
        C::AudioVolumeMute => Key::AudioVolumeMute,
        C::AudioVolumeUp => Key::AudioVolumeUp,
        C::WakeUp => Key::WakeUp,
        // Legacy / non-standard
        C::Meta => Key::Super,
        C::Hyper => Key::Hyper,
        C::Turbo => Key::Turbo,
        C::Abort => Key::Abort,
        C::Resume => Key::Resume,
        C::Suspend => Key::Suspend,
        C::Again => Key::Again,
        C::Copy => Key::Copy,
        C::Cut => Key::Cut,
        C::Find => Key::Find,
        C::Open => Key::Open,
        C::Paste => Key::Paste,
        C::Props => Key::Props,
        C::Select => Key::Select,
        C::Undo => Key::Undo,
        C::Hiragana => Key::Hiragana,
        C::Katakana => Key::Katakana,
        // F keys
        C::F1 => Key::F1,
        C::F2 => Key::F2,
        C::F3 => Key::F3,
        C::F4 => Key::F4,
        C::F5 => Key::F5,
        C::F6 => Key::F6,
        C::F7 => Key::F7,
        C::F8 => Key::F8,
        C::F9 => Key::F9,
        C::F10 => Key::F10,
        C::F11 => Key::F11,
        C::F12 => Key::F12,
        // wasi:surface has no key beyond f12, so these carry no code to the
        // guest at all. Spelled out so the next reader knows it was checked.
        C::F13
        | C::F14
        | C::F15
        | C::F16
        | C::F17
        | C::F18
        | C::F19
        | C::F20
        | C::F21
        | C::F22
        | C::F23
        | C::F24
        | C::F25
        | C::F26
        | C::F27
        | C::F28
        | C::F29
        | C::F30
        | C::F31
        | C::F32
        | C::F33
        | C::F34
        | C::F35 => return None,
        // `KeyCode` is `#[non_exhaustive]`; anything winit adds later is
        // unmapped until this table is extended.
        _ => return None,
    })
}

/// The text winit resolved for each key that is currently down, so a key-up can
/// carry the same text its key-down did.
///
/// winit fills `KeyEvent::text` on press only on macOS and Windows, but fills it
/// on release too on Linux — so forwarding it verbatim would give guests
/// platform-dependent asymmetry. Guests are entitled to the DOM behaviour, where
/// `KeyboardEvent.key` is set on `keyup` as well, and some *depend* on it: DOOM's
/// key map has no letter branch on the physical code, so it recognises W/A/S/D
/// only by the event's character. Text on press alone would leave the player
/// walking forever, the release having mapped to nothing. Remembering the press
/// text and replaying it on release makes the two halves symmetric everywhere.
///
/// This is held-key state, so it has to be updated for *every* key event the
/// window sees, not only the ones that reach a guest — see the call in
/// `window_event`, next to `keys_down`, and the note on `resolve`.
///
/// The memo holds winit's own `SmolStr`, which stores anything up to 22 bytes
/// inline: remembering a keystroke costs no allocation, and the single `String`
/// the wasi-gfx record needs is built once, at the end.
#[derive(Default)]
pub(super) struct KeyText(HashMap<KeyCode, SmolStr>);

impl KeyText {
    /// The text this event should carry: winit's own on press (remembered on the
    /// way through), the remembered press text on release.
    ///
    /// Call this for every key event, *before* any branch that might swallow it.
    /// A press that is remembered but whose release is swallowed leaves an entry
    /// behind, and a later release of that key — its own press swallowed in turn,
    /// so nothing overwrote the entry — would replay the older keystroke's text.
    pub(super) fn resolve(
        &mut self,
        code: KeyCode,
        text: Option<&SmolStr>,
        pressed: bool,
    ) -> Option<String> {
        if !pressed {
            // Fall back to winit's release text for the platforms that do supply
            // it — and for a release whose press went to another window, the two
            // handlers sharing one memo but not one focus.
            return self
                .0
                .remove(&code)
                .or_else(|| text.filter(|t| !t.is_empty()).cloned())
                .map(String::from);
        }
        match text {
            Some(t) if !t.is_empty() => {
                self.0.insert(code, t.clone());
                Some(t.to_string())
            }
            // Arrows, modifiers, F-keys: no text at all. Drop any stale entry so
            // a later release cannot resurrect an older keystroke's character.
            _ => {
                self.0.remove(&code);
                None
            }
        }
    }
}

/// Build the wasi-gfx key event for a winit key press/release.
///
/// `text` is `KeyText::resolve`'s output — winit's resolved character for the
/// key, layout- and shift-aware, and already Ctrl-free on every platform
/// (Ctrl+C arrives as "c", exactly what a browser puts in `KeyboardEvent.key`).
/// It is passed through unmodified even when ctrl/meta are held: AltGr is a
/// legitimate text producer under Alt+Ctrl, so a host that stripped chorded text
/// would break German keyboards, and the decision of whether a chord *types* is
/// the guest's — a browser hands the character over the same way (Qt's
/// `QInputControl` refuses to insert it, QTBUG-35734). Guests derive the typed
/// character from this and nothing else — `map_key`'s code says *which* key,
/// never *what it types*.
///
/// Feeding this alongside `encode_term_key` cannot double up: a node that owns a
/// surface is never given a Terminal (see the compositor's terminal reconcile),
/// and every dispatch site is an if/else on surface-vs-terminal, so the buffer
/// the node does not read is cleared unread each frame.
pub(super) fn key_event(
    code: KeyCode,
    text: Option<String>,
    mods: ModifiersState,
    repeat: bool,
) -> KeyEvent {
    KeyEvent {
        key: map_key(code),
        text,
        alt_key: mods.alt_key(),
        ctrl_key: mods.control_key(),
        meta_key: mods.super_key(),
        shift_key: mods.shift_key(),
        repeat,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The regression this file exists to prevent: `text` was hardcoded `None`
    /// here, so guests could see *that* a key moved but never *what it typed*,
    /// and every Qt line edit was effectively read-only.
    #[test]
    fn a_press_carries_the_character_winit_resolved() {
        let mut memo = KeyText::default();
        let text = memo.resolve(KeyCode::KeyA, Some(&SmolStr::new("A")), true);
        let ev = key_event(KeyCode::KeyA, text, ModifiersState::SHIFT, false);
        assert_eq!(ev.key, Some(Key::KeyA));
        assert_eq!(ev.text.as_deref(), Some("A"));
        assert!(ev.shift_key);
    }

    /// winit gives no release text on macOS/Windows, but DOOM recognises W/A/S/D
    /// by the character alone — a press without its matching release leaves the
    /// player walking forever. The memo replays it on every platform.
    #[test]
    fn a_release_repeats_the_text_of_its_press() {
        let w = SmolStr::new("w");
        let mut memo = KeyText::default();
        memo.resolve(KeyCode::KeyW, Some(&w), true);
        assert_eq!(
            memo.resolve(KeyCode::KeyW, None, false).as_deref(),
            Some("w")
        );
        // Consumed: a second release invents nothing.
        assert_eq!(memo.resolve(KeyCode::KeyW, None, false), None);
        // A textless key stays textless rather than resurrecting an older one.
        memo.resolve(KeyCode::KeyW, Some(&w), true);
        memo.resolve(KeyCode::KeyW, None, true);
        assert_eq!(memo.resolve(KeyCode::KeyW, None, false), None);
        // Empty text is no text, on release as much as on press: winit's X11
        // backend fills the field either way and the two halves must agree.
        let empty = SmolStr::default();
        assert_eq!(memo.resolve(KeyCode::KeyW, Some(&empty), true), None);
        assert_eq!(memo.resolve(KeyCode::KeyW, Some(&empty), false), None);
    }

    /// Ctrl+C is "c", not 0x03 — winit hands over the ctrl-free text and it is
    /// forwarded unstripped, matching what a browser puts in `KeyboardEvent.key`.
    #[test]
    fn a_chord_keeps_the_plain_character() {
        let ev = key_event(
            KeyCode::KeyC,
            Some("c".into()),
            ModifiersState::CONTROL,
            false,
        );
        assert_eq!(ev.text.as_deref(), Some("c"));
        assert!(ev.ctrl_key);
    }

    /// The map used to stop after ~50 codes; everything else reached guests as
    /// `key: none`. `F13`+ still does, because wasi:surface has no such key.
    #[test]
    fn the_map_covers_the_whole_w3c_set_up_to_f12() {
        assert_eq!(map_key(KeyCode::NumpadEnter), Some(Key::NumpadEnter));
        assert_eq!(map_key(KeyCode::Home), Some(Key::Home));
        assert_eq!(map_key(KeyCode::PageDown), Some(Key::PageDown));
        assert_eq!(map_key(KeyCode::Semicolon), Some(Key::Semicolon));
        assert_eq!(map_key(KeyCode::F12), Some(Key::F12));
        // The Command/Windows key is `Super*` in winit, `Meta*` in W3C/wasi.
        assert_eq!(map_key(KeyCode::SuperLeft), Some(Key::MetaLeft));
        assert_eq!(map_key(KeyCode::F13), None);
    }

    /// A 171-arm hand-written table's realistic failure is not a missing arm —
    /// it is a *slid* one, `C::NumpadStar => Key::NumpadSubtract`, which
    /// compiles and reads fine and sends the guest a plausible wrong key. So
    /// check the property the table is built on instead of the entries: both
    /// enums are the W3C UIEvents `code` set, so a mapped code and its key
    /// must carry the SAME NAME. Debug is that name — wasmtime's bindgen
    /// qualifies its enums (`Key::Backquote`), winit's does not.
    ///
    /// Two codes per W3C section at minimum, weighted towards the sections the
    /// old ~55-arm map did not reach at all (numpad, media, legacy, intl/lang):
    /// those are the ones no other test has ever looked at.
    #[test]
    fn mapped_codes_keep_their_w3c_name() {
        use KeyCode as C;
        let sample = [
            // Writing system, incl. the international keys.
            C::Backquote,
            C::BracketRight,
            C::Digit7,
            C::Equal,
            C::IntlBackslash,
            C::IntlRo,
            C::IntlYen,
            C::KeyQ,
            C::Quote,
            C::Slash,
            // Functional, incl. the IME/lang block.
            C::AltRight,
            C::CapsLock,
            C::ContextMenu,
            C::ControlLeft,
            C::Convert,
            C::KanaMode,
            C::Lang3,
            C::NonConvert,
            C::ShiftRight,
            C::Space,
            // Control pad + arrow pad.
            C::Delete,
            C::End,
            C::Help,
            C::Insert,
            C::PageUp,
            C::ArrowLeft,
            C::ArrowUp,
            // Numpad — 20 arms the old map had none of.
            C::NumLock,
            C::Numpad5,
            C::NumpadAdd,
            C::NumpadBackspace,
            C::NumpadClearEntry,
            C::NumpadComma,
            C::NumpadDecimal,
            C::NumpadDivide,
            C::NumpadEqual,
            C::NumpadHash,
            C::NumpadMemoryAdd,
            C::NumpadMemoryRecall,
            C::NumpadMemorySubtract,
            C::NumpadMultiply,
            C::NumpadParenLeft,
            C::NumpadParenRight,
            C::NumpadStar,
            C::NumpadSubtract,
            // Function section.
            C::Escape,
            C::Fn,
            C::FnLock,
            C::F1,
            C::F12,
            C::PrintScreen,
            C::ScrollLock,
            C::Pause,
            // Media.
            C::BrowserFavorites,
            C::BrowserRefresh,
            C::Eject,
            C::LaunchApp2,
            C::LaunchMail,
            C::MediaPlayPause,
            C::MediaTrackPrevious,
            C::Power,
            C::Sleep,
            C::AudioVolumeMute,
            C::WakeUp,
            // Legacy / non-standard.
            C::Hyper,
            C::Turbo,
            C::Abort,
            C::Resume,
            C::Suspend,
            C::Again,
            C::Copy,
            C::Cut,
            C::Find,
            C::Open,
            C::Paste,
            C::Props,
            C::Select,
            C::Undo,
            C::Hiragana,
            C::Katakana,
        ];
        for code in sample {
            let key = map_key(code).unwrap_or_else(|| panic!("{code:?} reaches a guest as none"));
            assert_eq!(
                format!("{key:?}").trim_start_matches("Key::"),
                format!("{code:?}"),
                "{code:?} is mapped to the wrong key"
            );
        }
    }

    /// The three arms that are deliberately NOT a name match, kept honest so
    /// nobody "corrects" them. `Super*` is W3C's `Meta*` under winit's name —
    /// the Command/Windows key, very much live — while winit's `Meta` is the
    /// legacy trio's third member, which wasi spells `super`. Getting these two
    /// confused is the one way this table can be wrong in a way that matters.
    #[test]
    fn the_names_that_deliberately_differ() {
        assert_eq!(map_key(KeyCode::SuperLeft), Some(Key::MetaLeft));
        assert_eq!(map_key(KeyCode::SuperRight), Some(Key::MetaRight));
        assert_eq!(map_key(KeyCode::Meta), Some(Key::Super));
        // ...and the neighbours that would absorb them if they slid.
        assert_eq!(map_key(KeyCode::Hyper), Some(Key::Hyper));
        assert_eq!(map_key(KeyCode::Turbo), Some(Key::Turbo));
    }

    /// The residue: winit codes with no wasi:surface counterpart. macOS emits
    /// F13-F35 from real scancodes, so these are a gap guests will feel, and
    /// `none` is the honest answer rather than a wrong neighbouring key.
    #[test]
    fn winit_only_codes_reach_the_guest_as_none() {
        use KeyCode as C;
        for code in [
            C::F13,
            C::F14,
            C::F15,
            C::F16,
            C::F17,
            C::F18,
            C::F19,
            C::F20,
            C::F21,
            C::F22,
            C::F23,
            C::F24,
            C::F25,
            C::F26,
            C::F27,
            C::F28,
            C::F29,
            C::F30,
            C::F31,
            C::F32,
            C::F33,
            C::F34,
            C::F35,
        ] {
            assert_eq!(map_key(code), None, "{code:?} should map to no wasi key");
        }
        // A key event for one still reaches the guest — text and modifiers are
        // worth having even when the code is not — it just carries no code.
        let ev = key_event(KeyCode::F13, None, ModifiersState::empty(), false);
        assert_eq!(ev.key, None);
    }

    /// The terminal half of this module, which the `text` work ran alongside
    /// and must not have disturbed: a node with a Terminal and no surface still
    /// gets the same stdin bytes it always did. Ctrl+letter is the one place
    /// where wk, not the guest, decides what a chord means — that asymmetry
    /// with `key_event` (which forwards the plain character) is deliberate,
    /// because a pty expects the control byte.
    #[test]
    fn the_terminal_path_is_unchanged() {
        let n = ModifiersState::empty();
        let bytes = |c, t, m| encode_term_key(c, t, m);
        assert_eq!(bytes(KeyCode::Enter, Some("\r"), n), Some(b"\r".to_vec()));
        assert_eq!(bytes(KeyCode::NumpadEnter, None, n), Some(b"\r".to_vec()));
        assert_eq!(bytes(KeyCode::Backspace, None, n), Some(vec![0x7f]));
        assert_eq!(bytes(KeyCode::Tab, Some("\t"), n), Some(b"\t".to_vec()));
        assert_eq!(bytes(KeyCode::Escape, None, n), Some(vec![0x1b]));
        assert_eq!(bytes(KeyCode::ArrowUp, None, n), Some(b"\x1b[A".to_vec()));
        assert_eq!(bytes(KeyCode::ArrowDown, None, n), Some(b"\x1b[B".to_vec()));
        assert_eq!(
            bytes(KeyCode::ArrowRight, None, n),
            Some(b"\x1b[C".to_vec())
        );
        assert_eq!(bytes(KeyCode::ArrowLeft, None, n), Some(b"\x1b[D".to_vec()));
        assert_eq!(bytes(KeyCode::Home, None, n), Some(b"\x1b[H".to_vec()));
        assert_eq!(bytes(KeyCode::End, None, n), Some(b"\x1b[F".to_vec()));
        // Anything else is winit's own text, verbatim and UTF-8 (a dead-key
        // composition or an AltGr character is several bytes, not one).
        assert_eq!(bytes(KeyCode::KeyA, Some("a"), n), Some(b"a".to_vec()));
        assert_eq!(
            bytes(KeyCode::Semicolon, Some("ö"), n),
            Some("ö".as_bytes().to_vec())
        );
        // A key that types nothing sends nothing — an empty string included,
        // which is what winit's X11 backend reports for modifiers.
        assert_eq!(bytes(KeyCode::ShiftLeft, None, n), None);
        assert_eq!(bytes(KeyCode::ShiftLeft, Some(""), n), None);
        // Ctrl-A .. Ctrl-Z are 0x01..0x1a, and beat the text branch.
        let ctrl = ModifiersState::CONTROL;
        assert_eq!(bytes(KeyCode::KeyA, Some("a"), ctrl), Some(vec![0x01]));
        assert_eq!(bytes(KeyCode::KeyC, Some("c"), ctrl), Some(vec![0x03]));
        assert_eq!(bytes(KeyCode::KeyZ, Some("z"), ctrl), Some(vec![0x1a]));
        // Ctrl+non-letter falls through to the ordinary handling.
        assert_eq!(bytes(KeyCode::Enter, None, ctrl), Some(b"\r".to_vec()));
    }
}
