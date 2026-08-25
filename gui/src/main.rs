//! heatsync desktop — prototype.
//!
//! This exists to answer one question before any more of it gets built: can an
//! immediate-mode toolkit carry a chat client whose text is full of animated
//! inline emotes, with paints on names and a scrollback long enough to matter?
//!
//! It renders a synthetic firehose rather than talking to the network, because
//! the question is about layout and frame cost, not about io. Real messages
//! arrive through the same `heatsync_core::emote::segments` call.
//!
//! Run with `--stats` to print a frame-cost line every second, which is how the
//! claim in the plan ("fast and clean" gets a number, not an assertion") is
//! actually measured.

mod bench;
mod chat;
mod emote;
mod paint;

use std::time::{Duration, Instant};

use egui::Color32;
use heatsync_core::emote::{Emote, EmoteSet};

use chat::{Message, View};
use paint::Paint;

const MSGS: usize = 10_000;

fn emote_set() -> EmoteSet {
    let mk = |name: &str, zero_width: bool, animated: bool| Emote {
        name: name.to_string(),
        url: format!("synthetic://{name}"),
        provider: "7tv".to_string(),
        id: name.to_string(),
        animated,
        zero_width,
    };
    EmoteSet::from_list([
        mk("KEKW", false, true),
        mk("PogU", false, false),
        mk("GAMBA", false, true),
        mk("catJAM", false, true),
        // zero-width: overlays the emote before it rather than taking a slot
        mk("notL", true, false),
        mk("RainTime", true, true),
    ])
}

fn corpus() -> Vec<&'static str> {
    vec![
        "this is a plain line with no emotes at all just words",
        "KEKW that was actually good",
        "GAMBA notL he is never winning that back",
        "w!KEKW wide one here",
        "chat is moving so fast right now catJAM catJAM catJAM",
        "PogU RainTime stacked modifiers on a long enough line that it has to wrap somewhere around here",
        "a much longer message that definitely wraps across several lines because it keeps going and going and mixes KEKW emotes into the middle of the flowing text to prove the wrap breaks at an emote exactly like it breaks at a word",
        "short",
        "z!catJAM forced overlay",
    ]
}

fn paints() -> Vec<Option<Paint>> {
    vec![
        None,
        None,
        Some(Paint::still(vec![
            Color32::from_rgb(0xff, 0x87, 0x00),
            Color32::from_rgb(0xff, 0xd7, 0x00),
        ])),
        Some(Paint::animated(
            vec![
                Color32::from_rgb(0xff, 0x00, 0x5e),
                Color32::from_rgb(0x7d, 0x00, 0xff),
                Color32::from_rgb(0x00, 0xd9, 0xff),
            ],
            0.35,
        )),
    ]
}

fn build(set: &EmoteSet) -> Vec<Message> {
    let corpus = corpus();
    let paints = paints();
    (0..MSGS)
        .map(|i| {
            Message::parse(
                &format!("user{:04}", i % 137),
                paints[i % paints.len()].clone(),
                corpus[i % corpus.len()],
                set,
                ((i * 7) % 21) as f64,
            )
        })
        .collect()
}

struct App {
    msgs: Vec<Message>,
    cache: emote::Cache,
    view: View,
    started: Instant,
    loaded: bool,
    stats: bool,
    frames: u32,
    last_report: Instant,
    frame_ms: f32,
}

impl App {
    fn new(stats: bool) -> App {
        let set = emote_set();
        App {
            msgs: build(&set),
            cache: emote::Cache::default(),
            view: View::default(),
            started: Instant::now(),
            loaded: false,
            stats,
            frames: 0,
            last_report: Instant::now(),
            frame_ms: 0.0,
        }
    }

    /// Upload the emote textures once, on the first frame that has a context
    /// to upload into.
    fn load(&mut self, ctx: &egui::Context) {
        upload_emotes(ctx, &mut self.cache);
        self.loaded = true;
    }
}

/// Every distinct stack key the corpus can produce, uploaded once. Shared with
/// the bench so the measured path is the one the app actually runs — a bench
/// against an empty cache silently measures the text fallback instead.
fn upload_emotes(ctx: &egui::Context, cache: &mut emote::Cache) {
    let set = emote_set();
    let mut keys: Vec<(String, bool)> = Vec::new();
    for line in corpus() {
        for seg in heatsync_core::emote::segments(line, &set) {
            if let heatsync_core::emote::Segment::Stack(s) = seg {
                let k = s.key();
                if !keys.iter().any(|(e, _)| e == &k) {
                    keys.push((k, s.wide()));
                }
            }
        }
    }
    for (i, (key, wide)) in keys.into_iter().enumerate() {
        // layer count = how many urls the stack carries (base + overlays)
        let layers = key
            .split('\n')
            .filter(|s| !s.starts_with('#'))
            .count()
            .max(1);
        let frames: Vec<_> = (0..layers)
            .map(|l| emote::synthetic((i * 3 + l) as u32, if l == 0 { 8 } else { 4 }, 32))
            .collect();
        cache.insert(ctx, &key, wide, frames);
    }
}

impl eframe::App for App {
    // egui 0.36 hands the app a Ui rather than a Context, and panels are shown
    // inside it. The Context is still reachable through `ui.ctx()`.
    fn ui(&mut self, ui: &mut egui::Ui, _f: &mut eframe::Frame) {
        let t0 = Instant::now();
        let ctx = ui.ctx().clone();
        if !self.loaded {
            self.load(&ctx);
        }
        let t_ms = self.started.elapsed().as_millis() as u64;

        egui::Panel::top("hud").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("heatsync")
                        .strong()
                        .color(Color32::from_rgb(0xff, 0x87, 0x00)),
                );
                ui.separator();
                ui.label(format!("{} msgs", self.msgs.len()));
                ui.separator();
                ui.label(format!("drawn {}", self.view.drawn_last_frame));
                ui.separator();
                ui.label(format!("{} stacks", self.cache.len()));
                ui.separator();
                ui.label(format!("{:.2} ms/frame", self.frame_ms));
            });
        });

        egui::CentralPanel::default().show(ui, |ui| {
            self.view.show(ui, &self.msgs, &self.cache, t_ms);
        });

        // Animated emotes and animated paints both need a clock, but only the
        // ones actually on screen. Ask for the soonest repaint either needs
        // rather than spinning at a fixed rate — a channel whose emotes are all
        // static, with no animated paint in view, then costs no frames at all.
        let animating_paint = self
            .msgs
            .iter()
            .any(|m| m.paint.as_ref().is_some_and(|p| p.speed != 0.0));
        let tick = match (self.cache.tick_ms(), animating_paint) {
            (Some(t), true) => Some(t.min(33)),
            (Some(t), false) => Some(t),
            (None, true) => Some(33),
            (None, false) => None,
        };
        if let Some(ms) = tick {
            ctx.request_repaint_after(Duration::from_millis(ms as u64));
        }

        let ms = t0.elapsed().as_secs_f32() * 1000.0;
        self.frame_ms = self.frame_ms * 0.9 + ms * 0.1;
        self.frames += 1;
        if self.stats && self.last_report.elapsed() >= Duration::from_secs(1) {
            eprintln!(
                "[stats] fps={} frame_ms={:.2} drawn={} of {} stacks={}",
                self.frames,
                self.frame_ms,
                self.view.drawn_last_frame,
                self.msgs.len(),
                self.cache.len()
            );
            self.frames = 0;
            self.last_report = Instant::now();
        }
    }
}

/// Headless: measure the cost of a frame and print it, then exit. No window,
/// no gpu — runnable in CI.
fn bench_main() {
    let set = emote_set();
    let msgs = build(&set);
    let size = egui::vec2(980.0, 720.0);
    let load = |ctx: &egui::Context, cache: &mut emote::Cache| upload_emotes(ctx, cache);

    eprintln!(
        "[bench] {} messages, {}x{} viewport",
        msgs.len(),
        size.x,
        size.y
    );
    bench::run(&msgs, &load, 120, size, 0.0).print("still", msgs.len());
    bench::run(&msgs, &load, 120, size, 40.0).print("scrolling 40px/frame", msgs.len());
    bench::run(&msgs, &load, 120, size, 400.0).print("flinging 400px/frame", msgs.len());

    // What a 10k backlog costs versus a small one, to show virtualisation is
    // actually doing something rather than the list simply being cheap.
    let small: Vec<_> = build(&set).into_iter().take(200).collect();
    bench::run(&small, &load, 120, size, 40.0).print("200 msgs, scrolling", small.len());

    // And with no emote textures at all, so the emote path's share is visible.
    let none = |_: &egui::Context, _: &mut emote::Cache| {};
    bench::run(&msgs, &none, 120, size, 40.0).print("no emotes (text only)", msgs.len());
}

fn main() -> eframe::Result<()> {
    if std::env::args().any(|a| a == "--bench") {
        bench_main();
        return Ok(());
    }
    let stats = std::env::args().any(|a| a == "--stats");
    eframe::run_native(
        "heatsync",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default().with_inner_size([980.0, 720.0]),
            ..Default::default()
        },
        Box::new(move |_cc| Ok(Box::new(App::new(stats)))),
    )
}
