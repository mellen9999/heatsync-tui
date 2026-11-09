//! filled notification rows: which dim fill a line wears, and painting it.
//! olive = stream events, purple = new people, navy = support, teal = channel
//! points + the broadcaster's voice, red = aimed at you. the fill is the dim
//! twin of the row's meaning; body text goes white and the name sits on a black
//! cutout. there is no zebra striping in this tui, so a filled row has nothing
//! to skip — it is simply never striped.

use crate::palette;
use crate::streamrow;
use heatsync_core::{Message, NoteKind};
use ratatui::style::{Color, Style};

pub(crate) use crate::palette::Fill;

/// the fill this line wears, if any. a line aimed at you outranks the rest.
pub(crate) fn of(m: &Message, pinged: bool) -> Option<Fill> {
    if m.gone.is_some() {
        return None;
    }
    if pinged {
        return Some(Fill::Red);
    }
    if streamrow::is_row(m) {
        return Some(Fill::Olive);
    }
    if let Some(n) = &m.note {
        use NoteKind as K;
        return match n.kind {
            K::Live | K::Offline | K::Category => Some(Fill::Olive),
            K::Raid => Some(Fill::Purple),
            K::Sub | K::Gift | K::Cheer | K::Streak | K::Milestone | K::Charity => {
                Some(Fill::Navy)
            }
            K::Redeem | K::Announce(_) => Some(Fill::Teal),
            K::Notice | K::Spike | K::Mod | K::Ritual | K::Mode => None,
        };
    }
    if m.extra.first {
        return Some(Fill::Purple);
    }
    m.extra.highlight.then_some(Fill::Teal)
}

/// paint every row of a laid-out line: background on the whole row, gray text
/// lifted to white (body text is white on a fill).
pub(crate) fn paint(rows: &mut [crate::layout::RowPlan], f: Fill) {
    for r in rows {
        r.line.style = r.line.style.bg(f.bg());
        for sp in &mut r.line.spans {
            if sp.style.fg == Some(palette::READ) {
                sp.style = sp.style.fg(Color::White);
            }
        }
    }
}

/// the name's cutout: black behind the name so it reads on any fill.
pub(crate) fn cutout(s: Style) -> Style {
    s.bg(Color::Black)
}

#[cfg(test)]
mod tests {
    use super::*;
    use heatsync_core::{Note, Platform};

    fn msg() -> Message {
        Message {
            platform: Platform::Twitch,
            user: "u".into(),
            text: "hi".into(),
            color: None,
            badges: Vec::new(),
            id: None,
            gone: None,
            reply_to: None,
            note: None,
            extra: Default::default(),
            emotes: None,
            heat: 0.0,
            uid: None,
            tags: Vec::new(),
        }
    }

    fn note(k: NoteKind) -> Message {
        let mut m = msg();
        m.note = Some(Note {
            kind: k,
            what: "x".into(),
        });
        m
    }

    #[test]
    fn each_family_wears_its_dim_twin() {
        assert_eq!(of(&note(NoteKind::Live), false), Some(Fill::Olive));
        assert_eq!(of(&note(NoteKind::Raid), false), Some(Fill::Purple));
        assert_eq!(of(&note(NoteKind::Sub), false), Some(Fill::Navy));
        assert_eq!(of(&note(NoteKind::Cheer), false), Some(Fill::Navy));
        assert_eq!(of(&note(NoteKind::Redeem), false), Some(Fill::Teal));
        assert_eq!(
            of(&note(NoteKind::Announce(heatsync_core::Tint::Plain)), false),
            Some(Fill::Teal)
        );
        assert_eq!(of(&note(NoteKind::Mod), false), None, "mod actions stay plain");
        let mut first = msg();
        first.extra.first = true;
        assert_eq!(of(&first, false), Some(Fill::Purple));
        assert_eq!(of(&msg(), false), None, "plain chat never gets a fill");
    }

    #[test]
    fn aimed_at_you_is_red_and_beats_the_rest() {
        let mut first = msg();
        first.extra.first = true;
        assert_eq!(of(&first, true), Some(Fill::Red));
        assert_eq!(of(&msg(), true), Some(Fill::Red));
    }

    #[test]
    fn a_removed_line_loses_its_fill() {
        let mut m = note(NoteKind::Sub);
        m.gone = Some("deleted".into());
        assert_eq!(of(&m, false), None);
    }
}
