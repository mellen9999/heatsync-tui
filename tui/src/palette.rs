//! the one place colors live: the 8 ansi names (so the user's terminal palette
//! rules) plus vt320 attributes. nothing else in the tui names a color.

use heatsync_core::heat::{Hue, Look, Tier};
use heatsync_core::{Badge, NoteKind, Platform};
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
/// fill behind a line that pings you, and the empty heat-bar track.
pub const SLAB: Style = Style::new().bg(Color::Blue);
pub const TRACK: Style = Style::new().fg(Color::Blue);
pub const LIVE: Style = Style::new().fg(Color::Green);

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
        Platform::Twitch => Color::Magenta,
        Platform::Kick => Color::Green,
        Platform::Youtube => Color::Red,
    })
}

/// role badge: a filled one-cell glyph in the role's color.
pub fn badge(b: Badge) -> Style {
    let s = Style::new().add_modifier(Modifier::BOLD);
    match b {
        Badge::Broadcaster => s.fg(Color::Black).bg(Color::Red),
        Badge::Moderator => s.fg(Color::Black).bg(Color::Green),
        Badge::Vip => s.fg(Color::Black).bg(Color::Magenta),
        Badge::Subscriber => s.fg(Color::White).bg(Color::Blue),
        Badge::Founder => s.fg(Color::Black).bg(Color::Yellow),
        Badge::Staff => s.fg(Color::Black).bg(Color::White),
        Badge::Verified => s.fg(Color::Black).bg(Color::Cyan),
        Badge::Og => s.fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::UNDERLINED),
    }
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
    }
}
