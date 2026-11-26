//! big emote-only lines — does this line draw its pictures bigger. the site's
//! `shared/emote-only.ts`, ported; `core/tests/emote-only.vectors.json` is the
//! contract (see `core/tests/emote_only_vectors.rs`).
//!
//! the rule reads the LAW'S read of a line, never its text: each word, in
//! order, is one of [`Kind`]. so there is no second resolver here: whatever the
//! row already drew as a picture is a picture, and [`verdict_of`] derives the
//! read from the segments [`crate::emote::segments_with`] laid out. an overlay
//! and a bound modifier belong to their picture and are never counted. one
//! text word, anywhere, makes a normal line; 1 to [`BIG_MAX`] pictures and no
//! text draw big; 6 or more stay normal (a spam wall stays dense).

use crate::emote::Segment;

/// the most pictures a big line holds; one more and the line is a wall.
pub const BIG_MAX: usize = 5;
/// big = this many times the line's normal emote height.
pub const BIG_SCALE: u16 = 2;

/// what one word of a line draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// a picture the law drew (room, user, global or platform-native).
    Emote,
    /// one unicode emoji (a word of three emoji is three of these).
    Emoji,
    /// a zero-width emote stacked on the picture before it.
    Overlay,
    /// a modifier word bound to a picture.
    Mod,
    /// anything else: a word, a link, a mention, a name the room does not
    /// know, a taken-down emote, a modifier with nothing to change.
    Text,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub big: bool,
    /// emotes and emoji counted, never an overlay or a modifier.
    pub pictures: usize,
}

#[derive(Default)]
struct Tally {
    pictures: usize,
    text: bool,
    /// a picture is on the line for an overlay to sit on.
    open: bool,
}

impl Tally {
    fn push(&mut self, k: Kind) {
        match k {
            Kind::Text => self.text = true,
            Kind::Emote | Kind::Emoji => {
                self.pictures += 1;
                self.open = true;
            }
            // an overlay with no picture before it draws inline: it is a picture
            Kind::Overlay if !self.open => {
                self.pictures += 1;
                self.open = true;
            }
            Kind::Overlay | Kind::Mod => {}
        }
    }

    fn verdict(&self) -> Verdict {
        Verdict {
            big: !self.text && (1..=BIG_MAX).contains(&self.pictures),
            pictures: self.pictures,
        }
    }
}

/// the rule on a read: one [`Kind`] per word of the line, in order.
pub fn emote_only(read: &[Kind]) -> Verdict {
    let mut t = Tally::default();
    for &k in read {
        t.push(k);
    }
    t.verdict()
}

/// the rule on the line the terminal drew: an emote stack is a picture (its
/// extra layers are overlays), a word of emoji is a picture per emoji, any
/// other word is text. a modifier bound to a stack is inside the stack, and an
/// unbound one stayed a word, so neither needs a read of its own.
pub fn verdict_of(segs: &[Segment]) -> Verdict {
    let mut t = Tally::default();
    for seg in segs {
        match seg {
            Segment::Stack(s) => {
                t.push(Kind::Emote);
                for _ in 1..s.layers.len() {
                    t.push(Kind::Overlay);
                }
            }
            Segment::Text(text) => {
                for w in text.split_whitespace() {
                    match emoji_run(w) {
                        Some(n) => (0..n).for_each(|_| t.push(Kind::Emoji)),
                        None => t.push(Kind::Text),
                    }
                }
            }
        }
    }
    t.verdict()
}

/// how many emoji a word is made of, None when it is anything but emoji. a unit
/// is a flag (two regional indicators), or a pictograph drawn as emoji (it
/// defaults to it, or is followed by the emoji selector) with an optional skin
/// tone, joined ones (zwj) and subdivision-flag tags. an approximation of the
/// unicode emoji sequence, with no table of every sequence and no dependency.
fn emoji_run(word: &str) -> Option<usize> {
    const VS16: char = '\u{FE0F}';
    const ZWJ: char = '\u{200D}';
    let skin = |c: &char| ('\u{1F3FB}'..='\u{1F3FF}').contains(c);
    let flag = |c: &char| ('\u{1F1E6}'..='\u{1F1FF}').contains(c);
    let tag = |c: &char| ('\u{E0020}'..='\u{E007F}').contains(c);
    let mut it = word.chars().peekable();
    let mut n = 0;
    while let Some(c) = it.next() {
        if flag(&c) {
            it.next_if(flag)?;
        } else if pictograph(c) {
            if it.next_if_eq(&VS16).is_none() && !emoji_default(c) {
                return None;
            }
            it.next_if(skin);
            while it.next_if_eq(&ZWJ).is_some() {
                if !pictograph(it.next()?) {
                    return None;
                }
                it.next_if_eq(&VS16);
                it.next_if(skin);
            }
            while it.next_if(tag).is_some() {}
        } else {
            return None;
        }
        n += 1;
    }
    (n > 0).then_some(n)
}

/// a character that can be an emoji at all.
fn pictograph(c: char) -> bool {
    matches!(c as u32,
        0x00A9 | 0x00AE | 0x203C | 0x2049 | 0x2122 | 0x2139 | 0x2194..=0x21AA
        | 0x231A..=0x23FF | 0x24C2 | 0x25AA..=0x25FE | 0x2600..=0x27BF | 0x2934..=0x2935
        | 0x2B05..=0x2B55 | 0x3030 | 0x303D | 0x3297 | 0x3299 | 0x1F000..=0x1FAFF)
}

/// a character a terminal draws as emoji without the emoji selector.
fn emoji_default(c: char) -> bool {
    matches!(c as u32,
        0x231A..=0x231B | 0x23E9..=0x23EC | 0x23F0 | 0x23F3 | 0x25FD..=0x25FE | 0x2614..=0x2615
        | 0x2648..=0x2653 | 0x267F | 0x2693 | 0x26A1 | 0x26AA..=0x26AB | 0x26BD..=0x26BE
        | 0x26C4..=0x26C5 | 0x26CE | 0x26D4 | 0x26EA | 0x26F2..=0x26F3 | 0x26F5 | 0x26FA
        | 0x26FD | 0x2705 | 0x270A..=0x270B | 0x2728 | 0x274C | 0x274E | 0x2753..=0x2755
        | 0x2757 | 0x2795..=0x2797 | 0x27B0 | 0x27BF | 0x2B1B..=0x2B1C | 0x2B50 | 0x2B55
        | 0x1F004 | 0x1F0CF | 0x1F18E | 0x1F191..=0x1F19A | 0x1F201 | 0x1F21A | 0x1F22F
        | 0x1F232..=0x1F236 | 0x1F238..=0x1F23A | 0x1F250..=0x1F251
        | 0x1F300..=0x1F64F | 0x1F680..=0x1F6FF | 0x1F900..=0x1F9FF | 0x1FA70..=0x1FAFF)
}

#[cfg(test)]
mod tests {
    use super::*;
    use Kind::*;

    #[test]
    fn five_pictures_are_big_and_six_are_a_wall() {
        assert_eq!(
            emote_only(&[Emote; 5]),
            Verdict {
                big: true,
                pictures: 5
            }
        );
        assert_eq!(
            emote_only(&[Emote; 6]),
            Verdict {
                big: false,
                pictures: 6
            }
        );
        assert_eq!(
            emote_only(&[]),
            Verdict {
                big: false,
                pictures: 0
            }
        );
    }

    #[test]
    fn overlays_and_modifiers_are_never_counted_and_one_word_ends_it() {
        assert_eq!(emote_only(&[Mod, Emote, Overlay, Mod]).pictures, 1);
        assert!(!emote_only(&[Emote, Text, Emote]).big);
        // an overlay with nothing under it draws inline, so it is a picture
        assert_eq!(
            emote_only(&[Overlay]),
            Verdict {
                big: true,
                pictures: 1
            }
        );
        assert_eq!(emote_only(&[Text, Overlay]).pictures, 1);
    }

    #[test]
    fn a_word_of_emoji_is_a_picture_each() {
        for (w, n) in [
            ("😂", 1),
            ("😂😂😂", 3),
            ("🔥", 1),
            ("❤️", 1), // heart + emoji selector
            ("✅", 1), // emoji by default
            ("👍🏽", 1), // skin tone
            ("🇸🇪", 1), // a flag is two regional indicators
            ("🇸🇪🇳🇴", 2),
            ("👨‍👩‍👧‍👦", 1), // zwj family
            ("🏴󠁧󠁢󠁥󠁮󠁧󠁿", 1), // subdivision flag
            ("🏳️‍🌈", 1),
        ] {
            assert_eq!(emoji_run(w), Some(n), "{w}");
        }
    }

    #[test]
    fn anything_else_is_not_emoji() {
        for w in [
            "",
            "a",
            "Kappa",
            "Kappa😂",
            "😂!",
            "1",
            "#",
            "™",
            "♥",
            "©",
            "→",
            "🇸",
            "http://x",
            "w!",
        ] {
            assert_eq!(emoji_run(w), None, "{w:?}");
        }
        // text-default symbols become emoji with the selector, and not without
        assert_eq!(emoji_run("♥\u{FE0F}"), Some(1));
        assert_eq!(emoji_run("™\u{FE0F}"), Some(1));
    }
}
