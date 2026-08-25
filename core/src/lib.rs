//! heatsync-core — the brain. protocol types, heat ramp, emote resolution, ws
//! parsing, mock feed, and the editing model. no terminal, no async, no i/o
//! (clocks and keystrokes are passed in). every client is a thin face over this.
//!
//! The editing half (`key`, `edit`, `vi`, `slash`, `clip`) lived in the tui
//! until a second face needed it. None of it ever touched the terminal except
//! for crossterm's key type, which `key` now replaces — a face maps its own
//! keystrokes into `key::KeyEvent` and gets the same editor.

pub mod clip;
pub mod complete;
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
    /// youtube live chat — a "channel" here is a live VIDEO id, not a handle.
    Youtube,
}

impl Platform {
    pub fn tag(self) -> &'static str {
        match self {
            Platform::Twitch => "tw",
            Platform::Kick => "kk",
            Platform::Youtube => "yt",
        }
    }
}

/// a user role badge, normalized across platforms. twitch sends
/// `"broadcaster/1,moderator/1"`, kick sends `[{name, version}]` — both parse
/// into this one vocabulary and unknown badges are dropped (nothing renders a
/// badge we don't recognize).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Badge {
    Broadcaster,
    Moderator,
    Vip,
    Subscriber,
    Founder,
    Staff,
    Verified,
    Og,
}

impl Badge {
    /// a platform badge name (already lowercased by the caller or matched
    /// case-insensitively here) → our vocabulary. `partner` is twitch's
    /// verified checkmark.
    pub fn from_name(name: &str) -> Option<Badge> {
        Some(match name.to_ascii_lowercase().as_str() {
            "broadcaster" => Badge::Broadcaster,
            "moderator" => Badge::Moderator,
            "vip" => Badge::Vip,
            "subscriber" => Badge::Subscriber,
            "founder" => Badge::Founder,
            "staff" | "admin" => Badge::Staff,
            "verified" | "partner" => Badge::Verified,
            "og" => Badge::Og,
            _ => return None,
        })
    }

    /// single-cell glyph — badges must never widen a line by more than one
    /// column each.
    pub fn glyph(self) -> char {
        match self {
            Badge::Broadcaster => 'B',
            Badge::Moderator => 'M',
            Badge::Vip => 'V',
            Badge::Subscriber => 'S',
            Badge::Founder => 'F',
            Badge::Staff => 'A',
            Badge::Verified => '✓',
            Badge::Og => 'O',
        }
    }
}

/// one chat line. `heat` is the channel's heat snapshotted at arrival, so a
/// message sent during a spike stays warm in scrollback. `color` is the user's
/// chat color (hex) if the platform sent one.
#[derive(Clone, Debug)]
pub struct Message {
    /// where this line came from — a merged tab interleaves platforms, and
    /// each line keeps its origin for display.
    pub platform: Platform,
    pub user: String,
    pub text: String,
    pub color: Option<String>,
    pub badges: Vec<Badge>,
    /// username this message replies to, when the platform sent one.
    pub reply_to: Option<String>,
    pub heat: f64,
}

/// live state for a single channel. bounded ring buffer — a raid can flood
/// forever and we never grow past `cap` (mele is 8GB; resource-conscious).
/// heat is a decaying counter driven by message arrivals.
#[derive(Clone, Debug)]
pub struct Channel {
    pub name: String,
    /// primary source — sends target it, and it leads the tab label.
    pub platform: Platform,
    /// further sources merged into this tab (a `+`-joined channel). their
    /// lines interleave into the one ring in arrival order.
    pub extra: Vec<(Platform, String)>,
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
            extra: Vec::new(),
            heat: 0.0,
            last_ms: 0,
            messages: VecDeque::with_capacity(cap),
            cap,
        }
    }

    /// every source feeding this tab, primary first.
    pub fn subs(&self) -> impl Iterator<Item = (Platform, &str)> {
        std::iter::once((self.platform, self.name.as_str()))
            .chain(self.extra.iter().map(|(p, n)| (*p, n.as_str())))
    }

    /// does a line for (platform, channel) belong to this tab?
    pub fn matches(&self, platform: Platform, name: &str) -> bool {
        self.subs()
            .any(|(p, n)| p == platform && n.eq_ignore_ascii_case(name))
    }

    /// is this a merged (multi-source) tab?
    pub fn merged(&self) -> bool {
        !self.extra.is_empty()
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

    /// seed scrollback with archived history: prepended oldest-outward, deduped
    /// against lines already buffered (the live feed may overlap the archive's
    /// tail), heat untouched — history is cold by definition. `msgs` is
    /// chronological (oldest first); the cap still bounds the buffer.
    pub fn backfill(&mut self, msgs: Vec<Message>) {
        for m in msgs.into_iter().rev() {
            if self.messages.len() == self.cap {
                break;
            }
            if self
                .messages
                .iter()
                .any(|e| e.user == m.user && e.text == m.text)
            {
                continue;
            }
            self.messages.push_front(m);
        }
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

#[cfg(test)]
mod channel_tests {
    use super::*;

    fn msg(user: &str, text: &str) -> Message {
        Message {
            platform: Platform::Twitch,
            user: user.into(),
            text: text.into(),
            color: None,
            badges: Vec::new(),
            reply_to: None,
            heat: 0.0,
        }
    }

    #[test]
    fn backfill_prepends_in_order_and_dedupes_against_live() {
        let mut ch = Channel::new("c", Platform::Twitch, 10);
        ch.record(msg("live", "already here"), 1);
        ch.backfill(vec![
            msg("a", "one"),
            msg("live", "already here"), // overlap with the live tail
            msg("b", "two"),
        ]);
        let got: Vec<&str> = ch.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(got, vec!["one", "two", "already here"]);
    }

    #[test]
    fn backfill_respects_the_cap_keeping_the_newest_history() {
        let mut ch = Channel::new("c", Platform::Twitch, 3);
        ch.record(msg("live", "now"), 1);
        ch.backfill((0..5).map(|i| msg("u", &format!("h{i}"))).collect());
        assert_eq!(ch.messages.len(), 3);
        let got: Vec<&str> = ch.messages.iter().map(|m| m.text.as_str()).collect();
        // newest history survives, oldest is dropped
        assert_eq!(got, vec!["h3", "h4", "now"]);
    }

    #[test]
    fn merged_tab_matches_every_sub_case_insensitively() {
        let mut ch = Channel::new("xqc", Platform::Twitch, 10);
        ch.extra = vec![
            (Platform::Kick, "xqc".into()),
            (Platform::Youtube, "Vid_123".into()),
        ];
        assert!(ch.merged());
        assert!(ch.matches(Platform::Twitch, "XQC"));
        assert!(ch.matches(Platform::Kick, "xqc"));
        assert!(ch.matches(Platform::Youtube, "vid_123"));
        assert!(!ch.matches(Platform::Kick, "other"));
        assert_eq!(ch.subs().count(), 3);
        assert!(!Channel::new("xqc", Platform::Twitch, 10).merged());
    }

    #[test]
    fn backfill_leaves_heat_alone() {
        let mut ch = Channel::new("c", Platform::Kick, 10);
        ch.backfill(vec![msg("a", "x")]);
        assert_eq!(ch.heat, 0.0);
        assert_eq!(ch.messages[0].heat, 0.0);
    }
}
