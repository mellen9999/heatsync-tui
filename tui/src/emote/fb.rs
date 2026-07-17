//! bare Linux console (TTY) emote tier — paints pixels straight to /dev/fb0,
//! the same mechanism notcurses' NCPIXEL_LINUXFB uses, but pure-Rust. we keep
//! the console in TEXT mode (never KD_GRAPHICS): ratatui draws the chat text via
//! the kernel console, and we overlay emote pixels onto the cells reserved for
//! them. lazy background load + shared clock like the terminal tier.
//!
//! the whole thing is linux-only; on other platforms `open` returns None and the
//! methods are no-ops, so the caller needs no cfg and the windows/mac .exe is
//! unaffected (the `framebuffer` dep is itself gated to linux in Cargo.toml).

use std::cell::RefCell;

use super::render::{EMOTE_H, EMOTE_W};

/// a queued emote to blit this frame at an (absolute) cell position.
pub struct Placement {
    pub col: u16,
    pub row: u16,
    pub url: String,
}

pub struct FbEmotes {
    #[cfg(target_os = "linux")]
    inner: linux::Inner,
    // pending is platform-agnostic so layout can push without cfg.
    pending: RefCell<Vec<Placement>>,
}

impl FbEmotes {
    /// open the framebuffer sized to the current cell grid, or None if this
    /// isn't a usable Linux console (wrong platform, no /dev/fb0, no perms).
    pub fn open(_cols: u16, _rows: u16) -> Option<FbEmotes> {
        #[cfg(target_os = "linux")]
        {
            let inner = linux::Inner::open(_cols, _rows)?;
            Some(FbEmotes { inner, pending: RefCell::new(Vec::new()) })
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }

    /// begin loading an emote (idempotent).
    pub fn request(&self, _url: &str) {
        #[cfg(target_os = "linux")]
        self.inner.request(_url);
    }

    /// drain finished loads into the cache.
    pub fn pump(&mut self) {
        #[cfg(target_os = "linux")]
        self.inner.pump();
    }

    /// is this emote decoded + scaled and ready to blit?
    pub fn is_ready(&self, _url: &str) -> bool {
        #[cfg(target_os = "linux")]
        {
            return self.inner.is_ready(_url);
        }
        #[cfg(not(target_os = "linux"))]
        false
    }

    /// is any loaded emote animated? (drives the fast redraw cadence)
    pub fn any_animated(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            return self.inner.any_animated();
        }
        #[cfg(not(target_os = "linux"))]
        false
    }

    /// queue an emote to be painted at a cell position (called during layout).
    pub fn push(&self, col: u16, row: u16, url: &str) {
        self.pending
            .borrow_mut()
            .push(Placement { col, row, url: url.to_string() });
    }

    /// paint all queued emotes to the framebuffer, then clear the queue. call
    /// AFTER the terminal draw has flushed its text.
    pub fn blit(&self) {
        let queued = std::mem::take(&mut *self.pending.borrow_mut());
        #[cfg(target_os = "linux")]
        self.inner.blit(&queued);
        #[cfg(not(target_os = "linux"))]
        let _ = queued;
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::HashMap;
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::thread;
    use std::time::Instant;

    use framebuffer::Framebuffer;
    use image::imageops::{resize, FilterType};
    use image::RgbaImage;

    use super::{Placement, EMOTE_H, EMOTE_W};
    use crate::emote::render::composite_frames;

    // fb blits are plain mmap writes (~3KB each) — far cheaper than terminal
    // graphics escapes, so the cap only exists to bound a pathological frame.
    const DRAW_BUDGET: usize = 256;

    struct FbFrame {
        rgba: RgbaImage, // pre-scaled to the emote's cell footprint in pixels
        delay_ms: u32,
    }
    enum Entry {
        Loading,
        Ready(Vec<FbFrame>),
        Failed,
    }
    struct Job {
        url: String,
    }
    struct Done {
        url: String,
        frames: Option<Vec<FbFrame>>,
    }

    /// packed pixel-format description read from the framebuffer once.
    struct Format {
        bpp: usize, // bytes per pixel
        line_len: usize,
        xoff: usize,
        yoff: usize,
        xres: usize,
        yres: usize,
        r: (u32, u32), // (offset, length)
        g: (u32, u32),
        b: (u32, u32),
    }

    pub struct Inner {
        fb: std::cell::RefCell<Framebuffer>,
        fmt: Format,
        cell_w: u32,
        cell_h: u32,
        cache: std::cell::RefCell<HashMap<String, Entry>>,
        jobs: Sender<Job>,
        done: Receiver<Done>,
        start: Instant,
        budget: std::cell::Cell<usize>,
    }

    impl Inner {
        pub fn open(cols: u16, rows: u16) -> Option<Inner> {
            // framebuffer tier is ONLY for the bare kernel console (TERM=linux).
            // under a wayland/x compositor /dev/fb0 may still exist but is not the
            // displayed surface, so blits vanish into an uncomposited buffer and
            // emotes render as blank reserved cells — fall back to text there.
            if std::env::var("TERM").ok().as_deref() != Some("linux")
                || std::env::var_os("WAYLAND_DISPLAY").is_some()
                || std::env::var_os("DISPLAY").is_some()
            {
                return None;
            }
            let fb = Framebuffer::new("/dev/fb0").ok()?;
            let v = &fb.var_screen_info;
            let f = &fb.fix_screen_info;
            if v.xres == 0 || v.yres == 0 || cols == 0 || rows == 0 {
                return None;
            }
            let fmt = Format {
                bpp: (v.bits_per_pixel / 8).max(1) as usize,
                line_len: f.line_length as usize,
                xoff: v.xoffset as usize,
                yoff: v.yoffset as usize,
                xres: v.xres as usize,
                yres: v.yres as usize,
                r: (v.red.offset, v.red.length),
                g: (v.green.offset, v.green.length),
                b: (v.blue.offset, v.blue.length),
            };
            // cell size in pixels ≈ the console font cell.
            let cell_w = (v.xres / cols as u32).max(1);
            let cell_h = (v.yres / rows as u32).max(1);
            let (jt, jr) = mpsc::channel::<Job>();
            let (dt, dr) = mpsc::channel::<Done>();
            let (tw, th) = (cell_w * EMOTE_W as u32, cell_h * EMOTE_H as u32);
            thread::spawn(move || loader(tw, th, jr, dt));
            Some(Inner {
                fb: std::cell::RefCell::new(fb),
                fmt,
                cell_w,
                cell_h,
                cache: std::cell::RefCell::new(HashMap::new()),
                jobs: jt,
                done: dr,
                start: Instant::now(),
                budget: std::cell::Cell::new(DRAW_BUDGET),
            })
        }

        pub fn request(&self, url: &str) {
            let mut cache = self.cache.borrow_mut();
            if cache.contains_key(url) {
                return;
            }
            cache.insert(url.to_string(), Entry::Loading);
            if self.jobs.send(Job { url: url.to_string() }).is_err() {
                cache.insert(url.to_string(), Entry::Failed);
            }
        }

        pub fn pump(&mut self) {
            while let Ok(d) = self.done.try_recv() {
                let e = match d.frames {
                    Some(f) if !f.is_empty() => Entry::Ready(f),
                    _ => Entry::Failed,
                };
                self.cache.borrow_mut().insert(d.url, e);
            }
            self.budget.set(DRAW_BUDGET);
        }

        pub fn is_ready(&self, url: &str) -> bool {
            matches!(self.cache.borrow().get(url), Some(Entry::Ready(_)))
        }

        pub fn any_animated(&self) -> bool {
            self.cache
                .borrow()
                .values()
                .any(|e| matches!(e, Entry::Ready(f) if f.len() > 1))
        }

        pub fn blit(&self, queued: &[Placement]) {
            let now = self.start.elapsed().as_millis() as u64;
            let cache = self.cache.borrow();
            let mut fb = self.fb.borrow_mut();
            for p in queued {
                if self.budget.get() == 0 {
                    break;
                }
                let frames = match cache.get(&p.url) {
                    Some(Entry::Ready(f)) => f,
                    _ => continue,
                };
                self.budget.set(self.budget.get() - 1);
                let idx = frame_index(frames, now);
                self.paint(&mut fb, &frames[idx].rgba, p.col, p.row);
            }
        }

        /// paint one emote region: opaque pixels drawn, transparent → black so
        /// the reserved cells stay clean (no ghosting) every frame.
        fn paint(&self, fb: &mut Framebuffer, img: &RgbaImage, col: u16, row: u16) {
            let x0 = col as usize * self.cell_w as usize;
            let y0 = row as usize * self.cell_h as usize;
            let buf = &mut fb.frame;
            for (px, py, pixel) in img.enumerate_pixels() {
                let x = x0 + px as usize;
                let y = y0 + py as usize;
                if x >= self.fmt.xres || y >= self.fmt.yres {
                    continue;
                }
                let [r, g, b, a] = pixel.0;
                let (r, g, b) = if a < 128 { (0, 0, 0) } else { (r, g, b) };
                let val = pack(&self.fmt, r, g, b);
                let off = (y + self.fmt.yoff) * self.fmt.line_len
                    + (x + self.fmt.xoff) * self.fmt.bpp;
                if off + self.fmt.bpp > buf.len() {
                    continue;
                }
                for i in 0..self.fmt.bpp {
                    buf[off + i] = ((val >> (i * 8)) & 0xff) as u8;
                }
            }
        }
    }

    /// pack an 8-bit rgb triple into the framebuffer's native pixel per its
    /// bitfields (handles 16/24/32bpp: rgb565, xrgb8888, etc.).
    fn pack(f: &Format, r: u8, g: u8, b: u8) -> u32 {
        let c = |v: u8, (off, len): (u32, u32)| ((v as u32) >> (8 - len)) << off;
        c(r, f.r) | c(g, f.g) | c(b, f.b)
    }

    fn frame_index(frames: &[FbFrame], now_ms: u64) -> usize {
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

    fn loader(tw: u32, th: u32, jobs: Receiver<Job>, done: Sender<Done>) {
        for job in jobs {
            let frames = build(tw, th, &job.url);
            if done.send(Done { url: job.url, frames }).is_err() {
                return;
            }
        }
    }

    fn build(tw: u32, th: u32, key: &str) -> Option<Vec<FbFrame>> {
        // key is one url or a '\n'-joined overlay stack — composite_frames
        // handles both (base at the bottom, zero-width overlays on top).
        let frames = composite_frames(key)?;
        let mut out = Vec::with_capacity(frames.len());
        for (canvas, delay_ms) in frames {
            let rgba = resize(&canvas, tw.max(1), th.max(1), FilterType::Triangle);
            out.push(FbFrame { rgba, delay_ms });
        }
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }
}
