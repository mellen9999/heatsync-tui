//! the one place colors live: the 8 ansi names (so the user's terminal palette
//! rules) plus vt320 attributes. nothing else in the tui names a color.
//! non-ansi colors allowed, exactly these and no others (a test pins the list):
//! the twitch marker, the read-tab gray, a future [H] marker, and the brand's
//! five dim row fills (olive, purple, navy, teal, dim red) — solid backgrounds
//! for notification rows, never text colors.

use heatsync_core::heat::{Hue, Look, Tier};
use heatsync_core::{Badge, BadgeKind, NoteKind, Platform, Tint, Unread};
use ratatui::style::{Color, Modifier, Style};

/// secondary text.
pub const DIM: Style = Style::new().fg(Color::White);
/// body text: chat messages, the input line.
pub const TEXT: Style = Style::new().fg(Color::White);
/// headers and key hints: bold.
pub const HEAD: Style = Style::new().fg(Color::White).add_modifier(Modifier::BOLD);
/// selection / mode tags / cursor block: inverse video.
pub const SEL: Style = Style::new().add_modifier(Modifier::REVERSED);
pub const TAG: Style = Style::new().add_modifier(Modifier::REVERSED.union(Modifier::BOLD));
/// status messages, errors, pending.
pub const WARN: Style = Style::new().fg(Color::Yellow);
pub const WARN_BOLD: Style = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);
/// blue is text-on-black only, never a background: the ping bar and the
/// empty heat-bar track.
pub const PING: Style = Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD);
pub const TRACK: Style = Style::new().fg(Color::Blue);
pub const LIVE: Style = Style::new().fg(Color::Green);

/// a tab with nothing unread: the extension's #808080 — the third allowed rgb.
pub const READ: Color = Color::Rgb(128, 128, 128);

/// reply context + `◇ first message`: the #808080 dim the thread line uses.
pub const DIM_TAG: Style = Style::new().fg(READ);
/// the `HH:MM` line prefix.
pub const STAMP: Style = Style::new().fg(READ);

/// a line a mod removed: gray, struck through; its `[deleted]` tag is gray only.
pub const GONE: Style = Style::new().fg(READ).add_modifier(Modifier::CROSSED_OUT);
pub const GONE_TAG: Style = Style::new().fg(READ);

/// a background tab by what it holds: read = gray, unread = white, a stream
/// event = yellow, a ping = bold red. the focused tab is `TAG` (reversed).
pub fn tab(u: Unread) -> Style {
    match u {
        Unread::None => Style::new().fg(READ),
        Unread::Chat => TEXT,
        Unread::Event => Style::new().fg(Color::Yellow),
        Unread::Ping => Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
    }
}

/// a notification row's solid fill: the dim twin of the row's meaning.
/// olive = stream events, purple = new people, navy = support, teal = channel
/// points + announcements, red = aimed at you.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fill {
    Olive,
    Purple,
    Navy,
    Teal,
    Red,
}

pub const FILL_OLIVE: Color = Color::Rgb(128, 128, 0);
pub const FILL_PURPLE: Color = Color::Rgb(128, 0, 128);
pub const FILL_NAVY: Color = Color::Rgb(0, 0, 128);
pub const FILL_TEAL: Color = Color::Rgb(0, 128, 128);
pub const FILL_RED: Color = Color::Rgb(95, 0, 0);

impl Fill {
    pub fn bg(self) -> Color {
        match self {
            Fill::Olive => FILL_OLIVE,
            Fill::Purple => FILL_PURPLE,
            Fill::Navy => FILL_NAVY,
            Fill::Teal => FILL_TEAL,
            Fill::Red => FILL_RED,
        }
    }
}

/// twitch's brand purple — a non-ansi color (see top).
pub const TWITCH: Color = Color::Rgb(200, 168, 255);

fn color(h: Hue) -> Color {
    match h {
        Hue::Black => Color::Black,
        Hue::Red => Color::Red,
        Hue::Green => Color::Green,
        Hue::Yellow => Color::Yellow,
        Hue::Blue => Color::Blue,
        Hue::Magenta => Color::Magenta,
        Hue::Cyan => Color::Cyan,
        Hue::White => Color::White,
    }
}

fn style(l: Look) -> Style {
    let mut s = Style::new().fg(color(l.hue));
    if l.bold {
        s = s.add_modifier(Modifier::BOLD);
    }
    if l.reversed {
        s = s.add_modifier(Modifier::REVERSED);
    }
    if l.blink {
        s = s.add_modifier(Modifier::SLOW_BLINK);
    }
    s
}

/// a tier's style on the heat ladder.
pub fn heat(t: Tier) -> Style {
    style(t.look())
}

/// the ladder for solid fills: reverse/blink on block glyphs would hollow them.
pub fn heat_fill(t: Tier) -> Style {
    let mut l = t.look();
    l.reversed = false;
    l.blink = false;
    style(l)
}

/// the merged-tab line marker.
pub fn platform(p: Platform) -> Style {
    Style::new().fg(match p {
        Platform::Twitch => TWITCH,
        Platform::Kick => Color::Green,
        Platform::Youtube => Color::Red,
    })
}

/// role badge: a filled one-cell glyph in the role's color.
pub fn badge(b: Badge) -> Style {
    use BadgeKind as K;
    let s = Style::new().add_modifier(Modifier::BOLD);
    match b.kind {
        K::Broadcaster => s.fg(Color::Black).bg(Color::Red),
        K::Moderator => s.fg(Color::Black).bg(Color::Green),
        K::Vip => s.fg(Color::Black).bg(Color::Magenta),
        K::Subscriber | K::Member => Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD),
        K::Founder => s.fg(Color::Black).bg(Color::Yellow),
        K::Staff | K::GlobalMod => s.fg(Color::Black).bg(Color::White),
        K::Verified => s.fg(Color::Black).bg(Color::Cyan),
        K::Og => s
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::UNDERLINED),
        K::Premium | K::Predictions | K::Hype => {
            Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD)
        }
        K::Bits | K::Artist => Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        K::Gifter | K::Turbo => Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        K::Other(_) => Style::new().add_modifier(Modifier::BOLD),
    }
}

/// a picked heatsync badge: its glyph in its ansi hue.
pub fn hs_badge(b: &heatsync_core::hsbadge::HsBadge) -> Style {
    Style::new().fg(color(b.hue))
}

/// glyph + style per event kind. green=live, red=mod/danger, yellow=money+hype,
/// magenta=social, cyan=info, white=gone.
pub fn note(k: NoteKind) -> (&'static str, Style) {
    use NoteKind as K;
    let s = Style::new();
    match k {
        K::Sub => ("★", s.fg(Color::Yellow)),
        K::Gift => ("✦", s.fg(Color::Magenta)),
        K::Cheer => ("◆", s.fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        K::Raid => ("⚑", s.fg(Color::Magenta).add_modifier(Modifier::BOLD)),
        K::Redeem => ("◇", s.fg(Color::Cyan)),
        K::Live => ("●", s.fg(Color::Green)),
        K::Offline => ("○", s.fg(Color::White)),
        K::Category => ("→", s.fg(Color::Cyan)),
        K::Notice => ("»", s.fg(Color::Cyan)),
        K::Spike => ("▲", s.fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        K::Mod => ("×", s.fg(Color::Red)),
        K::Announce(t) => (
            "!",
            match t {
                Tint::Plain => s.add_modifier(Modifier::BOLD),
                Tint::Blue => s.fg(Color::Blue),
                Tint::Green => s.fg(Color::Green),
                Tint::Yellow => s.fg(Color::Yellow),
                Tint::Magenta => s.fg(Color::Magenta),
            },
        ),
        K::Streak => ("≈", s.fg(Color::Yellow)),
        K::Milestone => ("♦", s.fg(Color::Magenta)),
        K::Charity => ("♥", s.fg(Color::Magenta)),
        K::Ritual => ("✧", s.fg(Color::Green)),
        K::Mode => ("◈", s.fg(Color::Cyan)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sanctioned_rgb_set_is_exactly_this() {
        // the doctrine: ansi 0-7 plus these. adding a colour means editing this
        // list on purpose — and the count below catches one slipping in unlisted.
        let allowed = [
            (128, 128, 128), // read gray
            (200, 168, 255), // twitch
            (128, 128, 0),   // olive
            (128, 0, 128),   // purple
            (0, 0, 128),     // navy
            (0, 128, 128),   // teal
            (95, 0, 0),      // dim red
        ];
        for c in [
            READ,
            TWITCH,
            FILL_OLIVE,
            FILL_PURPLE,
            FILL_NAVY,
            FILL_TEAL,
            FILL_RED,
        ] {
            let Color::Rgb(r, g, b) = c else {
                panic!("{c:?}")
            };
            assert!(allowed.contains(&(r, g, b)), "{c:?}");
        }
        let src = include_str!("palette.rs");
        let code = &src[..src.find("#[cfg(test)]").unwrap()];
        assert_eq!(code.matches("Color::Rgb(").count(), allowed.len());
        for f in [Fill::Olive, Fill::Purple, Fill::Navy, Fill::Teal, Fill::Red] {
            assert!(matches!(f.bg(), Color::Rgb(..)));
        }
    }

    #[test]
    fn gone_is_gray_and_crossed_out_but_its_tag_is_not() {
        assert_eq!(GONE.fg, Some(READ));
        assert!(GONE.add_modifier.contains(Modifier::CROSSED_OUT));
        assert!(!GONE_TAG.add_modifier.contains(Modifier::CROSSED_OUT));
    }

    #[test]
    fn tab_states_read_gray_unread_white_ping_bold_red() {
        assert_eq!(tab(Unread::None).fg, Some(READ));
        assert_eq!(tab(Unread::Chat), TEXT);
        assert_eq!(tab(Unread::Event).fg, Some(Color::Yellow));
        let p = tab(Unread::Ping);
        assert_eq!(p.fg, Some(Color::Red));
        assert!(p.add_modifier.contains(Modifier::BOLD));
    }
}
