//! Headless frame-cost measurement.
//!
//! "Fast and clean" is the entire reason this client exists, so it gets a
//! number rather than an assertion — and a number that can run in CI, with no
//! window, no compositor and no gpu.
//!
//! `Context::run_ui` drives a complete egui pass: layout, wrapping, and the
//! per-word widget emission a chat message costs. Tessellation is timed
//! separately because it is the part that scales with how much is actually on
//! screen. What is NOT measured here is gpu upload and draw, which is why this
//! is a floor on frame cost, not the whole of it.

use std::time::Instant;

use egui::{Context, RawInput, Rect, Vec2};

use crate::chat::{Message, View};
use crate::emote::Cache;

pub struct Report {
    pub frames: u32,
    pub layout_ms: f64,
    pub tessellate_ms: f64,
    pub drawn: usize,
    pub shapes: usize,
}

impl Report {
    pub fn total_ms(&self) -> f64 {
        self.layout_ms + self.tessellate_ms
    }

    pub fn print(&self, label: &str, msgs: usize) {
        eprintln!(
            "[bench] {label:<22} {:>7.3} ms/frame over {:>3} frames  (layout {:>6.3} + tessellate {:>6.3})  drew {:>3} of {}  {} verts",
            self.total_ms(),
            self.frames,
            self.layout_ms,
            self.tessellate_ms,
            self.drawn,
            msgs,
            self.shapes,
        );
    }
}

/// Run `frames` passes at a fixed viewport, scrolling by `scroll_per_frame`
/// pixels each pass so the measurement covers real work rather than a
/// perfectly cached still frame.
pub fn run(
    msgs: &[Message],
    load_emotes: &dyn Fn(&Context, &mut Cache),
    frames: u32,
    size: Vec2,
    scroll_per_frame: f32,
) -> Report {
    let ctx = Context::default();
    // Textures are registered against the context, which needs no gpu to do —
    // so the emote path measured here is the real one, not the text fallback.
    let mut owned = Cache::default();
    load_emotes(&ctx, &mut owned);
    let cache = &owned;
    let mut view = View::default();

    let mut layout = 0.0f64;
    let mut tess = 0.0f64;
    let mut shapes = 0usize;

    // One warm-up pass so font atlas build and first-time height measurement
    // do not land in the timings.
    let warm = RawInput {
        screen_rect: Some(Rect::from_min_size(Default::default(), size)),
        ..Default::default()
    };
    let _ = ctx.run_ui(warm, |ui| {
        view.show(ui, msgs, cache, 0);
    });

    for f in 0..frames {
        let t_ms = f as u64 * 16;
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(Default::default(), size)),
            events: if scroll_per_frame != 0.0 {
                vec![egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: Vec2::new(0.0, -scroll_per_frame),
                    phase: egui::TouchPhase::Move,
                    modifiers: Default::default(),
                }]
            } else {
                vec![]
            },
            ..Default::default()
        };

        let t0 = Instant::now();
        let out = ctx.run_ui(input, |ui| {
            view.show(ui, msgs, cache, t_ms);
        });
        layout += t0.elapsed().as_secs_f64() * 1000.0;

        let t1 = Instant::now();
        let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
        tess += t1.elapsed().as_secs_f64() * 1000.0;
        // Vertices, not primitive count — egui batches everything sharing a
        // clip rect and texture into one primitive, so `prims.len()` is nearly
        // always 1 and says nothing about how much was drawn.
        shapes = prims
            .iter()
            .map(|p| match &p.primitive {
                egui::epaint::Primitive::Mesh(m) => m.vertices.len(),
                _ => 0,
            })
            .sum();
    }

    Report {
        frames,
        layout_ms: layout / frames as f64,
        tessellate_ms: tess / frames as f64,
        drawn: view.drawn_last_frame,
        shapes,
    }
}
