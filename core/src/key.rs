//! Face-independent key events.
//!
//! The editor and the vi model are the same logic whether the keystroke came
//! from a terminal or a window, but each face has its own key type: the tui
//! reads `crossterm::event::KeyEvent`, a gui reads whatever its toolkit hands
//! it. Neither belongs in shared code — core takes no io dependencies.
//!
//! So core owns the vocabulary and each face maps into it. The shape here
//! deliberately mirrors crossterm's (`code` + `modifiers`, a `contains` on the
//! modifier set) so the terminal face's mapping is mechanical and the editor
//! reads the same as it always did.

/// Every key the editor and vi model actually distinguish. Closed on purpose —
/// a face that sees something else drops it rather than widening this.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyCode {
    Esc,
    Enter,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    Tab,
    BackTab,
    Char(char),
}

/// Held modifiers. `contains` asks "are all of the bits in `other` set here",
/// matching the bitflags semantics the call sites were written against.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct KeyModifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl KeyModifiers {
    pub const NONE: Self = Self { ctrl: false, alt: false, shift: false };
    pub const CONTROL: Self = Self { ctrl: true, alt: false, shift: false };
    pub const ALT: Self = Self { ctrl: false, alt: true, shift: false };
    pub const SHIFT: Self = Self { ctrl: false, alt: false, shift: true };

    pub fn contains(self, other: Self) -> bool {
        (!other.ctrl || self.ctrl) && (!other.alt || self.alt) && (!other.shift || self.shift)
    }
}

/// One keystroke.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KeyEvent {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl KeyEvent {
    pub fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self { code, modifiers }
    }

    /// An unmodified keystroke — the common case in tests and in plain typing.
    pub fn bare(code: KeyCode) -> Self {
        Self { code, modifiers: KeyModifiers::NONE }
    }

    pub fn ctrl(code: KeyCode) -> Self {
        Self { code, modifiers: KeyModifiers::CONTROL }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_contains_nothing_and_is_contained_by_everything() {
        assert!(KeyModifiers::NONE.contains(KeyModifiers::NONE));
        assert!(!KeyModifiers::NONE.contains(KeyModifiers::CONTROL));
        assert!(KeyModifiers::CONTROL.contains(KeyModifiers::NONE));
    }

    #[test]
    fn contains_asks_for_a_subset_not_equality() {
        let ctrl_shift = KeyModifiers { ctrl: true, alt: false, shift: true };
        assert!(ctrl_shift.contains(KeyModifiers::CONTROL));
        assert!(ctrl_shift.contains(KeyModifiers::SHIFT));
        assert!(!ctrl_shift.contains(KeyModifiers::ALT));
    }

    #[test]
    fn constructors_agree_with_literals() {
        assert_eq!(
            KeyEvent::bare(KeyCode::Enter),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
        );
        assert_eq!(
            KeyEvent::ctrl(KeyCode::Char('w')),
            KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)
        );
    }
}
