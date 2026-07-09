//! emote image layer — lazy background load (fetch → decode → encode protocols
//! off the render thread), a shared animation clock, and visible-only draw. the
//! render thread only ever blits already-built frames; a raid can't stall it.
//!
//! tier: ratatui-image auto-detects (sixel on foot, kitty where present, else
//! half-blocks). we hand it decoded frames, so no image codecs pulled here.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Instant;

use image::DynamicImage;
use ratatui::layout::Rect;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;
use ratatui_image::Resize;

use super::decode;
use crate::http;

/// inline emote footprint in cells (≈square once font aspect is applied).
pub const EMOTE_W: u16 = 2;
pub const EMOTE_H: u16 = 1;
/// cap graphics blits per frame — a raid of fresh emotes can't firehose the pty.
const DRAW_BUDGET: usize = 64;

struct FrameProto {
    proto: Protocol,
    delay_ms: u32,
}

enum Entry {
    Loading,
    Ready(Vec<FrameProto>),
    Failed,
}

struct Job {
    url: String,
}
struct Done {
    url: String,
    frames: Option<Vec<FrameProto>>,
}

pub struct EmoteStore {
    cache: HashMap<String, Entry>,
    jobs: Sender<Job>,
    done: Receiver<Done>,
    start: Instant,
    // interior-mutable so frame() can run inside ratatui's immutable draw pass.
    budget_left: Cell<usize>,
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
        if !graphics_capable_env() {
            return None;
        }
        let picker = Picker::from_query_stdio().ok()?;
        let (jobs_tx, jobs_rx) = mpsc::channel::<Job>();
        let (done_tx, done_rx) = mpsc::channel::<Done>();
        // one loader thread: fetch + decode + encode with a cloned picker.
        thread::spawn(move || loader(picker, jobs_rx, done_tx));
        Some(EmoteStore {
            cache: HashMap::new(),
            jobs: jobs_tx,
            done: done_rx,
            start: Instant::now(),
            budget_left: Cell::new(DRAW_BUDGET),
        })
    }

    /// ensure an emote is being loaded (idempotent). called during the tick.
    pub fn request(&mut self, url: &str) {
        if self.cache.contains_key(url) {
            return;
        }
        self.cache.insert(url.to_string(), Entry::Loading);
        // if the loader thread died the send fails — mark failed, fall to text.
        if self.jobs.send(Job { url: url.to_string() }).is_err() {
            self.cache.insert(url.to_string(), Entry::Failed);
        }
    }

    /// drain finished loads into the cache, and reset the per-frame draw budget.
    pub fn pump(&mut self) {
        while let Ok(d) = self.done.try_recv() {
            let entry = match d.frames {
                Some(f) if !f.is_empty() => Entry::Ready(f),
                _ => Entry::Failed,
            };
            self.cache.insert(d.url, entry);
        }
        self.budget_left.set(DRAW_BUDGET);
    }

    /// has this emote finished loading + encoding?
    pub fn is_ready(&self, url: &str) -> bool {
        matches!(self.cache.get(url), Some(Entry::Ready(_)))
    }

    /// the protocol for this emote's current animation frame, if ready and the
    /// draw budget isn't spent. consumes one budget unit (visible-only cost).
    pub fn frame(&self, url: &str) -> Option<&Protocol> {
        let left = self.budget_left.get();
        if left == 0 {
            return None;
        }
        let now = self.start.elapsed().as_millis() as u64;
        let frames = match self.cache.get(url) {
            Some(Entry::Ready(f)) => f,
            _ => return None,
        };
        self.budget_left.set(left - 1);
        let idx = frame_index(frames, now);
        Some(&frames[idx].proto)
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

fn loader(picker: Picker, jobs: Receiver<Job>, done: Sender<Done>) {
    let size = Rect::new(0, 0, EMOTE_W, EMOTE_H);
    for job in jobs {
        let frames = build(&picker, size, &job.url);
        if done.send(Done { url: job.url, frames }).is_err() {
            return; // store dropped → app closed
        }
    }
}

/// fetch → decode → encode every frame to a protocol. any failure → None.
fn build(picker: &Picker, size: Rect, url: &str) -> Option<Vec<FrameProto>> {
    let bytes = http::image_bytes(url)?;
    let decoded = decode::decode(&bytes).ok()?;
    let mut out = Vec::with_capacity(decoded.frames.len());
    for f in decoded.frames {
        let img = DynamicImage::ImageRgba8(f.img);
        let proto = picker.new_protocol(img, size, Resize::Fit(None)).ok()?;
        out.push(FrameProto { proto, delay_ms: f.delay_ms });
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}
