//! The message list.
//!
//! Two things here are the whole reason this prototype exists, because if
//! either is bad in an immediate-mode toolkit the framework choice is wrong:
//!
//! 1. **Emotes inline in wrapping text.** egui wraps *between* items in a
//!    `horizontal_wrapped`, never inside one, so a message is emitted as one
//!    item per word and one per emote stack. Wrapping then falls out for free
//!    and an emote breaks lines exactly like a word does.
//!
//! 2. **Virtualising rows that are not a uniform height.** A wrapped message
//!    has no known height until it is laid out at the current width, so
//!    `ScrollArea::show_rows` (uniform only) cannot be used. Instead heights
//!    are measured as rows are drawn, cached, and kept as a running prefix sum.
//!    Rows never drawn are estimated from the running mean of those that have
//!    been — see [`Heights`] for why a fixed estimate is not good enough. The
//!    cache is dropped whenever the wrap width changes, because every height in
//!    it was measured against the old width.

use egui::{Align, Color32, FontId, Layout, Rect, RichText, ScrollArea, Ui, Vec2};
use heatsync_core::emote::{EmoteSet, Segment};

use crate::emote::Cache;
use crate::paint::{self, Paint};

pub struct Message {
    pub user: String,
    pub paint: Option<Paint>,
    pub segments: Vec<Segment>,
    pub heat: f64,
}

impl Message {
    pub fn parse(
        user: &str,
        paint: Option<Paint>,
        text: &str,
        set: &EmoteSet,
        heat: f64,
    ) -> Message {
        Message {
            user: user.to_string(),
            paint,
            segments: heatsync_core::emote::segments(text, set),
            heat,
        }
    }
}

/// Height cache + the width it was measured at.
///
/// A wrapped message has no height until it is laid out, and only the rows on
/// screen ever get laid out — with a ten-thousand message backlog the vast
/// majority are never measured at all. So every row that has not been measured
/// has to be *estimated*, and the quality of that estimate is the quality of
/// the scrollbar.
///
/// A fixed estimate is not good enough: it makes the total height independent
/// of the wrap width, so a narrow window and a wide one claim the same content
/// height even though a narrow one wraps every message taller. Instead the
/// estimate is **the running mean of the rows actually measured at this
/// width**, which converges on the truth as the user scrolls and is already
/// correct on the first frame after a screenful has been laid out.
#[derive(Default)]
pub struct Heights {
    width: f32,
    /// `NaN` marks a row that has never been laid out at this width.
    rows: Vec<f32>,
    measured_sum: f32,
    measured_count: usize,
    /// running prefix sum; `sum[i]` is the top of row `i`
    sum: Vec<f32>,
    dirty: bool,
}

impl Heights {
    /// Only used before anything at all has been measured — one line of text.
    const SEED_ESTIMATE: f32 = 20.0;

    /// Every cached height was measured against one wrap width. If the window
    /// resized they are all wrong, so the cache is dropped rather than
    /// gradually corrected — a stale prefix sum makes the scrollbar lie.
    pub fn set_width(&mut self, w: f32) {
        if (self.width - w).abs() > 0.5 {
            self.width = w;
            let n = self.rows.len();
            self.rows.clear();
            self.rows.resize(n, f32::NAN);
            self.measured_sum = 0.0;
            self.measured_count = 0;
            self.sum.clear();
            self.dirty = true;
        }
    }

    pub fn ensure(&mut self, n: usize) {
        if self.rows.len() < n {
            self.rows.resize(n, f32::NAN);
            self.dirty = true;
        }
    }

    /// Height to assume for a row nothing has laid out yet.
    pub fn estimate(&self) -> f32 {
        if self.measured_count == 0 {
            Self::SEED_ESTIMATE
        } else {
            self.measured_sum / self.measured_count as f32
        }
    }

    /// What this row is worth right now: its measurement, or the estimate.
    pub fn height_of(&self, i: usize) -> f32 {
        match self.rows.get(i) {
            Some(h) if h.is_finite() => *h,
            _ => self.estimate(),
        }
    }

    pub fn record(&mut self, i: usize, h: f32) {
        let Some(slot) = self.rows.get_mut(i) else {
            return;
        };
        let was = *slot;
        if was.is_finite() {
            if (was - h).abs() <= 0.5 {
                return; // unchanged
            }
            self.measured_sum += h - was;
        } else {
            self.measured_sum += h;
            self.measured_count += 1;
        }
        *slot = h;
        self.dirty = true;
    }

    fn rebuild(&mut self) {
        let est = self.estimate();
        self.sum.clear();
        self.sum.reserve(self.rows.len() + 1);
        let mut acc = 0.0;
        self.sum.push(0.0);
        for h in &self.rows {
            acc += if h.is_finite() { *h } else { est };
            self.sum.push(acc);
        }
        self.dirty = false;
    }

    pub fn total(&mut self) -> f32 {
        if self.dirty {
            self.rebuild();
        }
        *self.sum.last().unwrap_or(&0.0)
    }

    /// First row whose bottom is below `y` — binary search over the prefix sum.
    pub fn row_at(&mut self, y: f32) -> usize {
        if self.dirty {
            self.rebuild();
        }
        match self.sum.binary_search_by(|p| p.partial_cmp(&y).unwrap()) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        }
    }

    pub fn top_of(&mut self, i: usize) -> f32 {
        if self.dirty {
            self.rebuild();
        }
        *self.sum.get(i).unwrap_or(&0.0)
    }
}

pub struct View {
    pub heights: Heights,
    pub emote_px: f32,
    pub font: f32,
    /// rows actually emitted last frame — the virtualisation proof
    pub drawn_last_frame: usize,
}

impl Default for View {
    fn default() -> Self {
        Self {
            heights: Heights::default(),
            emote_px: 28.0,
            font: 14.0,
            drawn_last_frame: 0,
        }
    }
}

impl View {
    pub fn show(&mut self, ui: &mut Ui, msgs: &[Message], cache: &Cache, t_ms: u64) {
        let avail = ui.available_width();
        self.heights.set_width(avail);
        self.heights.ensure(msgs.len());

        let mut drawn = 0;
        ScrollArea::vertical()
            .auto_shrink([false; 2])
            .stick_to_bottom(true)
            .show_viewport(ui, |ui, viewport| {
                let total = self.heights.total();
                ui.set_height(total);

                let first = self.heights.row_at(viewport.min.y);
                let last = self
                    .heights
                    .row_at(viewport.max.y)
                    .min(msgs.len().saturating_sub(1));

                let top = self.heights.top_of(first);
                let top_left = ui.min_rect().min + Vec2::new(0.0, top);

                let mut y = top_left.y;
                for i in first..=last.min(msgs.len().saturating_sub(1)) {
                    if msgs.is_empty() {
                        break;
                    }
                    let row_rect = Rect::from_min_size(
                        egui::pos2(ui.min_rect().min.x, y),
                        Vec2::new(avail, self.heights.height_of(i)),
                    );
                    let h = ui
                        .scope_builder(
                            egui::UiBuilder::new()
                                .max_rect(row_rect)
                                .layout(Layout::top_down(Align::LEFT)),
                            |ui| self.message(ui, &msgs[i], cache, t_ms),
                        )
                        .inner;
                    self.heights.record(i, h);
                    y += h;
                    drawn += 1;
                }
            });
        self.drawn_last_frame = drawn;
    }

    /// One message. Returns its measured height.
    fn message(&self, ui: &mut Ui, m: &Message, cache: &Cache, t_ms: u64) -> f32 {
        let start = ui.cursor().min.y;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;

            // heat marker — a graded number, never an emoji (project rule)
            ui.label(
                RichText::new(format!("{:>3}", m.heat.round() as i64))
                    .font(FontId::monospace(self.font))
                    .color(heat_color(m.heat)),
            );

            match &m.paint {
                Some(p) => paint::username(ui, &m.user, p, self.font, t_ms),
                None => {
                    ui.label(
                        RichText::new(&m.user)
                            .font(FontId::proportional(self.font))
                            .strong()
                            .color(Color32::from_rgb(0xff, 0x87, 0x00)),
                    );
                }
            }

            for seg in &m.segments {
                match seg {
                    // One item per word: egui wraps between items, so this is
                    // what makes a long message wrap at all.
                    Segment::Text(t) => {
                        for w in t.split_whitespace() {
                            ui.label(
                                RichText::new(w)
                                    .font(FontId::proportional(self.font))
                                    .color(Color32::from_gray(0xdd)),
                            );
                        }
                    }
                    Segment::Stack(s) => self.stack(ui, s, cache, t_ms),
                }
            }
        });
        (ui.cursor().min.y - start).max(1.0)
    }

    /// An emote stack: base plus any zero-width layers painted over the same
    /// box, which is how `GAMBA notL` renders as one glyph rather than two.
    fn stack(&self, ui: &mut Ui, s: &heatsync_core::emote::Stack, cache: &Cache, t_ms: u64) {
        let key = s.key();
        let Some(anim) = cache.get(&key) else {
            // Not loaded yet — the emote's name is the honest fallback, and it
            // occupies roughly the right space so the line does not jump.
            ui.label(
                RichText::new(&s.base)
                    .font(FontId::proportional(self.font))
                    .color(Color32::from_gray(0x88)),
            );
            return;
        };

        let h = self.emote_px;
        let w = (h * anim.aspect) * if anim.wide { 2.0 } else { 1.0 };
        let (rect, _) = ui.allocate_exact_size(Vec2::new(w, h), egui::Sense::hover());

        for layer in 0..anim.layers.len() {
            if let Some(tex) = anim.frame_at(layer, t_ms) {
                egui::Image::new((tex.id(), rect.size())).paint_at(ui, rect);
            }
        }
    }
}

/// Heat as a graded colour ramp — cold grey through the brand orange to white
/// hot. Deliberately a colour on a number, not an emoji.
pub fn heat_color(heat: f64) -> Color32 {
    let t = (heat / 20.0).clamp(0.0, 1.0) as f32;
    if t < 0.5 {
        let k = t * 2.0;
        Color32::from_rgb(
            (0x60 as f32 + k * (0xff - 0x60) as f32) as u8,
            (0x60 as f32 + k * (0x87 - 0x60) as f32) as u8,
            (0x60 as f32 * (1.0 - k)) as u8,
        )
    } else {
        let k = (t - 0.5) * 2.0;
        Color32::from_rgb(
            0xff,
            (0x87 as f32 + k * (0xff - 0x87) as f32) as u8,
            (k * 0xff as f32) as u8,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_width_change_drops_every_cached_height() {
        let mut h = Heights::default();
        h.set_width(400.0);
        h.ensure(3);
        h.record(0, 55.0);
        assert_eq!(h.height_of(0), 55.0);
        h.set_width(700.0);
        assert_eq!(
            h.measured_count, 0,
            "heights measured at 400px cannot be reused at 700px"
        );
    }

    #[test]
    fn a_sub_pixel_width_wobble_does_not_drop_the_cache() {
        let mut h = Heights::default();
        h.set_width(400.0);
        h.ensure(2);
        h.record(0, 44.0);
        h.set_width(400.2);
        assert_eq!(
            h.height_of(0),
            44.0,
            "a 0.2px jitter must not invalidate everything"
        );
    }

    #[test]
    fn unmeasured_rows_start_at_the_estimate() {
        let mut h = Heights::default();
        h.set_width(500.0);
        h.ensure(4);
        assert_eq!(h.total(), Heights::SEED_ESTIMATE * 4.0);
    }

    #[test]
    fn total_follows_measurements_as_they_arrive() {
        let mut h = Heights::default();
        h.set_width(500.0);
        h.ensure(3);
        h.record(1, 60.0);
        // one row measured at 60 -> the two unmeasured rows now estimate 60 too
        assert_eq!(h.total(), 180.0);
    }

    #[test]
    fn an_unmeasured_row_borrows_the_mean_of_the_measured_ones() {
        let mut h = Heights::default();
        h.set_width(500.0);
        h.ensure(10);
        assert_eq!(h.estimate(), Heights::SEED_ESTIMATE, "nothing measured yet");
        h.record(0, 40.0);
        h.record(1, 60.0);
        assert_eq!(h.estimate(), 50.0);
        assert_eq!(h.height_of(9), 50.0, "an undrawn row uses the mean");
        assert_eq!(h.height_of(0), 40.0, "a drawn row keeps its measurement");
    }

    #[test]
    fn the_estimate_tracks_a_row_being_re_measured() {
        let mut h = Heights::default();
        h.set_width(500.0);
        h.ensure(4);
        h.record(0, 40.0);
        assert_eq!(h.estimate(), 40.0);
        // same row, new height (content changed) — mean must follow, not double-count
        h.record(0, 80.0);
        assert_eq!(h.estimate(), 80.0);
        assert_eq!(h.measured_count, 1);
    }

    #[test]
    fn total_height_responds_to_width_because_the_estimate_does() {
        // The bug an e2e test caught: with a fixed estimate a narrow window and
        // a wide one claimed the same content height, so the same number of
        // rows fit either way and the scrollbar lied on a long backlog.
        let mut wide = Heights::default();
        wide.set_width(1200.0);
        wide.ensure(1000);
        wide.record(0, 20.0);

        let mut narrow = Heights::default();
        narrow.set_width(320.0);
        narrow.ensure(1000);
        narrow.record(0, 60.0); // same message wraps three lines when narrow

        assert!(
            narrow.total() > wide.total(),
            "a narrow window holds taller rows, so its content must be taller: \
             narrow {} vs wide {}",
            narrow.total(),
            wide.total()
        );
    }

    #[test]
    fn a_width_change_forgets_the_measurements_but_keeps_the_row_count() {
        let mut h = Heights::default();
        h.set_width(400.0);
        h.ensure(5);
        h.record(0, 90.0);
        assert_eq!(h.measured_count, 1);
        h.set_width(900.0);
        assert_eq!(h.measured_count, 0, "measurements are width-specific");
        assert_eq!(h.estimate(), Heights::SEED_ESTIMATE);
        assert_eq!(h.total(), Heights::SEED_ESTIMATE * 5.0, "still five rows");
    }

    #[test]
    fn row_at_finds_the_row_containing_a_scroll_offset() {
        let mut h = Heights::default();
        h.set_width(500.0);
        h.ensure(4);
        for i in 0..4 {
            h.record(i, 10.0);
        }
        // rows occupy 0-10, 10-20, 20-30, 30-40
        assert_eq!(h.row_at(0.0), 0);
        assert_eq!(h.row_at(5.0), 0);
        assert_eq!(h.row_at(15.0), 1);
        assert_eq!(h.row_at(35.0), 3);
    }

    #[test]
    fn top_of_is_the_sum_of_everything_above() {
        let mut h = Heights::default();
        h.set_width(500.0);
        h.ensure(3);
        h.record(0, 12.0);
        h.record(1, 30.0);
        assert_eq!(h.top_of(0), 0.0);
        assert_eq!(h.top_of(1), 12.0);
        assert_eq!(h.top_of(2), 42.0);
    }

    #[test]
    fn heat_colour_is_a_ramp_not_a_step() {
        let cold = heat_color(0.0);
        let mid = heat_color(10.0);
        let hot = heat_color(20.0);
        assert_ne!(cold, mid);
        assert_ne!(mid, hot);
        assert_eq!(hot, Color32::from_rgb(0xff, 0xff, 0xff));
        // clamps rather than wrapping past white
        assert_eq!(heat_color(1000.0), hot);
    }
}
