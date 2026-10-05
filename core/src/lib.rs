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

/// what kind of non-chat event a [`Note`] marks. one vocabulary across
/// platforms — a kick gift bomb and a twitch submysterygift are both `Gift`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NoteKind {
    /// new subscription or resub (paid, prime, membership).
    Sub,
    /// gifted subs — single or bomb.
    Gift,
    /// money thrown at the stream: bits, kicks, superchats, superstickers.
    Cheer,
    /// raid — incoming or outgoing.
    Raid,
    /// channel point redemption.
    Redeem,
    /// stream went live.
    Live,
    /// stream went offline.
    Offline,
    /// game/category or title change.
    Category,
    /// announcement or generic system notice.
    Notice,
    /// heat spike / hype train — chat is going off.
    Spike,
    /// moderation: ban / timeout / chat clear.
    Mod,
}

/// a non-chat event shown inline in the chat flow. `what` is the human
/// headline ("resubscribed for 14 months", "raiding with 500 viewers") — the
/// message's `user` is the actor and its `text` carries any attached user
/// message (a resub message, a redemption input), which renders like chat.
#[derive(Clone, Debug, PartialEq)]
pub struct Note {
    pub kind: NoteKind,
    pub what: String,
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
    /// the platform's message id, when the relay sent one.
    pub id: Option<String>,
    /// set when a mod removed this line: the marker ("deleted", "timed out
    /// 10m"…). the line stays in scrollback, struck through.
    pub gone: Option<String>,
    /// set when this line is an event (sub, raid, redemption, live…) rather
    /// than plain chat — it renders as an inline notice.
    pub note: Option<Note>,
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
    /// live lines recorded since the tab was last on screen.
    unread: u32,
    /// one of them pinged the user.
    pinged: bool,
    /// lines ever appended at the live end — a monotonic clock for "how many
    /// arrived since", unaffected by the ring evicting old ones.
    pub seq: u64,
    cap: usize,
}

/// what a tab has waiting, for its color: nothing, chat, or a ping.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unread {
    None,
    Chat,
    Ping,
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
            unread: 0,
            pinged: false,
            seq: 0,
            cap,
        }
    }

    /// the tab's unread state.
    pub fn unread(&self) -> Unread {
        if self.pinged {
            Unread::Ping
        } else if self.unread > 0 {
            Unread::Chat
        } else {
            Unread::None
        }
    }

    /// flag the most recent line as a ping (caller knows who "me" is).
    pub fn ping(&mut self) {
        self.pinged = true;
    }

    /// add a client-side line (a send rejection, a connection notice): keeps the
    /// cap, but touches neither heat nor unread — it is not chat.
    pub fn system(&mut self, msg: Message) {
        if self.messages.len() == self.cap {
            self.messages.pop_front();
        }
        self.messages.push_back(msg);
        self.seq += 1;
    }

    /// a mod removed lines: mark every matching chat line (events are never
    /// struck). returns how many were marked. the caller has already routed
    /// `d` to this tab; platform is checked per line (merged tabs).
    pub fn apply(&mut self, d: &proto::Delete) -> usize {
        let mut n = 0;
        for m in self.messages.iter_mut() {
            if m.platform != d.platform || m.note.is_some() || m.gone.is_some() {
                continue;
            }
            let hit = match &d.target {
                proto::Target::Msg(id) => m.id.as_deref() == Some(id.as_str()),
                proto::Target::User(u) => !m.user.is_empty() && m.user.eq_ignore_ascii_case(u),
                proto::Target::All => true,
            };
            if hit {
                m.gone = Some(d.label.clone());
                n += 1;
            }
        }
        n
    }

    /// the tab is on screen — everything it holds is now seen.
    pub fn mark_seen(&mut self) {
        self.unread = 0;
        self.pinged = false;
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
        self.seq += 1;
        self.unread = self.unread.saturating_add(1);
    }
}

#[cfg(test)]
mod channel_tests {
    use super::*;

    #[test]
    fn unread_goes_none_chat_ping_and_clears_when_seen() {
        let mut c = Channel::new("x", Platform::Twitch, 8);
        assert_eq!(c.unread(), Unread::None);
        c.record(msg("a", "hi"), 1);
        assert_eq!(c.unread(), Unread::Chat);
        c.ping();
        assert_eq!(c.unread(), Unread::Ping);
        c.record(msg("b", "yo"), 2);
        assert_eq!(c.unread(), Unread::Ping, "a later plain line does not downgrade a ping");
        c.mark_seen();
        assert_eq!(c.unread(), Unread::None);
    }

    fn gone_after(c: &Channel) -> Vec<Option<String>> {
        c.messages.iter().map(|m| m.gone.clone()).collect()
    }

    fn del(target: proto::Target, label: &str) -> proto::Delete {
        proto::Delete {
            platform: Platform::Twitch,
            channel: "x".into(),
            target,
            label: label.into(),
        }
    }

    #[test]
    fn delete_by_id_marks_only_that_line() {
        let mut c = Channel::new("x", Platform::Twitch, 8);
        for (i, t) in ["a", "b", "c"].iter().enumerate() {
            let mut m = msg("u", t);
            m.id = Some(format!("id{i}"));
            c.record(m, 1);
        }
        assert_eq!(c.apply(&del(proto::Target::Msg("id1".into()), "deleted")), 1);
        assert_eq!(gone_after(&c), vec![None, Some("deleted".into()), None]);
        assert_eq!(c.apply(&del(proto::Target::Msg("id1".into()), "deleted")), 0, "already marked");
    }

    #[test]
    fn timeout_marks_all_of_a_users_chat_lines_not_events() {
        let mut c = Channel::new("x", Platform::Twitch, 8);
        c.record(msg("Troll", "a"), 1);
        c.record(msg("other", "b"), 1);
        let mut ev = msg("troll", "");
        ev.note = Some(Note { kind: NoteKind::Sub, what: "subbed".into() });
        c.record(ev, 1);
        c.record(msg("TROLL", "c"), 1);
        assert_eq!(c.apply(&del(proto::Target::User("troll".into()), "timed out 10m")), 2);
        let g = gone_after(&c);
        assert_eq!(g[0].as_deref(), Some("timed out 10m"));
        assert_eq!(g[1], None);
        assert_eq!(g[2], None, "events stay");
        assert_eq!(g[3].as_deref(), Some("timed out 10m"));
    }

    #[test]
    fn clear_marks_everything_on_that_platform_only() {
        let mut c = Channel::new("x", Platform::Twitch, 8);
        c.record(msg("a", "1"), 1);
        let mut k = msg("b", "2");
        k.platform = Platform::Kick;
        c.record(k, 1);
        c.apply(&del(proto::Target::All, "cleared"));
        let g = gone_after(&c);
        assert_eq!(g[0].as_deref(), Some("cleared"));
        assert_eq!(g[1], None);
    }

    #[test]
    fn seq_counts_appends_through_eviction() {
        let mut c = Channel::new("x", Platform::Twitch, 2);
        for i in 0..5 {
            c.record(msg("u", &i.to_string()), 1);
        }
        assert_eq!((c.seq, c.messages.len()), (5, 2));
    }

    #[test]
    fn backfill_is_not_unread() {
        let mut c = Channel::new("x", Platform::Twitch, 8);
        c.backfill(vec![msg("a", "old")]);
        assert_eq!(c.unread(), Unread::None);
    }

    fn msg(user: &str, text: &str) -> Message {
        Message {
            platform: Platform::Twitch,
            user: user.into(),
            text: text.into(),
            color: None,
            badges: Vec::new(),
            id: None,
            gone: None,
            reply_to: None,
            note: None,
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
