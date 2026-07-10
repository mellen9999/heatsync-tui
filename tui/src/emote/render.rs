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
    proto: ProtocolType, // the detected inline-graphics protocol (for the tier readout)
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
        // one loader thread: fetch + decode + encode with a cloned picker.
        thread::spawn(move || loader(picker, jobs_rx, done_tx));
        Some(EmoteStore {
            proto,
            cache: HashMap::new(),
            jobs: jobs_tx,
            done: done_rx,
            start: Instant::now(),
            budget_left: Cell::new(DRAW_BUDGET),
        })
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

    /// drain finished loads into the cache. called on the data tick.
    pub fn pump(&mut self) {
        while let Ok(d) = self.done.try_recv() {
            let entry = match d.frames {
                Some(f) if !f.is_empty() => Entry::Ready(f),
                _ => Entry::Failed,
            };
            self.cache.insert(d.url, entry);
        }
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
            .any(|e| matches!(e, Entry::Ready(f) if f.len() > 1))
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
    if term.contains("kitty") || term.contains("ghostty") {
        Some(ProtocolType::Kitty)
    } else if term.starts_with("foot")
        || term.contains("wezterm")
        || term.contains("mlterm")
        || term.contains("contour")
    {
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
