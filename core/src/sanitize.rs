//! terminal-injection defense. chat text is UNTRUSTED — a raw escape sequence
//! in a message could retitle the terminal, poke the clipboard, or worse on some
//! emulators. we render into a terminal, so we strip every control byte before a
//! message is ever handed to the tui. this is the one risk chatterino never has.
//!
//! policy: keep printable text + emoji + normal whitespace; drop ALL C0/C1
//! control chars and everything an escape sequence is built from. our own
//! styling is applied structurally by the renderer, never smuggled in text.

/// strip control characters, returning render-safe text. tabs → space, newlines
/// dropped (chat is one line per message; layout owns line breaks).
pub fn clean(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\t' => out.push(' '),
            // C0 controls (incl. ESC 0x1B, BEL, backspace) and DEL.
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {}
            // C1 controls (0x80–0x9F) — the 8-bit CSI/OSC introducers.
            c if (0x80..=0x9f).contains(&(c as u32)) => {}
            // zero-width / bidi-override troublemakers used to spoof text.
            '\u{200b}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
            | '\u{feff}' => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_escape_sequences() {
        // an OSC that would retitle the terminal, plus a raw CSI color.
        let evil = "hi\u{1b}]0;pwned\u{7}\u{1b}[31mred\u{1b}[0m";
        assert_eq!(clean(evil), "hi]0;pwned[31mred[0m");
        // no ESC/BEL bytes survive.
        assert!(!clean(evil).contains('\u{1b}'));
        assert!(!clean(evil).contains('\u{7}'));
    }

    #[test]
    fn keeps_text_and_emoji() {
        assert_eq!(clean("gigachad 🔥 KEKW"), "gigachad 🔥 KEKW");
    }

    #[test]
    fn drops_bidi_and_zero_width() {
        assert_eq!(clean("a\u{202e}b\u{200b}c"), "abc");
    }

    #[test]
    fn tab_becomes_space_newline_dropped() {
        assert_eq!(clean("a\tb\nc"), "a bc");
    }
}
