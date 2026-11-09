//! the heatsync badge catalogue: the one mark a chatter picked to show.
//!
//! the wire carries only the id (`/api/paints` → `badges`); glyph and colour
//! live here, so an id this build doesn't know simply draws nothing. a mirror
//! of the site's badge-catalogue.js — ansi hues only. nothing is shown unless
//! the chatter picked it (the server only sends picks they still earn).

use crate::heat::Hue;

#[derive(Debug, PartialEq, Eq)]
pub struct HsBadge {
    pub id: &'static str,
    pub glyph: char,
    pub hue: Hue,
}

pub const BADGES: [HsBadge; 8] = [
    HsBadge { id: "staff", glyph: '■', hue: Hue::White },
    HsBadge { id: "plus_24m", glyph: '★', hue: Hue::Magenta },
    HsBadge { id: "plus_12m", glyph: '★', hue: Hue::Yellow },
    HsBadge { id: "plus_6m", glyph: '◆', hue: Hue::Green },
    HsBadge { id: "plus_3m", glyph: '◆', hue: Hue::Cyan },
    HsBadge { id: "plus_1m", glyph: '◇', hue: Hue::Cyan },
    HsBadge { id: "ember", glyph: '▲', hue: Hue::Red },
    HsBadge { id: "early_supporter", glyph: '●', hue: Hue::Blue },
];

/// the catalogue entry for an id; None for an id this build doesn't know.
pub fn by_id(id: &str) -> Option<&'static HsBadge> {
    BADGES.iter().find(|b| b.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_ids_resolve_and_unknown_draw_nothing() {
        assert_eq!(by_id("ember").map(|b| b.glyph), Some('▲'));
        assert_eq!(by_id("plus_12m").map(|b| b.hue), Some(Hue::Yellow));
        assert!(by_id("none").is_none());
        assert!(by_id("").is_none());
        assert!(by_id("EMBER").is_none(), "ids are exact");
    }

    #[test]
    fn ids_are_unique() {
        for (i, a) in BADGES.iter().enumerate() {
            assert!(BADGES[i + 1..].iter().all(|b| b.id != a.id), "{}", a.id);
        }
    }
}
