//! the one place colors live: the 8 ansi colors as exact hex, plus the heat
//! ladder. nothing else in the gui names a color.

use egui::{Color32, RichText};
use heatsync_core::heat::{self, Hue, Look};

pub const BLACK: Color32 = Color32::from_rgb(0x00, 0x00, 0x00);
pub const RED: Color32 = Color32::from_rgb(0xff, 0x00, 0x00);
pub const GREEN: Color32 = Color32::from_rgb(0x00, 0xff, 0x00);
pub const YELLOW: Color32 = Color32::from_rgb(0xff, 0xff, 0x00);
pub const BLUE: Color32 = Color32::from_rgb(0x00, 0x00, 0x80);
pub const MAGENTA: Color32 = Color32::from_rgb(0xff, 0x00, 0xff);
pub const CYAN: Color32 = Color32::from_rgb(0x00, 0xff, 0xff);
pub const WHITE: Color32 = Color32::from_rgb(0xff, 0xff, 0xff);

/// secondary text (message body); primary text is `.strong()` white.
pub const TEXT: Color32 = WHITE;
/// an emote that has not loaded yet.
pub const PENDING: Color32 = WHITE;

pub fn hue(h: Hue) -> Color32 {
    match h {
        Hue::Black => BLACK,
        Hue::Red => RED,
        Hue::Green => GREEN,
        Hue::Yellow => YELLOW,
        Hue::Blue => BLUE,
        Hue::Magenta => MAGENTA,
        Hue::Cyan => CYAN,
        Hue::White => WHITE,
    }
}

/// dress `t` in a heat look: bold → strong, reversed → fill the cell with the
/// hue and print black on it. (no blink in egui: it would force a repaint loop.)
pub fn dress(t: RichText, l: Look) -> RichText {
    let t = if l.bold { t.strong() } else { t };
    if l.reversed {
        t.color(BLACK).background_color(hue(l.hue))
    } else {
        t.color(hue(l.hue))
    }
}

pub fn heat_look(h: f64) -> Look {
    heat::look(h)
}
