//! Name paints.
//!
//! A paint is a gradient across a username, optionally scrolling. The obvious
//! implementation — draw each glyph as its own `Shape::text` at a computed x —
//! throws away kerning and shaping, which looks wrong on exactly the short
//! proportional strings this is used for.
//!
//! Instead one `LayoutJob` carries one section per character, each with its own
//! colour. egui lays the whole string out as a single galley, so spacing stays
//! correct, and the gradient is just the per-section colour sampled at that
//! character's position. Animation is re-sampling the same job each frame —
//! there is no texture and nothing to upload.
//!
//! Rule R2 in `docs/ecosystem-posture.md` keeps heatsync cosmetics off a
//! platform's native chat DOM. A native heatsync window is a heatsync surface,
//! so paints belong here.

use egui::{text::LayoutJob, Color32, FontId, TextFormat, Ui};

/// Repaint interval for an animating name paint — ~30/s.
///
/// A paint's gradient is continuous, so unlike a gif it has no natural frame
/// delay of its own; 30fps is the cap we choose for it.
///
/// It lives here rather than in the app loop because it is a property of how a
/// paint animates, and the view needs it to report what the visible rows
/// require.
pub const TICK_MS: u32 = 33;

#[derive(Clone, Debug)]
pub struct Paint {
    /// Two or more colour stops, evenly spaced across the name.
    pub stops: Vec<Color32>,
    /// Full gradient cycles per second. 0.0 is a still gradient.
    pub speed: f32,
}

impl Paint {
    pub fn still(stops: Vec<Color32>) -> Paint {
        Paint { stops, speed: 0.0 }
    }

    pub fn animated(stops: Vec<Color32>, speed: f32) -> Paint {
        Paint { stops, speed }
    }

    /// Colour at `t` in 0..=1 across the gradient, wrapping so an animated
    /// paint has no seam.
    pub fn sample(&self, t: f32) -> Color32 {
        if self.stops.is_empty() {
            return Color32::WHITE;
        }
        if self.stops.len() == 1 {
            return self.stops[0];
        }
        let t = t.rem_euclid(1.0);
        // wrap: the last stop blends back into the first
        let n = self.stops.len();
        let scaled = t * n as f32;
        let i = scaled.floor() as usize % n;
        let k = scaled - scaled.floor();
        lerp(self.stops[i], self.stops[(i + 1) % n], k)
    }
}

fn lerp(a: Color32, b: Color32, k: f32) -> Color32 {
    let k = k.clamp(0.0, 1.0);
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * k) as u8;
    Color32::from_rgba_unmultiplied(
        f(a.r(), b.r()),
        f(a.g(), b.g()),
        f(a.b(), b.b()),
        f(a.a(), b.a()),
    )
}

/// Build the per-character job for `name` under `paint` at `t_ms`.
pub fn job(name: &str, paint: &Paint, size: f32, t_ms: u64) -> LayoutJob {
    let mut job = LayoutJob::default();
    let chars: Vec<char> = name.chars().collect();
    let n = chars.len().max(1);
    let offset = if paint.speed == 0.0 {
        0.0
    } else {
        (t_ms as f32 / 1000.0) * paint.speed
    };
    for (i, c) in chars.iter().enumerate() {
        let t = i as f32 / n as f32 + offset;
        let mut buf = [0u8; 4];
        job.append(
            c.encode_utf8(&mut buf),
            0.0,
            TextFormat {
                font_id: FontId::proportional(size),
                color: paint.sample(t),
                ..Default::default()
            },
        );
    }
    job
}

pub fn username(ui: &mut Ui, name: &str, paint: &Paint, size: f32, t_ms: u64) {
    ui.label(job(name, paint, size, t_ms));
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: Color32 = Color32::from_rgb(255, 0, 0);
    const BLUE: Color32 = Color32::from_rgb(0, 0, 255);

    #[test]
    fn a_single_stop_is_a_flat_colour_everywhere() {
        let p = Paint::still(vec![RED]);
        assert_eq!(p.sample(0.0), RED);
        assert_eq!(p.sample(0.5), RED);
        assert_eq!(p.sample(0.99), RED);
    }

    #[test]
    fn an_empty_paint_degrades_to_white_rather_than_panicking() {
        assert_eq!(Paint::still(vec![]).sample(0.3), Color32::WHITE);
    }

    #[test]
    fn two_stops_blend_between_them() {
        let p = Paint::still(vec![RED, BLUE]);
        assert_eq!(p.sample(0.0), RED);
        let mid = p.sample(0.25);
        assert!(
            mid.r() > 0 && mid.b() > 0,
            "quarter way is a blend, got {mid:?}"
        );
    }

    #[test]
    fn the_gradient_wraps_so_an_animated_paint_has_no_seam() {
        let p = Paint::animated(vec![RED, BLUE], 1.0);
        // just before the wrap point we are heading back toward the first stop
        assert_eq!(p.sample(1.0), p.sample(0.0));
        assert_eq!(p.sample(2.5), p.sample(0.5));
        // negative offsets wrap too
        assert_eq!(p.sample(-1.0), p.sample(0.0));
    }

    #[test]
    fn a_still_paint_ignores_time() {
        let p = Paint::still(vec![RED, BLUE]);
        let a = job("mellen", &p, 14.0, 0);
        let b = job("mellen", &p, 14.0, 900_000);
        assert_eq!(a.sections[0].format.color, b.sections[0].format.color);
    }

    #[test]
    fn an_animated_paint_moves_with_time() {
        let p = Paint::animated(vec![RED, BLUE], 1.0);
        let a = job("mellen", &p, 14.0, 0);
        let b = job("mellen", &p, 14.0, 250);
        assert_ne!(
            a.sections[0].format.color, b.sections[0].format.color,
            "a quarter second in, the gradient should have moved"
        );
    }

    #[test]
    fn one_section_per_character_so_layout_is_still_one_galley() {
        let p = Paint::still(vec![RED, BLUE]);
        let j = job("abcd", &p, 14.0, 0);
        assert_eq!(j.sections.len(), 4);
        assert_eq!(j.text, "abcd");
    }

    #[test]
    fn multibyte_names_get_one_section_per_char_not_per_byte() {
        let p = Paint::still(vec![RED, BLUE]);
        let j = job("héllo→", &p, 14.0, 0);
        assert_eq!(j.sections.len(), 6, "6 chars, not {} bytes", "héllo→".len());
        assert_eq!(j.text, "héllo→");
    }

    #[test]
    fn an_empty_name_produces_an_empty_job_without_dividing_by_zero() {
        let p = Paint::still(vec![RED, BLUE]);
        let j = job("", &p, 14.0, 0);
        assert!(j.sections.is_empty());
    }
}
