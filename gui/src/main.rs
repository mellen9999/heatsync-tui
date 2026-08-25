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
mod cadence;
mod chat;
mod e2e;
mod emote;
mod paint;

use std::time::{Duration, Instant};

use egui::Color32;
use heatsync_core::emote::{Emote, EmoteSet};

use chat::{Message, View};
use paint::Paint;

const MSGS: usize = 10_000;

/// Enough frames for the height cache to settle and a second animation frame to
/// land, so the smoke test proves a steady state rather than one lucky paint.
const SMOKE_FRAMES: u32 = 30;

/// A smoke run that cannot finish must fail, not hang.
///
/// This catches the case where eframe still drives the app but never paints.
/// It does NOT catch a window that never maps at all — observed under a
/// tag-based compositor, where eframe stops calling the app entirely and no
/// in-process guard can fire. CI therefore wraps this in an external timeout,
/// which is the guard that actually cannot be evaded.
const SMOKE_DEADLINE: Duration = Duration::from_secs(30);

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
    /// Exit after this many frames — the windowed smoke test, which is the only
    /// thing that exercises glow, winit and the real swapchain. Everything else
    /// (the kittest e2e) drives the widget tree with no window at all.
    smoke_frames: Option<u32>,
    /// Close is not instantaneous, so the verdict is latched to print once.
    smoke_done: bool,
    msgs: Vec<Message>,
    cache: emote::Cache,
    view: View,
    started: Instant,
    /// Wall time from process start to the first painted frame — "instant to
    /// open" as a number rather than an impression. Printed by the smoke run on
    /// every platform, so it cannot regress unnoticed.
    first_frame_ms: Option<f32>,
    loaded: bool,
    stats: bool,
    frames: u32,
    last_report: Instant,
    frame_ms: f32,
}

impl App {
    fn new(stats: bool, smoke_frames: Option<u32>) -> App {
        let set = emote_set();
        App {
            smoke_frames,
            smoke_done: false,
            msgs: build(&set),
            cache: emote::Cache::default(),
            view: View::default(),
            started: Instant::now(),
            first_frame_ms: None,
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
    /// Runs before every `ui()`, and *also* while eframe considers the window
    /// hidden — when no ui pass happens at all. That makes it the only place a
    /// smoke run can notice it is getting no frames and fail instead of hang.
    fn logic(&mut self, ctx: &egui::Context, _f: &mut eframe::Frame) {
        if self.smoke_frames.is_some() && self.started.elapsed() > SMOKE_DEADLINE {
            eprintln!(
                "[smoke] FAILED: {} frames in {:?} — the window never rendered. \
                 Visible={:?} focused={:?}. On a headless runner this needs a \
                 display (xvfb); under a tag-based compositor the window may \
                 have opened on an inactive tag.",
                self.frames,
                SMOKE_DEADLINE,
                ctx.input(|i| i.viewport().visible()),
                ctx.input(|i| i.viewport().focused),
            );
            std::process::exit(1);
        }
    }

    // egui 0.36 hands the app a Ui rather than a Context, and panels are shown
    // inside it. The Context is still reachable through `ui.ctx()`.
    fn ui(&mut self, ui: &mut egui::Ui, _f: &mut eframe::Frame) {
        let t0 = Instant::now();
        let ctx = ui.ctx().clone();
        if !self.loaded {
            self.load(&ctx);
        }
        let t_ms = self.started.elapsed().as_millis() as u64;
        if self.first_frame_ms.is_none() {
            // First pass through the app's own draw — window created, GL
            // context live, emotes uploaded. What a user experiences as launch.
            self.first_frame_ms = Some(self.started.elapsed().as_secs_f32() * 1000.0);
        }

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

        // Animated emotes and animated paints both need a clock. Ask for the
        // soonest repaint either needs — and nothing at all when the window is
        // off screen or nothing is animating. See cadence.rs for why that
        // matters more than it sounds.
        // What the VISIBLE rows need — not what the cache holds and not what
        // the backlog contains. Asking the whole cache meant one fast emote
        // anywhere in a channel's 7TV set pinned the repaint rate even while
        // scrolled far off screen, and the paint check swept all 10,000
        // messages every frame to answer a question only ~24 drawn rows can.
        let tick = self.view.tick_ms();
        let vis = ctx.input(|i| {
            let vp = i.viewport();
            cadence::Visibility {
                visible: vp.visible(),
                focused: vp.focused,
            }
        });
        if let Some(delay) = cadence::repaint_delay(vis, tick) {
            ctx.request_repaint_after(delay);
        }

        if let Some(budget) = self.smoke_frames {
            if !self.smoke_done && self.frames + 1 >= budget {
                self.smoke_done = true;
                // The footprint numbers ride along with the smoke verdict on
                // purpose: "lighter than what you already run" is the product
                // claim, and a claim that is only ever measured by hand is one
                // that regresses quietly. Every CI run on all three platforms
                // now prints it.
                let (font_kb, emote_kb) = texture_kb(&ctx);
                println!(
                    "[smoke] rendered {} frames, {} rows of {} msgs, {} emote stacks — ok",
                    self.frames + 1,
                    self.view.drawn_last_frame,
                    self.msgs.len(),
                    self.cache.len()
                );
                println!(
                    "[smoke] startup: first frame at {:.0} ms",
                    self.first_frame_ms.unwrap_or(f32::NAN)
                );
                println!(
                    "[smoke] footprint: rss={} font_tex={}KB emote_tex={}KB",
                    rss_kb().map_or("n/a".to_string(), |kb| format!("{kb}KB")),
                    font_kb,
                    emote_kb
                );
                assert!(
                    self.view.drawn_last_frame > 0,
                    "[smoke] a window opened but drew no rows"
                );
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            ctx.request_repaint();
        }

        let ms = t0.elapsed().as_secs_f32() * 1000.0;
        self.frame_ms = self.frame_ms * 0.9 + ms * 0.1;
        self.frames += 1;
        if self.stats && self.last_report.elapsed() >= Duration::from_secs(1) {
            let (font_kb, emote_kb) = texture_kb(&ctx);
            eprintln!(
                "[stats] fps={} frame_ms={:.2} drawn={} of {} stacks={}                  rss={} font_tex={}KB emote_tex={}KB",
                self.frames,
                self.frame_ms,
                self.view.drawn_last_frame,
                self.msgs.len(),
                self.cache.len(),
                rss_kb().map_or("n/a".to_string(), |kb| format!("{kb}KB")),
                font_kb,
                emote_kb
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
    // --smoke opens a real window, renders a few frames through glow, and
    // exits. It is the only check that covers windowing and the gpu path, so
    // it is what CI runs on each platform.
    let smoke = std::env::args()
        .any(|a| a == "--smoke")
        .then_some(SMOKE_FRAMES);
    eframe::run_native(
        "heatsync",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default().with_inner_size([980.0, 720.0]),
            ..Default::default()
        },
        Box::new(move |_cc| Ok(Box::new(App::new(stats, smoke)))),
    )
}

/// Resident set size, so the footprint claim is a number rather than a belief.
///
/// /proc only — a portable RSS read would mean a dependency, and the claim this
/// backs ("lighter than what you already run") is checked on linux and windows
/// by hand, not in a loop. Returns None where there is no /proc.
fn rss_kb() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
        // field 1 is resident pages
        let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
        Some(pages * 4)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Split egui's texture memory into the font atlas and everything else.
///
/// Those are the two things worth telling apart: the atlas is a fixed cost paid
/// once for whichever fonts are compiled in, and the rest scales with how many
/// distinct emote stacks are on screen. Knowing which one dominates is what
/// decides whether trimming fonts is worth its cost in emoji coverage.
fn texture_kb(ctx: &egui::Context) -> (usize, usize) {
    let manager = ctx.tex_manager();
    let manager = manager.read();
    let mut font = 0;
    let mut other = 0;
    for (_, meta) in manager.allocated() {
        if meta.name.contains("font") {
            font += meta.bytes_used();
        } else {
            other += meta.bytes_used();
        }
    }
    (font / 1024, other / 1024)
}
