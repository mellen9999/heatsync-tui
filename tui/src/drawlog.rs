//! HS_DEBUG_DRAW=<path>: append one line per second — draws and key events.
//! off (and free) when the variable is unset. while it is set the composer also
//! shows without a send token, so typing can be benchmarked on a logged-out box.

use std::fs::File;
use std::io::Write;
use std::time::{Duration, Instant};

pub fn enabled() -> bool {
    std::env::var_os("HS_DEBUG_DRAW").is_some()
}

pub struct DrawLog {
    file: Option<File>,
    since: Instant,
    draws: u32,
    keys: u32,
}

impl DrawLog {
    pub fn from_env() -> Self {
        let file = std::env::var_os("HS_DEBUG_DRAW").and_then(|p| File::create(p).ok());
        DrawLog {
            file,
            since: Instant::now(),
            draws: 0,
            keys: 0,
        }
    }

    pub fn draw(&mut self) {
        self.draws += 1;
        self.flush(false);
    }

    pub fn key(&mut self) {
        self.keys += 1;
    }

    fn flush(&mut self, force: bool) {
        let Some(f) = self.file.as_mut() else { return };
        if !force && self.since.elapsed() < Duration::from_secs(1) {
            return;
        }
        let _ = writeln!(f, "draws={} keys={}", self.draws, self.keys);
        self.draws = 0;
        self.keys = 0;
        self.since = Instant::now();
    }
}
