//! emote image layer — lazy background load (fetch → decode → scale → encode
//! protocols off the render thread), a shared animation clock, and visible-only
//! draw. the render thread only ever blits already-built frames; a raid can't
//! stall it.
//!
//! tier: ratatui-image auto-detects (sixel on foot, kitty where present, else
//! half-blocks). we hand it decoded frames, so no image codecs pulled here.
//!
//! three properties this module is responsible for:
//!   * DIMENSIONS — every emote is scaled to exactly its cell block in pixels and
//!     centred, so nothing is letterboxed into a corner, stretched, or upscaled
//!     from a too-small source. width is per-emote (real aspect), height uniform.
//!   * NO FLASH — frames of one emote share a single quantised palette, so the
//!     sixel encoder's error diffusion has nothing to diffuse and static regions
//!     encode byte-identically frame to frame (that shimmer IS the flashing).
//!     the draw budget freezes animation instead of blanking an emote.
//!   * FULL FPS — the event loop sleeps until the next frame boundary of the
//!     soonest-flipping emote, so playback runs at the emote's authored rate.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use image::imageops::FilterType;
use image::{DynamicImage, RgbaImage};
use ratatui::layout::Rect;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;
use ratatui_image::Resize;

use super::decode;
use crate::http;

/// inline emote height in cells. 2 rows so emotes read big; messages containing
/// a ready emote get EMOTE_H rows in the layout. WIDTH is per-emote — derived
/// from the emote's real aspect and the terminal's cell aspect — so a square
/// emote renders square and a wide one isn't crushed into a square hole.
pub const EMOTE_H: u16 = 2;
/// widest an emote may get, in cells. a hostile 20:1 image can't eat the row.
const MAX_W: u16 = 8;
/// frames kept per emote. longer animations are resampled onto this many slots
/// at the same total duration — bounds memory and terminal-side image handles
/// without changing playback speed.
pub(crate) const MAX_FRAMES: usize = 120;
/// total encoded frames held across all emotes; the LRU trims past this. also
/// caps how many image ids live in a kitty terminal at once.
const MAX_LIVE_FRAMES: usize = 6000;
/// graphics blits per draw. exceeding it FREEZES animation on the frames already
/// shown — it never blanks an emote (a blank frame is exactly the flash we're
/// removing). a raid still can't firehose the pty.
const DRAW_BUDGET: usize = 96;
/// per-frame delay floor (50fps ceiling) — matches decode's floor so a malformed
/// emote can't spin the animator.
pub(crate) const MIN_DELAY_MS: u32 = 20;
/// colors in the shared per-emote palette. under sixel's 256 with headroom.
const PALETTE_COLORS: u32 = 200;

struct FrameProto {
    proto: Protocol,
    delay_ms: u32,
}

/// a fully built emote: its frames, its cell footprint, and its loop length.
struct Anim {
    frames: Vec<FrameProto>,
    w_cells: u16,
    total_ms: u64,
    /// last index actually drawn — what we freeze on when the budget runs out.
    shown: Cell<usize>,
    /// LRU stamp, bumped whenever the draw path touches this emote.
    used: Cell<u64>,
}

enum Entry {
    Loading,
    Ready(Anim),
    Failed,
}

struct Job {
    key: String, // one url, or base+overlay urls joined by '\n' (a stack)
}
struct Done {
    key: String,
    built: Option<Built>,
}
struct Built {
    frames: Vec<FrameProto>,
    w_cells: u16,
    total_ms: u64,
}

pub struct EmoteStore {
    proto: ProtocolType, // the detected inline-graphics protocol (for the tier readout)
    /// inside tmux the attached client can change at any time — detach from
    /// foot, reattach over mosh from a phone — and our frames are encoded for
    /// the protocol probed at startup. a watcher THREAD re-asks tmux every 2s
    /// and stores the answer here; the render thread only ever reads the flag,
    /// so the subprocess spawn can never hitch a frame.
    tmux: bool,
    outer_ok: Arc<AtomicBool>,
    /// footprint of a 1:1 emote in cells. the layout reserves this BEFORE an
    /// emote finishes loading, so nothing reflows when the image lands.
    square_w: u16,
    cache: HashMap<String, Entry>,
    jobs: Sender<Job>,
    done: Receiver<Done>,
    start: Instant,
    /// animation clock snapshotted once per draw, so every emote in a frame is
    /// sampled at the SAME instant (deterministic frame, exact flip scheduling).
    now_ms: Cell<u64>,
    /// soonest ms-to-flip among the animated emotes ACTUALLY DRAWN this frame,
    /// recorded by frame() as a free by-product of picking the frame index. this
    /// is what the event loop sleeps on — off-screen animations never wake it.
    next_flip_ms: Cell<u64>,
    // interior-mutable so frame() can run inside ratatui's immutable draw pass.
    budget_left: Cell<usize>,
    lru: Cell<u64>,
    live_frames: usize,
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
        let (cw, ch) = picker.font_size();
        let square_w = width_cells(1.0, cw, ch);
        let (jobs_tx, jobs_rx) = mpsc::channel::<Job>();
        let (done_tx, done_rx) = mpsc::channel::<Done>();
        // a small loader pool: fetch + decode + scale + encode off the render
        // thread. joining a channel drops ~30 jobs at once; one thread built
        // them serially and emotes trickled in — a few threads (network-bound
        // work) make the whole set land near-simultaneously. capped low so a
        // passive-cooled box never spins every core on decode.
        let jobs_rx = Arc::new(Mutex::new(jobs_rx));
        let workers = thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2)
            .clamp(2, 4);
        for _ in 0..workers {
            let rx = Arc::clone(&jobs_rx);
            let tx = done_tx.clone();
            let p = picker.clone();
            thread::spawn(move || loader(p, rx, tx));
        }
        let tmux = std::env::var_os("TMUX").is_some();
        let outer_ok = Arc::new(AtomicBool::new(true));
        if tmux {
            // the attached-client watcher. asking tmux means fork+exec — done on
            // the render thread that was a guaranteed frame hitch every TTL.
            let flag = Arc::clone(&outer_ok);
            thread::spawn(move || loop {
                thread::sleep(Duration::from_secs(2));
                // no client attached → keep the last answer (nobody is looking).
                if let Some(p) = tmux_outer_protocol_now() {
                    flag.store(p == Some(proto), Ordering::Relaxed);
                }
            });
        }
        Some(EmoteStore {
            proto,
            tmux,
            outer_ok,
            square_w,
            cache: HashMap::new(),
            jobs: jobs_tx,
            done: done_rx,
            start: Instant::now(),
            now_ms: Cell::new(0),
            next_flip_ms: Cell::new(u64::MAX),
            budget_left: Cell::new(DRAW_BUDGET),
            lru: Cell::new(0),
            live_frames: 0,
        })
    }

    /// the footprint a not-yet-loaded emote is laid out at: square, since most
    /// emotes are. reserving space up front is what stops the chat from jumping
    /// (and every sixel below from being re-emitted) as images arrive.
    pub fn square_cells(&self) -> u16 {
        self.square_w
    }

    /// can the terminal that is on screen RIGHT NOW display our frames?
    /// outside tmux the terminal can't change under us — always true. inside
    /// tmux the session outlives any one client: started from foot, later
    /// reattached over mosh from termux, the sixel/kitty escapes we emit reach
    /// a terminal that silently drops them and every emote is a blank hole.
    /// the watcher thread re-asks tmux every 2s and reports false while the
    /// outer terminal can't render the protocol our frames are encoded in —
    /// the caller then draws the text tier (emote names) instead. this read is
    /// a single atomic load: nothing here can stall a frame.
    pub fn usable(&self) -> bool {
        !self.tmux || self.outer_ok.load(Ordering::Relaxed)
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
        if self.cache.contains_key(key) {
            return;
        }
        self.cache.insert(key.to_string(), Entry::Loading);
        // if the loader thread died the send fails — mark failed, fall to text.
        if self
            .jobs
            .send(Job {
                key: key.to_string(),
            })
            .is_err()
        {
            self.cache.insert(key.to_string(), Entry::Failed);
        }
    }

    /// drain finished loads into the cache. called on the data tick. returns true
    /// if ≥1 emote finished loading (the caller forces a full re-emit — see below).
    pub fn pump(&mut self) -> bool {
        let mut loaded = false;
        let stamp = self.lru.get();
        while let Ok(d) = self.done.try_recv() {
            let entry = match d.built {
                Some(b) if !b.frames.is_empty() => {
                    self.live_frames += b.frames.len();
                    Entry::Ready(Anim {
                        frames: b.frames,
                        w_cells: b.w_cells,
                        total_ms: b.total_ms,
                        shown: Cell::new(0),
                        used: Cell::new(stamp),
                    })
                }
                _ => Entry::Failed,
            };
            if let Some(Entry::Ready(old)) = self.cache.insert(d.key, entry) {
                self.live_frames -= old.frames.len(); // a re-request replacing itself
            }
            loaded = true;
        }
        if loaded {
            self.evict();
        }
        loaded
    }

    /// trim least-recently-drawn emotes until the live frame count is back under
    /// budget. bounds our memory AND the number of images the terminal holds.
    fn evict(&mut self) {
        if self.live_frames <= MAX_LIVE_FRAMES {
            return;
        }
        let mut aged: Vec<(u64, String)> = self
            .cache
            .iter()
            .filter_map(|(k, e)| match e {
                Entry::Ready(a) => Some((a.used.get(), k.clone())),
                _ => None,
            })
            .collect();
        aged.sort_unstable_by_key(|(used, _)| *used);
        for (_, key) in aged {
            if self.live_frames <= MAX_LIVE_FRAMES {
                break;
            }
            if let Some(Entry::Ready(a)) = self.cache.remove(&key) {
                self.live_frames -= a.frames.len();
            }
        }
    }

    /// start a draw: snapshot the animation clock and reset the blit budget. every
    /// emote in this frame is then sampled at the same instant.
    pub fn begin_frame(&self) {
        self.now_ms.set(self.start.elapsed().as_millis() as u64);
        self.budget_left.set(DRAW_BUDGET);
        self.next_flip_ms.set(u64::MAX);
    }

    /// how long until the soonest-flipping emote DRAWN LAST FRAME changes frame.
    /// the event loop sleeps exactly this long, so on-screen animations run at
    /// their authored fps — while animated emotes in unfocused channels (still
    /// cached, never drawn) schedule nothing and cost nothing.
    pub fn next_flip_in(&self) -> Option<Duration> {
        let soonest = self.next_flip_ms.get();
        (soonest != u64::MAX).then(|| Duration::from_millis(soonest.max(1)))
    }

    /// this emote/stack's width in cells, or None if it isn't loaded yet. doubles
    /// as the readiness check — a caller that gets a width can lay it out.
    pub fn cells(&self, key: &str) -> Option<u16> {
        match self.cache.get(key) {
            Some(Entry::Ready(a)) => {
                self.touch(a);
                Some(a.w_cells)
            }
            _ => None,
        }
    }

    /// the protocol for this emote/stack's current animation frame. a ready emote
    /// ALWAYS yields a frame — when the blit budget is spent it yields the one
    /// already on screen (a frozen emote, never a blank hole).
    pub fn frame(&self, key: &str) -> Option<&Protocol> {
        let Some(Entry::Ready(a)) = self.cache.get(key) else {
            return None;
        };
        self.touch(a);
        let idx = if a.frames.len() == 1 {
            0
        } else {
            let left = self.budget_left.get();
            if left == 0 {
                // frozen, not skipped: retry at the fps floor so a burst that
                // exhausts the budget can't park animations on the data tick.
                self.next_flip_ms
                    .set(self.next_flip_ms.get().min(MIN_DELAY_MS as u64));
                a.shown.get().min(a.frames.len() - 1)
            } else {
                self.budget_left.set(left - 1);
                let (idx, flip_in) = frame_at(&a.frames, self.now_ms.get(), a.total_ms);
                self.next_flip_ms.set(self.next_flip_ms.get().min(flip_in));
                idx
            }
        };
        a.shown.set(idx);
        Some(&a.frames[idx].proto)
    }

    fn touch(&self, a: &Anim) {
        let t = self.lru.get() + 1;
        self.lru.set(t);
        a.used.set(t);
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
    tmux_outer_protocol_now().flatten()
}

/// like [`tmux_outer_protocol`], but keeps "couldn't ask tmux / no client
/// attached" (outer None) apart from "a client is attached and it has no
/// graphics" (Some(None)) — the runtime re-check must not flip the tier on a
/// momentary detach.
fn tmux_outer_protocol_now() -> Option<Option<ProtocolType>> {
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
    Some(
        if term.contains("kitty") || term.contains("ghostty") || term.contains("wezterm") {
            Some(ProtocolType::Kitty)
        } else if term.starts_with("foot") || term.contains("mlterm") || term.contains("contour") {
            Some(ProtocolType::Sixel)
        } else {
            None
        },
    )
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

/// the frame showing at `now_ms` (cumulative-delay window, looping) and how many
/// ms remain before it flips. `(0, u64::MAX)` for a static image — nothing to
/// schedule.
fn frame_at(frames: &[FrameProto], now_ms: u64, total_ms: u64) -> (usize, u64) {
    if frames.len() <= 1 || total_ms == 0 {
        return (0, u64::MAX);
    }
    let mut t = now_ms % total_ms;
    for (i, f) in frames.iter().enumerate() {
        let d = f.delay_ms.max(MIN_DELAY_MS) as u64;
        if t < d {
            return (i, d - t);
        }
        t -= d;
    }
    (0, 1)
}

/// how wide, in cells, an emote of this aspect ratio should be so that it renders
/// at its true proportions in a terminal whose cells are `cw`×`ch` pixels. this is
/// the whole fix for "square emotes look squashed": cells are ~1:2, so a square
/// emote spanning EMOTE_H rows needs ≈ EMOTE_H·ch/cw columns, not a fixed 3.
pub(crate) fn width_cells(aspect: f32, cw: u16, ch: u16) -> u16 {
    let block_h = (EMOTE_H as f32) * (ch.max(1) as f32);
    let want_px = block_h * aspect.max(0.01);
    ((want_px / cw.max(1) as f32).round() as i64).clamp(1, MAX_W as i64) as u16
}

/// scale `src` to fit inside `tw`×`th` preserving aspect, then CENTRE it on a
/// fully transparent canvas of exactly that size. two things matter here:
/// the output is always exactly the cell block in pixels (so the encoder does no
/// resizing of its own and the cell footprint is deterministic), and the image is
/// centred rather than shoved into the top-left with dead space below it.
pub(crate) fn fit_center(src: &RgbaImage, tw: u32, th: u32) -> RgbaImage {
    let (sw, sh) = src.dimensions();
    let mut canvas = RgbaImage::new(tw.max(1), th.max(1));
    if sw == 0 || sh == 0 {
        return canvas;
    }
    let ratio = (tw as f32 / sw as f32).min(th as f32 / sh as f32);
    let nw = ((sw as f32 * ratio).round() as u32).clamp(1, tw.max(1));
    let nh = ((sh as f32 * ratio).round() as u32).clamp(1, th.max(1));
    // Lanczos3 for downscales (crisp, and — unlike Nearest — stable frame to
    // frame, so a moving edge doesn't pop pixels on and off); Triangle when
    // enlarging, which stays smooth instead of ringing.
    let filter = if ratio < 1.0 {
        FilterType::Lanczos3
    } else {
        FilterType::Triangle
    };
    let scaled = if (nw, nh) == (sw, sh) {
        src.clone()
    } else {
        image::imageops::resize(src, nw, nh, filter)
    };
    // replace, not overlay — copy alpha verbatim onto the empty canvas.
    image::imageops::replace(
        &mut canvas,
        &scaled,
        ((tw - nw) / 2) as i64,
        ((th - nh) / 2) as i64,
    );
    canvas
}

/// alpha-composite an rgba frame onto black (premultiply), making it opaque so
/// sixel renders no transparent fringe and each animation frame fully overwrites.
pub(crate) fn flatten_onto_black(img: &mut RgbaImage) {
    for px in img.pixels_mut() {
        let a = px[3] as u16;
        px[0] = (px[0] as u16 * a / 255) as u8;
        px[1] = (px[1] as u16 * a / 255) as u8;
        px[2] = (px[2] as u16 * a / 255) as u8;
        px[3] = 255;
    }
}

/// collapse every frame of ONE emote onto a single shared palette.
///
/// this is the anti-flash fix. sixel encoders pick a palette per image and
/// error-diffuse (dither) into it; run that independently on consecutive frames
/// of an animation and identical regions come out with different dither noise
/// every frame — the emote boils/shimmers even where nothing moves. quantising
/// all frames against one palette up front leaves the encoder with ≤256 exact
/// colors and ~zero diffusion error, so unchanged pixels encode identically and
/// only real motion changes on screen.
pub(crate) fn stabilize_palette(frames: &mut [RgbaImage]) {
    if frames.len() < 2 {
        return;
    }
    let total: usize = frames.iter().map(|f| f.as_raw().len()).sum();
    let mut sample: Vec<u8> = Vec::with_capacity(total);
    for f in frames.iter() {
        sample.extend_from_slice(f.as_raw());
    }
    if sample.is_empty() {
        return;
    }
    // samplefac 10: NeuQuant learns from a 1-in-10 pixel sample — visually
    // indistinguishable on 32-64px emotes, an order of magnitude faster than
    // scanning every pixel of every frame. what matters for the anti-flash
    // property is only that ALL frames map through the SAME palette.
    let nq = color_quant::NeuQuant::new(10, PALETTE_COLORS as usize, &sample);
    let map = nq.color_map_rgba();
    // memoize color→palette lookups: index_of is a search per call, and an
    // animation repeats the same few hundred distinct colors across every
    // frame. one lookup per distinct color instead of one per pixel.
    let mut memo: HashMap<[u8; 4], [u8; 3]> = HashMap::new();
    for f in frames.iter_mut() {
        for px in f.pixels_mut() {
            let key = px.0;
            let rgb = *memo.entry(key).or_insert_with(|| {
                let i = nq.index_of(&key) * 4;
                if i + 2 < map.len() {
                    [map[i], map[i + 1], map[i + 2]]
                } else {
                    [key[0], key[1], key[2]]
                }
            });
            px.0 = [rgb[0], rgb[1], rgb[2], key[3]];
        }
    }
}

/// alpha-composite `over` onto `base` in place (standard src-over). both must be
/// the same dimensions. used to stack zero-width (overlay) emotes on their base.
fn alpha_over(base: &mut RgbaImage, over: &RgbaImage) {
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
    let total: u64 = frames
        .iter()
        .map(|f| f.delay_ms.max(MIN_DELAY_MS) as u64)
        .sum();
    if total == 0 {
        return 0;
    }
    let mut t = t_ms % total;
    for (i, f) in frames.iter().enumerate() {
        let d = f.delay_ms.max(MIN_DELAY_MS) as u64;
        if t < d {
            return i;
        }
        t -= d;
    }
    0
}

/// emote CDNs serve fixed size tiers and the emote set hands us the smallest
/// (1x = 32px). that's pixel-perfect on an 8×16 cell but has to be UPSCALED —
/// i.e. blurred — on terminals with larger cells. pick the smallest tier that
/// covers `min_px`. None → the url shape is unknown, or 1x already covers it.
fn source_url(url: &str, min_px: u32) -> Option<String> {
    if min_px <= 34 {
        return None; // 1x is 32px — already at or above the target
    }
    let swap = |from: &str, to: &str| {
        let s = url.replacen(from, to, 1);
        (s != url).then_some(s)
    };
    if url.contains("cdn.7tv.app") {
        let t = if min_px > 96 {
            "/4x"
        } else if min_px > 64 {
            "/3x"
        } else {
            "/2x"
        };
        return swap("/1x", t);
    }
    if url.contains("betterttv.net") {
        return swap("/1x", if min_px > 64 { "/3x" } else { "/2x" });
    }
    if url.contains("frankerfacez.com") {
        // ffz publishes 1, 2 and 4 only.
        let t = if min_px > 64 { "4" } else { "2" };
        return url.strip_suffix("/1").map(|s| format!("{s}/{t}"));
    }
    if url.contains("jtvnw.net") {
        let t = if min_px > 64 { "3.0" } else { "2.0" };
        return url.strip_suffix("/1.0").map(|s| format!("{s}/{t}"));
    }
    None
}

/// fetch an emote at a resolution that covers `min_px`, falling back to the url
/// we were given if the higher tier 404s or the host is unknown.
fn fetch_at(url: &str, min_px: u32) -> Option<Vec<u8>> {
    if let Some(alt) = source_url(url, min_px) {
        if let Some(bytes) = http::image_bytes(&alt) {
            return Some(bytes);
        }
    }
    http::image_bytes(url)
}

/// resample a long animation onto at most `max` frames, preserving total loop
/// duration — a 150-frame emote plays at the same speed on a third the handles.
pub(crate) fn subsample(frames: Vec<(RgbaImage, u32)>, max: usize) -> Vec<(RgbaImage, u32)> {
    if frames.len() <= max || max == 0 {
        return frames;
    }
    let total: u64 = frames.iter().map(|f| f.1.max(MIN_DELAY_MS) as u64).sum();
    let per = ((total / max as u64) as u32).max(MIN_DELAY_MS);
    let last = frames.len() - 1;
    let step = frames.len() as f64 / max as f64;
    let keep: Vec<usize> = (0..max)
        .map(|i| ((i as f64 * step).round() as usize).min(last))
        .collect();
    frames
        .into_iter()
        .enumerate()
        .filter(|(i, _)| keep.binary_search(i).is_ok())
        .map(|(_, f)| (f.0, per))
        .collect()
}

fn loader(picker: Picker, jobs: Arc<Mutex<Receiver<Job>>>, done: Sender<Done>) {
    // sixel drops the alpha channel (to_rgb8) so a transparent pixel falls back to
    // its raw rgb — often a colored fringe or box. flatten onto black first so
    // emotes sit cleanly on a dark terminal. kitty/iterm2 keep true transparency.
    let flatten = picker.protocol_type() == ProtocolType::Sixel;
    loop {
        // hold the lock only while receiving: the holder blocks on recv, takes
        // one job, releases, and works — its siblings then take the next jobs.
        let job = match jobs.lock() {
            Ok(rx) => match rx.recv() {
                Ok(j) => j,
                Err(_) => return, // store dropped → app closed
            },
            Err(_) => return,
        };
        let built = build(&picker, &job.key, flatten);
        if done
            .send(Done {
                key: job.key,
                built,
            })
            .is_err()
        {
            return; // store dropped → app closed
        }
    }
}

/// fetch + decode + composite a stack into RGBA frames (base at the bottom,
/// overlays on top), driven by the longest-animating layer. `key` is one or more
/// image urls joined by '\n'; `min_px` is the pixel size the caller will render
/// at, used to pick a CDN size tier that never needs upscaling. no encoding —
/// shared by build(), the framebuffer tier, and the render-test harness. any
/// fetch/decode failure → None.
pub fn composite_frames(key: &str, min_px: u32) -> Option<Vec<(RgbaImage, u32)>> {
    let mut layers: Vec<decode::Decoded> = Vec::new();
    for url in key.split('\n') {
        let bytes = fetch_at(url, min_px)?;
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
            let mut c = RgbaImage::new(tw, th);
            for layer in &layers {
                let f = &layer.frames[sample_frame(&layer.frames, t)];
                if f.img.dimensions() == (tw, th) {
                    alpha_over(&mut c, &f.img);
                } else {
                    let r = image::imageops::resize(&f.img, tw, th, FilterType::Lanczos3);
                    alpha_over(&mut c, &r);
                }
            }
            c
        };
        out.push((canvas, df.delay_ms));
        t += df.delay_ms.max(MIN_DELAY_MS) as u64;
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// composite a stack (see [`composite_frames`]), scale every frame to exactly its
/// cell block, share one palette across the animation, and encode to the
/// terminal's inline-graphics protocol. any failure → None.
fn build(picker: &Picker, key: &str, flatten: bool) -> Option<Built> {
    let (cw, ch) = picker.font_size();
    let block_h = EMOTE_H as u32 * ch.max(1) as u32;
    let frames = subsample(composite_frames(key, block_h)?, MAX_FRAMES);
    let (sw, sh) = frames[0].0.dimensions();
    if sw == 0 || sh == 0 {
        return None;
    }
    let w_cells = width_cells(sw as f32 / sh as f32, cw, ch);
    let (tw, th) = (w_cells as u32 * cw.max(1) as u32, block_h);

    let mut canvases: Vec<RgbaImage> = frames.iter().map(|(f, _)| fit_center(f, tw, th)).collect();
    if flatten {
        for c in &mut canvases {
            flatten_onto_black(c);
        }
        // sixel dithers per image — without a shared palette the static parts of
        // an animation shimmer. see stabilize_palette.
        stabilize_palette(&mut canvases);
    }

    let size = Rect::new(0, 0, w_cells, EMOTE_H);
    let mut out = Vec::with_capacity(canvases.len());
    let mut total_ms = 0u64;
    for (canvas, (_, delay_ms)) in canvases.into_iter().zip(frames.iter()) {
        // the canvas is ALREADY exactly `size` in pixels, so this encodes without
        // resizing and the protocol's footprint is exactly `size` — no letterbox,
        // no surprise cell count.
        let proto = picker
            .new_protocol(DynamicImage::ImageRgba8(canvas), size, Resize::Fit(None))
            .ok()?;
        let delay_ms = (*delay_ms).max(MIN_DELAY_MS);
        total_ms += delay_ms as u64;
        out.push(FrameProto { proto, delay_ms });
    }
    if out.is_empty() {
        None
    } else {
        Some(Built {
            frames: out,
            w_cells,
            total_ms,
        })
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

    // ---- dimensions ------------------------------------------------------

    #[test]
    fn square_emote_gets_square_cell_block() {
        // 8x16 cells: 2 rows = 32px tall, so a 1:1 emote needs 4 columns to be
        // square. the old fixed width of 3 made every emote 25% too narrow.
        assert_eq!(width_cells(1.0, 8, 16), 4);
        // 10x20 cells: 2 rows = 40px → 4 columns.
        assert_eq!(width_cells(1.0, 10, 20), 4);
        // a square cell (rare, but valid) → 2 columns for 2 rows.
        assert_eq!(width_cells(1.0, 10, 10), 2);
    }

    #[test]
    fn wide_emote_gets_more_columns() {
        assert_eq!(width_cells(2.0, 8, 16), 8); // 2:1 → 64px → 8 cols
        assert_eq!(width_cells(0.5, 8, 16), 2); // 1:2 (tall) → 16px → 2 cols
    }

    #[test]
    fn absurd_aspect_is_clamped_not_unbounded() {
        assert_eq!(width_cells(50.0, 8, 16), MAX_W);
        assert_eq!(width_cells(0.0001, 8, 16), 1);
        // a terminal reporting a degenerate cell size must still yield a sane,
        // in-range footprint rather than panicking or dividing by zero.
        assert!((1..=MAX_W).contains(&width_cells(1.0, 0, 0)));
        assert!((1..=MAX_W).contains(&width_cells(f32::NAN, 8, 16)));
        assert!((1..=MAX_W).contains(&width_cells(f32::INFINITY, 8, 16)));
    }

    #[test]
    fn fit_center_fills_exact_block_and_centres() {
        // 32x32 source into a 64x32 block → scaled to 32x32, centred with 16px
        // of transparent padding each side. never stretched.
        let src = RgbaImage::from_pixel(32, 32, Rgba([255, 135, 0, 255]));
        let out = fit_center(&src, 64, 32);
        assert_eq!(out.dimensions(), (64, 32)); // exactly the block
        assert_eq!(out.get_pixel(0, 16)[3], 0); // left pad transparent
        assert_eq!(out.get_pixel(63, 16)[3], 0); // right pad transparent
        assert_eq!(out.get_pixel(32, 16), &Rgba([255, 135, 0, 255])); // centred
    }

    #[test]
    fn fit_center_preserves_aspect_when_downscaling() {
        // a 4:1 source into a square block must letterbox, never squash.
        let src = RgbaImage::from_pixel(128, 32, Rgba([255, 255, 255, 255]));
        let out = fit_center(&src, 32, 32);
        assert_eq!(out.dimensions(), (32, 32));
        assert_eq!(out.get_pixel(16, 0)[3], 0); // top pad
        assert_eq!(out.get_pixel(16, 31)[3], 0); // bottom pad
        assert_eq!(out.get_pixel(16, 16)[3], 255); // image band through the middle
    }

    #[test]
    fn fit_center_upscales_small_sources_to_the_block() {
        let src = RgbaImage::from_pixel(16, 16, Rgba([1, 2, 3, 255]));
        let out = fit_center(&src, 48, 48);
        assert_eq!(out.dimensions(), (48, 48));
        assert_eq!(out.get_pixel(24, 24)[3], 255); // fills, not a 16px speck
    }

    // ---- animation clock -------------------------------------------------

    fn protos(delays: &[u32]) -> Vec<FrameProto> {
        // Protocol has no cheap constructor; Halfblocks::default() is the one
        // variant we can build without a terminal.
        delays
            .iter()
            .map(|&delay_ms| FrameProto {
                proto: Protocol::Halfblocks(Default::default()),
                delay_ms,
            })
            .collect()
    }

    #[test]
    fn frame_at_reports_index_and_time_to_flip() {
        let f = protos(&[100, 100, 100]);
        assert_eq!(frame_at(&f, 0, 300), (0, 100));
        assert_eq!(frame_at(&f, 40, 300), (0, 60));
        assert_eq!(frame_at(&f, 150, 300), (1, 50));
        assert_eq!(frame_at(&f, 300, 300), (0, 100)); // loops
    }

    #[test]
    fn static_image_never_schedules_a_redraw() {
        assert_eq!(frame_at(&protos(&[0]), 12_345, 0).1, u64::MAX);
    }

    // ---- resampling ------------------------------------------------------

    #[test]
    fn subsample_caps_frames_and_keeps_duration() {
        let frames: Vec<(RgbaImage, u32)> = (0..300).map(|_| (RgbaImage::new(1, 1), 20)).collect();
        let out = subsample(frames, MAX_FRAMES);
        assert_eq!(out.len(), MAX_FRAMES);
        let total: u64 = out.iter().map(|f| f.1 as u64).sum();
        assert!(
            (5500..=6500).contains(&total),
            "loop length preserved: {total}"
        );
    }

    #[test]
    fn subsample_leaves_short_animations_alone() {
        let frames: Vec<(RgbaImage, u32)> = (0..10).map(|_| (RgbaImage::new(1, 1), 40)).collect();
        assert_eq!(subsample(frames, MAX_FRAMES).len(), 10);
    }

    // ---- palette stability (the anti-flash property) ----------------------

    #[test]
    fn stabilize_palette_makes_unchanged_regions_byte_identical() {
        // two frames sharing a large static region, differing only in one corner.
        let mut a = RgbaImage::new(32, 32);
        for (x, y, px) in a.enumerate_pixels_mut() {
            *px = Rgba([(x * 7 % 256) as u8, (y * 5 % 256) as u8, 90, 255]);
        }
        let mut b = a.clone();
        for x in 0..8 {
            for y in 0..8 {
                b.put_pixel(x, y, Rgba([255, 0, 0, 255]));
            }
        }
        let mut frames = vec![a, b];
        stabilize_palette(&mut frames);
        // the shared region must quantise identically in both frames — that is
        // exactly what stops the sixel dither from boiling between frames.
        let (qa, qb) = (&frames[0], &frames[1]);
        for y in 8..32 {
            for x in 8..32 {
                assert_eq!(qa.get_pixel(x, y), qb.get_pixel(x, y), "drift at {x},{y}");
            }
        }
    }

    #[test]
    fn stabilize_palette_collapses_to_at_most_the_palette_size() {
        let mut f = RgbaImage::new(64, 64);
        for (x, y, px) in f.enumerate_pixels_mut() {
            *px = Rgba([x as u8 * 4, y as u8 * 4, (x + y) as u8, 255]);
        }
        let mut frames = vec![f.clone(), f];
        stabilize_palette(&mut frames);
        let mut seen = std::collections::HashSet::new();
        for px in frames[0].pixels() {
            seen.insert([px[0], px[1], px[2]]);
        }
        assert!(
            seen.len() <= PALETTE_COLORS as usize,
            "{} colors",
            seen.len()
        );
    }

    #[test]
    fn stabilize_palette_noop_on_single_frame() {
        let f = RgbaImage::from_pixel(4, 4, Rgba([1, 2, 3, 255]));
        let mut frames = vec![f.clone()];
        stabilize_palette(&mut frames);
        assert_eq!(frames[0], f);
    }

    // ---- cdn size tiers --------------------------------------------------

    #[test]
    fn source_url_upgrades_only_when_the_block_needs_it() {
        let u = "https://cdn.7tv.app/emote/ABC/1x.webp";
        assert_eq!(source_url(u, 32), None); // 1x already covers a 32px block
        assert_eq!(
            source_url(u, 40).as_deref(),
            Some("https://cdn.7tv.app/emote/ABC/2x.webp")
        );
        assert_eq!(
            source_url(u, 100).as_deref(),
            Some("https://cdn.7tv.app/emote/ABC/4x.webp")
        );
    }

    #[test]
    fn source_url_handles_each_cdn_shape() {
        assert_eq!(
            source_url("https://cdn.betterttv.net/emote/ID/1x", 40).as_deref(),
            Some("https://cdn.betterttv.net/emote/ID/2x")
        );
        assert_eq!(
            source_url("https://cdn.frankerfacez.com/emote/1234/1", 40).as_deref(),
            Some("https://cdn.frankerfacez.com/emote/1234/2")
        );
        assert_eq!(
            source_url(
                "https://static-cdn.jtvnw.net/emoticons/v2/25/default/dark/1.0",
                40
            )
            .as_deref(),
            Some("https://static-cdn.jtvnw.net/emoticons/v2/25/default/dark/2.0")
        );
    }

    #[test]
    fn source_url_leaves_unknown_hosts_alone() {
        assert_eq!(source_url("https://example.com/emote.png", 64), None);
    }
}
