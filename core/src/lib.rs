//! heatsync-core — the brain. protocol types, heat ramp, emote resolution, ws
//! parsing, mock feed, and the editing model. no terminal, no async, no i/o
//! (clocks and keystrokes are passed in). every client is a thin face over this.
//!
//! The editing half (`key`, `edit`, `vi`, `slash`, `clip`) lived in the tui
//! until a second face needed it. None of it ever touched the terminal except
//! for crossterm's key type, which `key` now replaces — a face maps its own
//! keystrokes into `key::KeyEvent` and gets the same editor.

pub mod clip;
pub mod edit;
pub mod emote;
pub mod heat;
pub mod key;
pub mod mock;
pub mod proto;
pub mod sanitize;
pub mod slash;
pub mod vi;

use std::collections::VecDeque;

/// where a channel's chat comes from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Platform {
    Twitch,
    Kick,
}

impl Platform {
    pub fn tag(self) -> &'static str {
        match self {
            Platform::Twitch => "tw",
            Platform::Kick => "kk",
        }
    }
}

/// one chat line. `heat` is the channel's heat snapshotted at arrival, so a
/// message sent during a spike stays warm in scrollback. `color` is the user's
/// chat color (hex) if the platform sent one.
#[derive(Clone, Debug)]
pub struct Message {
    pub user: String,
    pub text: String,
    pub color: Option<String>,
    pub heat: f64,
}

/// live state for a single channel. bounded ring buffer — a raid can flood
/// forever and we never grow past `cap` (mele is 8GB; resource-conscious).
/// heat is a decaying counter driven by message arrivals.
#[derive(Clone, Debug)]
pub struct Channel {
    pub name: String,
    pub platform: Platform,
    pub heat: f64,
    /// wall-clock ms of the last heat update — decay is computed against it.
    pub last_ms: u64,
    pub messages: VecDeque<Message>,
    cap: usize,
}

impl Channel {
    pub fn new(name: &str, platform: Platform, cap: usize) -> Channel {
        Channel {
            name: name.to_string(),
            platform,
            heat: 0.0,
            last_ms: 0,
            messages: VecDeque::with_capacity(cap),
            cap,
        }
    }

    /// advance heat decay to `now_ms` without adding anything (idle cooling).
    pub fn cool(&mut self, now_ms: u64) {
        if self.last_ms == 0 {
            self.last_ms = now_ms;
            return;
        }
        let dt = now_ms.saturating_sub(self.last_ms) as f64;
        self.heat = heat::decay(self.heat, dt);
        self.last_ms = now_ms;
    }

    /// record a message: decay to now, add one increment, snapshot heat onto
    /// the line, then store it (evicting the oldest past the cap).
    pub fn record(&mut self, mut msg: Message, now_ms: u64) {
        self.cool(now_ms);
        self.heat += heat::INCREMENT;
        msg.heat = self.heat;
        if self.messages.len() == self.cap {
            self.messages.pop_front();
        }
        self.messages.push_back(msg);
    }
}
