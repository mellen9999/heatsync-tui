//! crossterm → `heatsync_core::key`.
//!
//! The editor moved into core so a second face could share it, which meant it
//! could no longer speak crossterm. This is the terminal face's half of that
//! trade: one mapping, at the one place keys enter the editor.
//!
//! Keys core has no vocabulary for map to `None` and are dropped by the caller
//! — that is deliberate. Widening `core::key::KeyCode` is a decision about the
//! editing model, not something a face should be able to force.

use crossterm::event::{KeyCode as X, KeyEvent as XEvent, KeyModifiers as XMods};
use heatsync_core::key::{KeyCode, KeyEvent, KeyModifiers};

pub fn to_core(k: XEvent) -> Option<KeyEvent> {
    let code = match k.code {
        X::Esc => KeyCode::Esc,
        X::Enter => KeyCode::Enter,
        X::Backspace => KeyCode::Backspace,
        X::Delete => KeyCode::Delete,
        X::Left => KeyCode::Left,
        X::Right => KeyCode::Right,
        X::Up => KeyCode::Up,
        X::Down => KeyCode::Down,
        X::Home => KeyCode::Home,
        X::End => KeyCode::End,
        X::Tab => KeyCode::Tab,
        X::BackTab => KeyCode::BackTab,
        X::Char(c) => KeyCode::Char(c),
        _ => return None,
    };
    Some(KeyEvent::new(
        code,
        KeyModifiers {
            ctrl: k.modifiers.contains(XMods::CONTROL),
            alt: k.modifiers.contains(XMods::ALT),
            shift: k.modifiers.contains(XMods::SHIFT),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carries_the_character_and_the_ctrl_bit() {
        let got = to_core(XEvent::new(X::Char('w'), XMods::CONTROL)).unwrap();
        assert_eq!(got.code, KeyCode::Char('w'));
        assert!(got.modifiers.contains(KeyModifiers::CONTROL));
    }

    #[test]
    fn maps_every_special_the_editor_understands() {
        for (x, want) in [
            (X::Esc, KeyCode::Esc),
            (X::Enter, KeyCode::Enter),
            (X::Backspace, KeyCode::Backspace),
            (X::Delete, KeyCode::Delete),
            (X::Left, KeyCode::Left),
            (X::Right, KeyCode::Right),
            (X::Up, KeyCode::Up),
            (X::Down, KeyCode::Down),
            (X::Home, KeyCode::Home),
            (X::End, KeyCode::End),
            (X::Tab, KeyCode::Tab),
            (X::BackTab, KeyCode::BackTab),
        ] {
            assert_eq!(to_core(XEvent::new(x, XMods::NONE)).unwrap().code, want);
        }
    }

    #[test]
    fn drops_keys_core_has_no_word_for() {
        assert!(to_core(XEvent::new(X::F(5), XMods::NONE)).is_none());
        assert!(to_core(XEvent::new(X::Insert, XMods::NONE)).is_none());
    }

    #[test]
    fn an_unmodified_key_carries_no_modifiers() {
        let got = to_core(XEvent::new(X::Char('a'), XMods::NONE)).unwrap();
        assert_eq!(got.modifiers, KeyModifiers::NONE);
    }
}
