//! twitch refuses a message identical to your previous one in the same channel
//! (msg_duplicate). chatterino's bypass: append an invisible tag char, so the
//! wire text differs. it toggles — a third identical send goes out plain.

/// the invisible char twitch accepts as "different text".
const TAG: char = '\u{E0000}';
const SUFFIX: &str = " \u{E0000}";

/// the text to put on the wire, given the last wire text sent to this channel.
pub fn bypass(text: &str, last_wire: Option<&str>) -> String {
    if last_wire == Some(text) {
        format!("{text}{SUFFIX}")
    } else {
        text.to_string()
    }
}

/// drop our bypass suffix (" \u{E0000}" or a bare trailing \u{E0000}) so echoes
/// compare and display as the user typed them.
pub fn strip(text: &str) -> &str {
    let t = text.strip_suffix(TAG).unwrap_or(text);
    if t.len() == text.len() {
        return text;
    }
    t.strip_suffix(' ').unwrap_or(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_and_different_sends_go_plain() {
        assert_eq!(bypass("hi", None), "hi");
        assert_eq!(bypass("hi", Some("yo")), "hi");
    }

    #[test]
    fn repeat_gets_suffix_and_toggles_back() {
        let a = bypass("hi", Some("hi"));
        assert_eq!(a, "hi \u{E0000}");
        // next identical send: last wire was the suffixed one, so plain
        assert_eq!(bypass("hi", Some(&a)), "hi");
    }

    #[test]
    fn strip_handles_spaced_and_bare_suffix() {
        assert_eq!(strip("hi \u{E0000}"), "hi");
        assert_eq!(strip("hi\u{E0000}"), "hi");
        assert_eq!(strip("hi"), "hi");
        assert_eq!(strip("hi "), "hi ");
        assert_eq!(strip("\u{E0000}"), "");
    }
}
