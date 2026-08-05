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

    /// this emote's width in cells once it's decoded + scaled, else None. width
    /// is per-emote (its real aspect), so nothing is stretched to fit.
    pub fn cells(&self, _url: &str) -> Option<u16> {
        #[cfg(target_os = "linux")]
        {
            return self.inner.cells(_url);
        }
        #[cfg(not(target_os = "linux"))]
        None
    }

    /// footprint of a not-yet-loaded emote, reserved up front so the chat does
    /// not reflow when images land.
    pub fn square_cells(&self) -> u16 {
        #[cfg(target_os = "linux")]
        {
            return self.inner.square_cells();
        }
        #[cfg(not(target_os = "linux"))]
        1
    }

    /// how long until the soonest-flipping loaded emote changes frame — the event
    /// loop sleeps exactly this long so animations run at their authored fps.
    pub fn next_flip_in(&self) -> Option<std::time::Duration> {
        #[cfg(target_os = "linux")]
        {
            return self.inner.next_flip_in();
        }
        #[cfg(not(target_os = "linux"))]
        None
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
    use std::time::{Duration, Instant};

    use framebuffer::Framebuffer;
    use image::RgbaImage;

    use super::Placement;
    use crate::emote::render::{
        composite_frames, fit_center, subsample, width_cells, EMOTE_H, MAX_FRAMES, MIN_DELAY_MS,
    };

    // fb blits are plain mmap writes (~3KB each) — far cheaper than terminal
    // graphics escapes, so the cap only exists to bound a pathological frame.
    const DRAW_BUDGET: usize = 256;

    struct FbFrame {
        rgba: RgbaImage, // pre-scaled to the emote's cell footprint in pixels
        delay_ms: u32,
    }
    /// a built emote: frames already at their exact cell-block pixel size, plus
    /// the footprint the layout needs to reserve.
    struct Anim {
        frames: Vec<FbFrame>,
        w_cells: u16,
        total_ms: u64,
    }
    enum Entry {
        Loading,
        Ready(Anim),
        Failed,
    }
    struct Job {
        url: String,
    }
    struct Done {
        url: String,
        anim: Option<Anim>,
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
        square_w: u16,
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
            let (cw, ch) = (cell_w as u16, cell_h as u16);
            thread::spawn(move || loader(cw, ch, jr, dt));
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
                square_w: width_cells(1.0, cw, ch),
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
                let e = match d.anim {
                    Some(a) if !a.frames.is_empty() => Entry::Ready(a),
                    _ => Entry::Failed,
                };
                self.cache.borrow_mut().insert(d.url, e);
            }
            self.budget.set(DRAW_BUDGET);
        }

        pub fn square_cells(&self) -> u16 {
            self.square_w
        }

        /// width in cells once built, else None (which is also the "not ready
        /// yet, show the emote's name instead" signal).
        pub fn cells(&self, url: &str) -> Option<u16> {
            match self.cache.borrow().get(url) {
                Some(Entry::Ready(a)) => Some(a.w_cells),
                _ => None,
            }
        }

        /// ms until the soonest-flipping loaded emote changes frame.
        pub fn next_flip_in(&self) -> Option<Duration> {
            let now = self.start.elapsed().as_millis() as u64;
            let mut soonest = u64::MAX;
            for e in self.cache.borrow().values() {
                if let Entry::Ready(a) = e {
                    if a.frames.len() > 1 {
                        soonest = soonest.min(frame_at(&a.frames, now, a.total_ms).1);
                    }
                }
            }
            (soonest != u64::MAX).then(|| Duration::from_millis(soonest.max(1)))
        }

        pub fn blit(&self, queued: &[Placement]) {
            let now = self.start.elapsed().as_millis() as u64;
            let cache = self.cache.borrow();
            let mut fb = self.fb.borrow_mut();
            for p in queued {
                if self.budget.get() == 0 {
                    break;
                }
                let a = match cache.get(&p.url) {
                    Some(Entry::Ready(a)) => a,
                    _ => continue,
                };
                self.budget.set(self.budget.get() - 1);
                let idx = frame_at(&a.frames, now, a.total_ms).0;
                self.paint(&mut fb, &a.frames[idx].rgba, p.col, p.row);
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

    /// the frame showing at `now_ms` and how many ms until it flips.
    fn frame_at(frames: &[FbFrame], now_ms: u64, total_ms: u64) -> (usize, u64) {
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

    fn loader(cw: u16, ch: u16, jobs: Receiver<Job>, done: Sender<Done>) {
        for job in jobs {
            let anim = build(cw, ch, &job.url);
            if done.send(Done { url: job.url, anim }).is_err() {
                return;
            }
        }
    }

    fn build(cw: u16, ch: u16, key: &str) -> Option<Anim> {
        let block_h = EMOTE_H as u32 * ch.max(1) as u32;
        // key is one url or a '\n'-joined overlay stack — composite_frames
        // handles both (base at the bottom, zero-width overlays on top).
        let frames = subsample(composite_frames(key, block_h)?, MAX_FRAMES);
        let (sw, sh) = frames[0].0.dimensions();
        if sw == 0 || sh == 0 {
            return None;
        }
        // width from the emote's REAL aspect — the old code stretched every emote
        // onto a fixed 3x2 block, so square emotes came out squashed and wide ones
        // came out mangled.
        let w_cells = width_cells(sw as f32 / sh as f32, cw, ch);
        let (tw, th) = (w_cells as u32 * cw.max(1) as u32, block_h);
        let mut out = Vec::with_capacity(frames.len());
        let mut total_ms = 0u64;
        for (canvas, delay_ms) in frames {
            let delay_ms = delay_ms.max(MIN_DELAY_MS);
            total_ms += delay_ms as u64;
            out.push(FbFrame { rgba: fit_center(&canvas, tw, th), delay_ms });
        }
        if out.is_empty() {
            None
        } else {
            Some(Anim { frames: out, w_cells, total_ms })
        }
    }
}
