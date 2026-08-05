//! emote image layer — lazy background load (fetch → decode → encode protocols
//! off the render thread), a shared animation clock, and visible-only draw. the
//! render thread only ever blits already-built frames; a raid can't stall it.
//!
//! tier: ratatui-image auto-detects (sixel on foot, kitty where present, else
//! half-blocks). we hand it decoded frames, so no image codecs pulled here.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use heatsync_core::emote::{split_key, Effect};
use image::DynamicImage;
use ratatui::layout::Rect;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;
use ratatui_image::Resize;

use super::decode;
use crate::http;

/// inline emote footprint in cells. 2 rows tall so emotes read big (≈32px on a
/// bare console, and chunky in emulators); width ≈square once font aspect applies.
/// messages containing a ready emote get EMOTE_H rows in the layout.
pub const EMOTE_W: u16 = 3;
pub const EMOTE_H: u16 = 2;
/// cap graphics blits per frame — a raid of fresh emotes can't firehose the pty.
const DRAW_BUDGET: usize = 64;
/// parallel fetch+decode+encode workers — network dominates load latency.
const LOADER_THREADS: usize = 3;
/// LRU cap on cached stacks. bounds RAM and — critically on kitty — the
/// terminal's image storage: the protocol never deletes transmitted images, so
/// an unbounded cache would eventually hit the terminal's own LRU and evicted
/// emotes would render blank forever. dropping OUR entry rebuilds+retransmits.
const CACHE_CAP: usize = 512;

struct FrameProto {
    proto: Protocol,
    delay_ms: u32,
}

enum Entry {
    Loading,
    Ready(Vec<FrameProto>),
    Failed,
}

/// a cache slot: the entry plus its last-touch tick (LRU eviction key).
struct Slot {
    entry: Entry,
    used: Cell<u64>,
}

impl Slot {
    fn new(entry: Entry) -> Slot {
        Slot {
            entry,
            used: Cell::new(0),
        }
    }
}

struct Job {
    key: String, // one url, or base+overlay urls joined by '\n' (a stack)
}
struct Done {
    key: String,
    frames: Option<Vec<FrameProto>>,
}

pub struct EmoteStore {
    proto: ProtocolType, // the detected inline-graphics protocol (for the tier readout)
    cache: HashMap<String, Slot>,
    jobs: Sender<Job>,
    done: Receiver<Done>,
    start: Instant,
    // interior-mutable so frame() can run inside ratatui's immutable draw pass.
    budget_left: Cell<usize>,
    tick: Cell<u64>, // monotonic touch counter for LRU
}

impl EmoteStore {
    /// probe the terminal for graphics support. `None` → no protocol (text only).
    /// must be called BEFORE raw mode / alt screen.
    ///
    /// we ONLY run the stdin capability query on terminals we know will answer
    /// it. Picker's query spawns a reader thread that, on a terminal that never
    /// responds (linux tty, dumb ssh, plain xterm), lingers blocked on stdin and
    /// steals our keypresses → a hang. env-gating sidesteps that entirely: any
    /// terminal we can't positively identify as graphics-capable gets text mode.
    pub fn new() -> Option<EmoteStore> {
        // build a Picker for the terminal's inline-graphics protocol. inside tmux
        // we must NOT run the stdio capability query: its reader thread lingers on
        // stdin and steals keypresses (dead keyboard). instead we learn the OUTER
        // terminal from tmux and the cell pixel size from an ioctl — both stdin-
        // free — and force the matching protocol. from_fontsize still reads the
        // env-detected is_tmux, so output is wrapped in tmux passthrough and
        // reaches the outer terminal intact.
        let picker = if std::env::var_os("TMUX").is_some() {
            let proto = tmux_outer_protocol()?;
            let mut p = Picker::from_fontsize(cell_pixels()?);
            p.set_protocol_type(proto);
            p
        } else {
            if !graphics_capable_env() {
                return None;
            }
            Picker::from_query_stdio().ok()?
        };
        let proto = picker.protocol_type();
        // a terminal that only offers halfblocks renders emotes as blocky mush;
        // clean text names read better, so fall back to text in that case.
        if proto == ProtocolType::Halfblocks {
            return None;
        }
        let (jobs_tx, jobs_rx) = mpsc::channel::<Job>();
        let (done_tx, done_rx) = mpsc::channel::<Done>();
        // a small pool of loader threads sharing the job queue — fetch is the
        // serial bottleneck; 2x assets doubled the bytes, the pool hides it.
        let jobs_rx = Arc::new(Mutex::new(jobs_rx));
        for _ in 0..LOADER_THREADS {
            let rx = Arc::clone(&jobs_rx);
            let tx = done_tx.clone();
            let p = picker.clone();
            thread::spawn(move || loader(p, rx, tx));
        }
        Some(EmoteStore {
            proto,
            cache: HashMap::new(),
            jobs: jobs_tx,
            done: done_rx,
            start: Instant::now(),
            budget_left: Cell::new(DRAW_BUDGET),
            tick: Cell::new(0),
        })
    }

    /// redraw cadence for smooth animation, tuned to the protocol. kitty and
    /// iterm2 replace an image in place (flicker-free), so they can run fast.
    /// sixel repaints pixel bands and TEARS if pushed — foot, Windows Terminal,
    /// xterm all share this — so it gets a gentle, stable cadence instead.
    pub fn anim_interval(&self) -> Duration {
        match self.proto {
            ProtocolType::Kitty | ProtocolType::Iterm2 => Duration::from_millis(40), // ~25fps
            ProtocolType::Sixel | ProtocolType::Halfblocks => Duration::from_millis(100), // ~10fps, tear-free
        }
    }

    /// a short label for the detected graphics protocol (startup tier readout).
    pub fn tier_label(&self) -> &'static str {
        match self.proto {
            ProtocolType::Sixel => "sixel",
            ProtocolType::Kitty => "kitty",
            ProtocolType::Iterm2 => "iterm2",
            ProtocolType::Halfblocks => "halfblocks",
        }
    }

    /// ensure an emote (or overlay stack) is being loaded (idempotent). `key` is a
    /// single url or base+overlay urls joined by '\n'. called during the tick.
    pub fn request(&mut self, key: &str) {
        if let Some(slot) = self.cache.get(key) {
            slot.used.set(self.tick.get()); // wanted on screen — keep it hot
            return;
        }
        self.cache
            .insert(key.to_string(), Slot::new(Entry::Loading));
        // if the loader threads died the send fails — mark failed, fall to text.
        if self
            .jobs
            .send(Job {
                key: key.to_string(),
            })
            .is_err()
        {
            self.cache.insert(key.to_string(), Slot::new(Entry::Failed));
        }
    }

    /// drain finished loads into the cache. called on the data tick. returns true
    /// if ≥1 emote finished loading (the caller forces a full re-emit — see below).
    pub fn pump(&mut self) -> bool {
        self.tick.set(self.tick.get() + 1);
        let mut loaded = false;
        while let Ok(d) = self.done.try_recv() {
            let entry = match d.frames {
                Some(f) if !f.is_empty() => Entry::Ready(f),
                _ => Entry::Failed,
            };
            let slot = Slot::new(entry);
            slot.used.set(self.tick.get());
            self.cache.insert(d.key, slot);
            loaded = true;
        }
        self.evict();
        loaded
    }

    /// LRU eviction past CACHE_CAP. never evicts in-flight loads (their Done
    /// would resurrect a zombie entry). dropping a Ready entry frees RAM and,
    /// on re-need, rebuilds + retransmits — see CACHE_CAP for why that matters
    /// on kitty.
    fn evict(&mut self) {
        if self.cache.len() <= CACHE_CAP {
            return;
        }
        let mut by_age: Vec<(u64, String)> = self
            .cache
            .iter()
            .filter(|(_, s)| !matches!(s.entry, Entry::Loading))
            .map(|(k, s)| (s.used.get(), k.clone()))
            .collect();
        by_age.sort_unstable();
        let excess = self.cache.len().saturating_sub(CACHE_CAP);
        for (_, k) in by_age.into_iter().take(excess) {
            self.cache.remove(&k);
        }
    }

    /// sixel/halfblocks emit a freshly-loaded static image once, and foot/tmux can
    /// drop that single incremental write — ratatui's diff then never re-sends it
    /// (the cell is unchanged), so the emote stays blank until an unrelated redraw.
    /// on those tiers we force ONE full re-emit when an emote lands. kitty/iterm2
    /// update images in place and don't need it.
    pub fn needs_load_repaint(&self) -> bool {
        matches!(self.proto, ProtocolType::Sixel | ProtocolType::Halfblocks)
    }

    /// reset the per-frame draw budget. called once before each draw (decoupled
    /// from pump so the animation can redraw faster than the data tick).
    pub fn reset_budget(&self) {
        self.budget_left.set(DRAW_BUDGET);
    }

    /// is any loaded emote animated (>1 frame)? drives the redraw cadence — we
    /// only spin the fast animation clock while there's something to animate,
    /// staying lazy (event-driven) on text-only / static views.
    pub fn any_animated(&self) -> bool {
        self.cache
            .values()
            .any(|s| matches!(&s.entry, Entry::Ready(f) if f.len() > 1))
    }

    /// has this emote/stack finished loading + encoding?
    pub fn is_ready(&self, key: &str) -> bool {
        matches!(self.cache.get(key).map(|s| &s.entry), Some(Entry::Ready(_)))
    }

    /// the protocol for this emote/stack's current animation frame, if ready and
    /// the draw budget isn't spent. consumes one budget unit (visible-only cost)
    /// and touches the slot for LRU.
    pub fn frame(&self, key: &str) -> Option<&Protocol> {
        let left = self.budget_left.get();
        if left == 0 {
            return None;
        }
        let now = self.start.elapsed().as_millis() as u64;
        let slot = self.cache.get(key)?;
        slot.used.set(self.tick.get());
        let Entry::Ready(frames) = &slot.entry else {
            return None;
        };
        self.budget_left.set(left - 1);
        let idx = frame_index(frames, now);
        Some(&frames[idx].proto)
    }
}

/// cell size in pixels via TIOCGWINSZ (crossterm reads the tty geometry, never
/// stdin). None when the terminal reports no pixel dimensions to divide by.
fn cell_pixels() -> Option<(u16, u16)> {
    let ws = crossterm::terminal::window_size().ok()?;
    if ws.width == 0 || ws.height == 0 || ws.columns == 0 || ws.rows == 0 {
        return None;
    }
    Some((ws.width / ws.columns, ws.height / ws.rows))
}

/// inside tmux, ask tmux for the attached client's terminal (the OUTER emulator,
/// which tmux masks behind TERM=tmux-256color) and map it to a forced graphics
/// protocol: kitty for kitty/ghostty, sixel for foot/wezterm/mlterm/contour.
/// None → the outer terminal isn't a known graphics-capable one → text tier.
/// this is how we get inline emotes in tmux without a stdin-stealing query.
fn tmux_outer_protocol() -> Option<ProtocolType> {
    let out = std::process::Command::new("tmux")
        .args(["display-message", "-p", "#{client_termname}"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let term = String::from_utf8_lossy(&out.stdout).trim().to_lowercase();
    // prefer kitty (flicker-free, in-place image updates) wherever the outer
    // terminal supports it; sixel only where that's the terminal's best option.
    if term.contains("kitty") || term.contains("ghostty") || term.contains("wezterm") {
        Some(ProtocolType::Kitty)
    } else if term.starts_with("foot") || term.contains("mlterm") || term.contains("contour") {
        Some(ProtocolType::Sixel)
    } else {
        None
    }
}

/// heuristic: is this terminal known to support (and answer a query for) an
/// inline-graphics protocol? conservative — false for anything unrecognized,
/// including the linux tty (`TERM=linux`), which has no graphics and only 16
/// colors. detection is env-only; we never touch stdin here.
fn graphics_capable_env() -> bool {
    let e = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let term = e("TERM").unwrap_or_default();
    let prog = e("TERM_PROGRAM").unwrap_or_default();

    // kitty graphics protocol
    if e("KITTY_WINDOW_ID").is_some() || term.contains("kitty") || term.contains("ghostty") {
        return true;
    }
    // foot (sixel)
    if term.starts_with("foot") {
        return true;
    }
    // wezterm (sixel/kitty/iterm2)
    if prog == "WezTerm" || e("WEZTERM_EXECUTABLE").is_some() {
        return true;
    }
    // iterm2 / mintty (iterm2 inline images)
    if prog == "iTerm.app" || e("ITERM_SESSION_ID").is_some() || term.contains("mintty") {
        return true;
    }
    // other sixel-capable emulators
    if term.contains("mlterm") || term.contains("contour") || term.contains("st-") {
        return true;
    }
    // VTE (gnome-terminal etc.) gained sixel in 0.78 → VTE_VERSION >= 7800
    if let Some(v) = e("VTE_VERSION") {
        if v.parse::<u32>().map(|n| n >= 7800).unwrap_or(false) {
            return true;
        }
    }
    false
}

/// pick the frame whose cumulative delay window contains `now_ms` (looping).
fn frame_index(frames: &[FrameProto], now_ms: u64) -> usize {
    if frames.len() <= 1 {
        return 0;
    }
    let total: u64 = frames.iter().map(|f| f.delay_ms.max(20) as u64).sum();
    if total == 0 {
        return 0;
    }
    let mut t = now_ms % total;
    for (i, f) in frames.iter().enumerate() {
        let d = f.delay_ms.max(20) as u64;
        if t < d {
            return i;
        }
        t -= d;
    }
    0
}

fn loader(picker: Picker, jobs: Arc<Mutex<Receiver<Job>>>, done: Sender<Done>) {
    // sixel drops the alpha channel (to_rgb8) so a transparent pixel falls back to
    // its raw rgb — often a colored fringe or box. flatten onto black first so
    // emotes sit cleanly on a dark terminal. kitty/iterm2 keep true transparency.
    let flatten = picker.protocol_type() == ProtocolType::Sixel;
    loop {
        // hold the lock only for the recv — fetch/decode/encode runs unlocked.
        let job = match jobs.lock() {
            Ok(rx) => rx.recv(),
            Err(_) => return,
        };
        let Ok(job) = job else { return }; // store dropped → app closed
        let frames = build(&picker, &job.key, flatten);
        if done
            .send(Done {
                key: job.key,
                frames,
            })
            .is_err()
        {
            return;
        }
    }
}

/// how many cells wide a stack renders — `w!`/ffzW doubles the footprint.
pub fn key_width(key: &str) -> u16 {
    if split_key(key).1.contains(&Effect::Wide) {
        EMOTE_W * 2
    } else {
        EMOTE_W
    }
}

/// alpha-composite an rgba frame onto black (premultiply), making it opaque so
/// sixel renders no transparent fringe and each animation frame fully overwrites.
fn flatten_onto_black(img: &mut image::RgbaImage) {
    for px in img.pixels_mut() {
        let a = px[3] as u16;
        px[0] = (px[0] as u16 * a / 255) as u8;
        px[1] = (px[1] as u16 * a / 255) as u8;
        px[2] = (px[2] as u16 * a / 255) as u8;
        px[3] = 255;
    }
}

/// alpha-composite `over` onto `base` in place (standard src-over). both must be
/// the same dimensions. used to stack zero-width (overlay) emotes on their base.
fn alpha_over(base: &mut image::RgbaImage, over: &image::RgbaImage) {
    for (b, o) in base.pixels_mut().zip(over.pixels()) {
        let oa = o[3] as u16;
        let ia = 255 - oa;
        for c in 0..3 {
            b[c] = ((o[c] as u16 * oa + b[c] as u16 * ia) / 255) as u8;
        }
        b[3] = (oa + b[3] as u16 * ia / 255) as u8;
    }
}

/// which frame of a decoded layer is showing at `t_ms` (cumulative-delay window,
/// looping) — lets overlay layers be sampled against the driver's timeline.
fn sample_frame(frames: &[decode::Frame], t_ms: u64) -> usize {
    if frames.len() <= 1 {
        return 0;
    }
    let total: u64 = frames.iter().map(|f| f.delay_ms.max(20) as u64).sum();
    if total == 0 {
        return 0;
    }
    let mut t = t_ms % total;
    for (i, f) in frames.iter().enumerate() {
        let d = f.delay_ms.max(20) as u64;
        if t < d {
            return i;
        }
        t -= d;
    }
    0
}

/// fetch + decode + composite a stack into RGBA frames (base at the bottom,
/// overlays on top), driven by the longest-animating layer, then run the effect
/// pixel pass (`w!`/ffz modifiers). `key` is layer urls joined by '\n' plus an
/// optional `#codes` effect line. no encoding — shared by build() and the
/// render-test verification harness. any fetch/decode failure → None.
pub fn composite_frames(key: &str) -> Option<Vec<(image::RgbaImage, u32)>> {
    let (urls, fx) = split_key(key);
    let mut layers: Vec<decode::Decoded> = Vec::new();
    for url in urls {
        let bytes = http::image_bytes(url)?;
        layers.push(decode::decode(&bytes).ok()?);
    }
    // canvas = base (first layer) native size; overlays resize onto it.
    let (tw, th) = layers[0].frames.first()?.img.dimensions();
    let single = layers.len() == 1;
    let driver = layers
        .iter()
        .max_by_key(|l| l.frames.len())
        .expect("≥1 layer");
    let mut out = Vec::with_capacity(driver.frames.len());
    let mut t = 0u64;
    for df in &driver.frames {
        let canvas = if single {
            layers[0].frames[sample_frame(&layers[0].frames, t)]
                .img
                .clone()
        } else {
            let mut c = image::RgbaImage::new(tw, th);
            for layer in &layers {
                let f = &layer.frames[sample_frame(&layer.frames, t)];
                if f.img.dimensions() == (tw, th) {
                    alpha_over(&mut c, &f.img);
                } else {
                    let r = image::imageops::resize(
                        &f.img,
                        tw,
                        th,
                        image::imageops::FilterType::Triangle,
                    );
                    alpha_over(&mut c, &r);
                }
            }
            c
        };
        out.push((canvas, df.delay_ms));
        t += df.delay_ms.max(20) as u64;
    }
    apply_effects(&mut out, &fx);
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// the effect pixel pass. fx arrive sorted in application order (geometry →
/// color → motion — see [`Effect`]'s Ord), so a `w!c!` stack widens THEN
/// grays regardless of how the user typed it: stable keys, stable output.
fn apply_effects(frames: &mut Vec<(image::RgbaImage, u32)>, fx: &[Effect]) {
    use image::imageops;
    for f in fx {
        match f {
            Effect::RotateL => remap(frames, imageops::rotate270),
            Effect::RotateR => remap(frames, imageops::rotate90),
            Effect::FlipX => remap(frames, imageops::flip_horizontal),
            Effect::FlipY => remap(frames, imageops::flip_vertical),
            Effect::Wide => remap(frames, |i| {
                imageops::resize(i, i.width() * 2, i.height(), imageops::FilterType::Triangle)
            }),
            Effect::Cursed => {
                for (img, _) in frames.iter_mut() {
                    cursed(img);
                }
            }
            Effect::Rainbow => rainbow(frames),
            Effect::Shake => shake(frames),
        }
    }
}

fn remap(
    frames: &mut [(image::RgbaImage, u32)],
    f: impl Fn(&image::RgbaImage) -> image::RgbaImage,
) {
    for (img, _) in frames.iter_mut() {
        *img = f(img);
    }
}

/// ffzCursed / `c!` — grayscale, darkened, alpha kept.
fn cursed(img: &mut image::RgbaImage) {
    for px in img.pixels_mut() {
        let l = (px[0] as u32 * 30 + px[1] as u32 * 59 + px[2] as u32 * 11) / 100;
        let v = (l * 55 / 100) as u8;
        px[0] = v;
        px[1] = v;
        px[2] = v;
    }
}

/// ffzRainbow / `p!` — hue cycle. a static emote gains synthetic frames (one
/// full cycle); an animated one hue-shifts along its own timeline.
fn rainbow(frames: &mut Vec<(image::RgbaImage, u32)>) {
    use image::imageops::huerotate;
    if frames.len() == 1 {
        let (base, _) = frames.remove(0);
        *frames = (0..12).map(|k| (huerotate(&base, k * 30), 90)).collect();
        return;
    }
    let total: u64 = frames.iter().map(|(_, d)| (*d).max(20) as u64).sum();
    let mut t = 0u64;
    for (img, d) in frames.iter_mut() {
        let deg = (t * 360 / total.max(1)) as i32;
        *img = huerotate(img, deg);
        t += (*d).max(20) as u64;
    }
}

/// ffzHyper / `s!` — jitter. a static emote gains synthetic offset frames; an
/// animated one offsets each of its own frames. deterministic pattern (no rng —
/// identical keys must render identically for the cache + tests).
fn shake(frames: &mut Vec<(image::RgbaImage, u32)>) {
    const PAT: [(i64, i64); 6] = [(0, 0), (1, -1), (-1, 1), (1, 1), (-1, 0), (0, 1)];
    let offset = |img: &image::RgbaImage, (dx, dy): (i64, i64)| {
        let amp = (img.width() as i64 / 10).max(1);
        let mut canvas = image::RgbaImage::new(img.width(), img.height());
        image::imageops::overlay(&mut canvas, img, dx * amp, dy * amp);
        canvas
    };
    if frames.len() == 1 {
        let (base, _) = frames.remove(0);
        *frames = PAT.iter().map(|&p| (offset(&base, p), 50)).collect();
        return;
    }
    for (i, (img, _)) in frames.iter_mut().enumerate() {
        *img = offset(img, PAT[i % PAT.len()]);
    }
}

/// composite a stack (see [`composite_frames`]) and encode every frame to the
/// terminal's inline-graphics protocol. frames are pre-resized HERE to the
/// exact pixel footprint with Lanczos3: ratatui-image's own resize defaults to
/// Nearest and refuses to upscale — handing it an exact-size image bypasses
/// its filter, its no-upscale rule, and its padding overlay entirely.
fn build(picker: &Picker, key: &str, flatten: bool) -> Option<Vec<FrameProto>> {
    let cells_w = key_width(key);
    let size = Rect::new(0, 0, cells_w, EMOTE_H);
    let (fw, fh) = picker.font_size();
    let (px_w, px_h) = (cells_w as u32 * fw as u32, EMOTE_H as u32 * fh as u32);
    let frames = composite_frames(key)?;
    let mut out = Vec::with_capacity(frames.len());
    for (canvas, delay_ms) in frames {
        let mut canvas = if canvas.dimensions() == (px_w, px_h) || px_w == 0 || px_h == 0 {
            canvas
        } else {
            image::imageops::resize(&canvas, px_w, px_h, image::imageops::FilterType::Lanczos3)
        };
        if flatten {
            flatten_onto_black(&mut canvas);
        }
        let proto = picker
            .new_protocol(DynamicImage::ImageRgba8(canvas), size, Resize::Fit(None))
            .ok()?;
        out.push(FrameProto { proto, delay_ms });
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    #[test]
    fn alpha_over_opaque_replaces() {
        let mut base = RgbaImage::from_pixel(2, 2, Rgba([255, 0, 0, 255])); // red
        let over = RgbaImage::from_pixel(2, 2, Rgba([0, 0, 255, 255])); // opaque blue
        alpha_over(&mut base, &over);
        assert_eq!(base.get_pixel(0, 0), &Rgba([0, 0, 255, 255])); // fully covered → blue
    }

    #[test]
    fn alpha_over_transparent_keeps_base() {
        let mut base = RgbaImage::from_pixel(2, 2, Rgba([255, 0, 0, 255])); // red
        let over = RgbaImage::from_pixel(2, 2, Rgba([0, 0, 255, 0])); // transparent blue
        alpha_over(&mut base, &over);
        assert_eq!(base.get_pixel(0, 0), &Rgba([255, 0, 0, 255])); // untouched → red
    }

    #[test]
    fn alpha_over_half_blends() {
        let mut base = RgbaImage::from_pixel(1, 1, Rgba([0, 0, 0, 255])); // black
        let over = RgbaImage::from_pixel(1, 1, Rgba([255, 255, 255, 128])); // ~50% white
        alpha_over(&mut base, &over);
        let p = base.get_pixel(0, 0);
        assert!((120..=136).contains(&p[0]), "blended ~half: {}", p[0]);
        assert_eq!(p[3], 255); // base was opaque → stays opaque
    }

    #[test]
    fn wide_doubles_pixels_and_cells() {
        let mut frames = vec![(RgbaImage::new(8, 8), 0u32)];
        apply_effects(&mut frames, &[Effect::Wide]);
        assert_eq!(frames[0].0.dimensions(), (16, 8));
        assert_eq!(key_width("u\n#w"), EMOTE_W * 2);
        assert_eq!(key_width("u"), EMOTE_W);
    }

    #[test]
    fn flip_x_mirrors() {
        let mut img = RgbaImage::new(2, 1);
        img.put_pixel(0, 0, Rgba([255, 0, 0, 255]));
        let mut frames = vec![(img, 0u32)];
        apply_effects(&mut frames, &[Effect::FlipX]);
        assert_eq!(frames[0].0.get_pixel(1, 0), &Rgba([255, 0, 0, 255]));
    }

    #[test]
    fn cursed_grays_and_darkens() {
        let mut frames = vec![(RgbaImage::from_pixel(1, 1, Rgba([200, 100, 50, 255])), 0u32)];
        apply_effects(&mut frames, &[Effect::Cursed]);
        let p = frames[0].0.get_pixel(0, 0);
        assert_eq!(p[0], p[1]);
        assert_eq!(p[1], p[2]); // gray
        assert!(p[0] < 130); // darkened
        assert_eq!(p[3], 255); // alpha kept
    }

    #[test]
    fn rainbow_animates_a_static_emote() {
        let mut frames = vec![(RgbaImage::from_pixel(2, 2, Rgba([255, 0, 0, 255])), 0u32)];
        apply_effects(&mut frames, &[Effect::Rainbow]);
        assert_eq!(frames.len(), 12);
        // hue actually moves: not every frame is still pure red
        assert!(frames.iter().any(|(f, _)| f.get_pixel(0, 0)[1] > 64));
    }

    #[test]
    fn shake_animates_a_static_emote() {
        let mut frames = vec![(
            RgbaImage::from_pixel(4, 4, Rgba([255, 255, 255, 255])),
            0u32,
        )];
        apply_effects(&mut frames, &[Effect::Shake]);
        assert_eq!(frames.len(), 6);
        // offset frames leave a transparent edge somewhere
        assert!(frames.iter().any(|(f, _)| f.pixels().any(|p| p[3] == 0)));
    }

    #[test]
    fn sample_frame_walks_timeline() {
        let f = |ms: u32| decode::Frame {
            img: RgbaImage::new(1, 1),
            delay_ms: ms,
        };
        let frames = vec![f(100), f(100), f(100)];
        assert_eq!(sample_frame(&frames, 0), 0);
        assert_eq!(sample_frame(&frames, 150), 1);
        assert_eq!(sample_frame(&frames, 250), 2);
        assert_eq!(sample_frame(&frames, 300), 0); // loops
    }
}
