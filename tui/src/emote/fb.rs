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
/// only the linux blitter reads the fields — elsewhere the queue just drains.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
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
            Some(FbEmotes {
                inner,
                pending: RefCell::new(Vec::new()),
            })
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
            self.inner.cells(_url)
        }
        #[cfg(not(target_os = "linux"))]
        None
    }

    /// footprint of a not-yet-loaded emote, reserved up front so the chat does
    /// not reflow when images land.
    pub fn square_cells(&self) -> u16 {
        #[cfg(target_os = "linux")]
        {
            self.inner.square_cells()
        }
        #[cfg(not(target_os = "linux"))]
        1
    }

    /// how long until the soonest-flipping loaded emote changes frame — the event
    /// loop sleeps exactly this long so animations run at their authored fps.
    pub fn next_flip_in(&self) -> Option<std::time::Duration> {
        #[cfg(target_os = "linux")]
        {
            self.inner.next_flip_in()
        }
        #[cfg(not(target_os = "linux"))]
        None
    }

    /// queue an emote to be painted at a cell position (called during layout).
    pub fn push(&self, col: u16, row: u16, url: &str) {
        self.pending.borrow_mut().push(Placement {
            col,
            row,
            url: url.to_string(),
        });
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
        host: Host,
        /// the pane's top-left in console cells. under tmux a pane can move or
        /// resize at any time, so it's re-read on a short TTL rather than cached
        /// for the process lifetime.
        origin: std::cell::Cell<(u16, u16)>,
        origin_at: std::cell::Cell<Instant>,
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
            // the framebuffer tier needs the kernel console to be what's actually
            // on screen. two ways that's true — bare console, or tmux whose client
            // is attached from one. inside tmux we must NOT trust WAYLAND_DISPLAY /
            // DISPLAY: those are inherited from the tmux server's environment at
            // session-create time and say nothing about where the client is now.
            // `client_termname` is the authoritative answer, so it alone gates the
            // tmux path.
            let host = Host::detect()?;
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
            // cell size in pixels ≈ the console font cell. it must be derived
            // from the WHOLE console grid, never the pane: inside a split, pane
            // cols/rows are a fraction of the screen and would inflate the cell.
            let (grid_cols, grid_rows) = host.grid().unwrap_or((cols, rows));
            if grid_cols == 0 || grid_rows == 0 {
                return None;
            }
            let cell_w = (v.xres / grid_cols as u32).max(1);
            let cell_h = (v.yres / grid_rows as u32).max(1);
            let (jt, jr) = mpsc::channel::<Job>();
            let (dt, dr) = mpsc::channel::<Done>();
            let (cw, ch) = (cell_w as u16, cell_h as u16);
            thread::spawn(move || loader(cw, ch, jr, dt));
            let origin0 = host.origin().unwrap_or((0, 0));
            Some(Inner {
                host,
                origin: std::cell::Cell::new(origin0),
                origin_at: std::cell::Cell::new(Instant::now()),
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
            if self
                .jobs
                .send(Job {
                    url: url.to_string(),
                })
                .is_err()
            {
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

        /// re-read the pane origin at most 4x/sec. a `tmux display -p` fork is
        /// ~2ms — fine at this rate, far too much per 20fps animation frame.
        fn refresh_origin(&self) {
            if self.origin_at.get().elapsed() < Duration::from_millis(250) {
                return;
            }
            self.origin_at.set(Instant::now());
            if let Some(o) = self.host.origin() {
                self.origin.set(o);
            }
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
            self.refresh_origin();
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
            let (ocol, orow) = self.origin.get();
            let x0 = (col as usize + ocol as usize) * self.cell_w as usize;
            let y0 = (row as usize + orow as usize) * self.cell_h as usize;
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
                let off =
                    (y + self.fmt.yoff) * self.fmt.line_len + (x + self.fmt.xoff) * self.fmt.bpp;
                if off + self.fmt.bpp > buf.len() {
                    continue;
                }
                for i in 0..self.fmt.bpp {
                    buf[off + i] = ((val >> (i * 8)) & 0xff) as u8;
                }
            }
        }
    }

    /// where the console grid we're drawing into lives: the bare kernel console,
    /// or a tmux pane on top of one.
    enum Host {
        Console,
        Tmux { pane: String },
    }

    impl Host {
        fn detect() -> Option<Host> {
            let term = std::env::var("TERM").unwrap_or_default();
            if std::env::var_os("TMUX").is_some() {
                // only when the ATTACHED CLIENT is a kernel console. a tmux
                // session also reachable from foot/ssh answers something else,
                // and blitting then would paint over whatever VT is up.
                return match tmux(&["display", "-p", "#{client_termname}"])?.trim() {
                    "linux" => Some(Host::Tmux {
                        pane: std::env::var("TMUX_PANE").unwrap_or_default(),
                    }),
                    _ => None,
                };
            }
            // bare console: no multiplexer, and no compositor holding the scanout.
            if term == "linux"
                && std::env::var_os("WAYLAND_DISPLAY").is_none()
                && std::env::var_os("DISPLAY").is_none()
            {
                return Some(Host::Console);
            }
            None
        }

        /// the whole console's cell grid (not the pane's).
        fn grid(&self) -> Option<(u16, u16)> {
            match self {
                Host::Console => crossterm::terminal::size().ok(),
                Host::Tmux { .. } => {
                    let out = tmux(&["display", "-p", "#{client_width} #{client_height}"])?;
                    let mut it = out.split_whitespace();
                    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
                }
            }
        }

        /// the pane's top-left corner in console cells.
        fn origin(&self) -> Option<(u16, u16)> {
            match self {
                Host::Console => Some((0, 0)),
                Host::Tmux { pane } => {
                    let out = tmux(&["display", "-p", "-t", pane, "#{pane_left} #{pane_top}"])?;
                    let mut it = out.split_whitespace();
                    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
                }
            }
        }
    }

    fn tmux(args: &[&str]) -> Option<String> {
        let out = std::process::Command::new("tmux")
            .args(args)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
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
            out.push(FbFrame {
                rgba: fit_center(&canvas, tw, th),
                delay_ms,
            });
        }
        if out.is_empty() {
            None
        } else {
            Some(Anim {
                frames: out,
                w_cells,
                total_ms,
            })
        }
    }
}
