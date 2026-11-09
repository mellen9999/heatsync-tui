//! heatsync-core — the brain. protocol types, heat ramp, emote resolution, ws
//! parsing, mock feed, and the editing model. no terminal, no async, no i/o
//! (clocks and keystrokes are passed in). every client is a thin face over this.
//!
//! The editing half (`key`, `edit`, `vi`, `slash`, `clip`) lived in the tui
//! until a second face needed it. None of it ever touched the terminal except
//! for crossterm's key type, which `key` now replaces — a face maps its own
//! keystrokes into `key::KeyEvent` and gets the same editor.

pub mod block;
pub mod clip;
pub mod complete;
pub mod dupe;
pub mod edit;
pub mod emote;
pub mod heat;
pub mod hsbadge;
pub mod iso;
pub mod key;
pub mod layout;
pub mod mock;
pub mod paint;
pub mod proto;
pub mod room;
pub mod sanitize;
pub mod scroll;
pub mod slash;
pub mod social;
pub mod streamev;
pub mod vi;

use std::collections::VecDeque;

/// where a channel's chat comes from.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
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

/// what a tab shows. special tabs are virtual — fed by the app, not by joining
/// a channel — and pin left of the channel tabs in this order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TabKind {
    Channel,
    Mentions,
    Feed,
    Whispers,
}

impl TabKind {
    /// every virtual tab, in pin order.
    pub const SPECIAL: [TabKind; 3] = [TabKind::Mentions, TabKind::Feed, TabKind::Whispers];

    /// the tab's label and its config / slash-command name.
    pub fn name(self) -> &'static str {
        match self {
            TabKind::Channel => "channel",
            TabKind::Mentions => "mentions",
            TabKind::Feed => "feed",
            TabKind::Whispers => "whispers",
        }
    }

    pub fn parse(s: &str) -> Option<TabKind> {
        TabKind::SPECIAL
            .into_iter()
            .find(|k| k.name().eq_ignore_ascii_case(s.trim()))
    }

    pub fn is_special(self) -> bool {
        self != TabKind::Channel
    }
}

/// a leading label on a line: where it came from, or what kind of row it is.
#[derive(Clone, Debug, PartialEq)]
pub struct Tag {
    pub text: String,
    pub kind: TagKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TagKind {
    /// the source channel of a mention — dim.
    Origin,
    /// the line's platform, as `[T]` / `[K]` / `[Y]`, in the platform's hue.
    Platform,
    /// feed: a top-level post / the thread starter (magenta) / a reply.
    Op,
    ThreadOp,
    Reply,
    /// feed: the row's heat number, in its heat tier's look (text unused —
    /// the number is read off `Message::heat` so updates land in place).
    Heat,
    Whisper,
    Dm,
}

impl Tag {
    pub fn new(text: &str, kind: TagKind) -> Tag {
        Tag {
            text: text.to_string(),
            kind,
        }
    }

    /// `[T]` / `[K]` / `[Y]`.
    pub fn platform() -> Tag {
        Tag::new("", TagKind::Platform)
    }
}

impl Platform {
    /// one-letter label for line prefixes.
    pub fn letter(self) -> char {
        match self {
            Platform::Twitch => 'T',
            Platform::Kick => 'K',
            Platform::Youtube => 'Y',
        }
    }
}

/// a user role badge, normalized across platforms. twitch sends
/// what a badge is, normalized across platforms. twitch sends
/// `"broadcaster/1,moderator/1"`, kick sends `[{name, version}]` — both parse
/// into this one vocabulary (the extension's BADGE_SHORT set). a badge we
/// don't know keeps its first letter instead of vanishing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BadgeKind {
    Broadcaster,
    Moderator,
    Vip,
    Subscriber,
    Founder,
    Staff,
    GlobalMod,
    Verified,
    Og,
    Member,
    Premium,
    Turbo,
    Bits,
    Gifter,
    Predictions,
    Hype,
    Artist,
    Other(char),
}

/// a badge plus its version — for a subscriber, the months tier twitch sent.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Badge {
    pub kind: BadgeKind,
    pub ver: u16,
}

impl Badge {
    pub fn new(kind: BadgeKind) -> Badge {
        Badge { kind, ver: 0 }
    }

    /// a platform badge name (matched case-insensitively) + its version → our
    /// vocabulary. an unknown name falls back to its first letter; a nameless
    /// one is dropped.
    pub fn from_name(name: &str, ver: u16) -> Option<Badge> {
        use BadgeKind as K;
        let lower = name.to_ascii_lowercase();
        let kind = match lower.as_str() {
            "broadcaster" | "owner" => K::Broadcaster,
            "moderator" => K::Moderator,
            "vip" => K::Vip,
            "subscriber" => K::Subscriber,
            "founder" => K::Founder,
            "staff" | "admin" => K::Staff,
            "global_mod" => K::GlobalMod,
            "verified" | "partner" => K::Verified,
            "og" => K::Og,
            "member" => K::Member,
            "premium" => K::Premium,
            "turbo" => K::Turbo,
            "bits" | "bits-leader" | "anonymous-cheerer" => K::Bits,
            "sub-gifter" | "sub_gifter" | "sub-gift-leader" => K::Gifter,
            "predictions" => K::Predictions,
            "hype-train" => K::Hype,
            "artist" | "artist-badge" => K::Artist,
            other => K::Other(other.chars().next().filter(|c| c.is_alphanumeric())?),
        };
        Some(Badge { kind, ver })
    }

    /// single-cell glyph — badges must never widen a line by more than one
    /// column each.
    pub fn glyph(self) -> char {
        use BadgeKind as K;
        match self.kind {
            K::Broadcaster => 'B',
            K::Moderator => 'M',
            K::Vip => 'V',
            K::Subscriber => 'S',
            K::Founder => 'F',
            K::Staff => 'A',
            K::GlobalMod => 'G',
            K::Verified => '✓',
            K::Og => 'O',
            K::Member => 'm',
            K::Premium => 'P',
            K::Turbo => 'T',
            K::Bits => '◆',
            K::Gifter => '✦',
            K::Predictions => '%',
            K::Hype => 'H',
            K::Artist => 'a',
            K::Other(c) => c.to_ascii_uppercase(),
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
    /// a streamer announcement, tinted by the color they picked.
    Announce(Tint),
    /// watch streak shared in chat.
    Streak,
    /// viewer milestone.
    Milestone,
    /// charity donation.
    Charity,
    /// a mod's anniversary (twitch modiversary).
    ModAnniv,
    /// a first-time chatter's welcome (ritual / new_chatter).
    Ritual,
    /// room mode change: slow, emote-only, followers-only, sub-only.
    Mode,
}

/// an announcement's color, mapped onto the ansi palette: twitch's PRIMARY is
/// the plain one, ORANGE reads as yellow, PURPLE as magenta.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tint {
    Plain,
    Blue,
    Green,
    Yellow,
    Magenta,
}

impl Tint {
    /// twitch's `msg-param-color` → tint; anything unknown stays plain.
    pub fn from_name(s: &str) -> Tint {
        match s.to_ascii_uppercase().as_str() {
            "BLUE" => Tint::Blue,
            "GREEN" => Tint::Green,
            "ORANGE" => Tint::Yellow,
            "PURPLE" => Tint::Magenta,
            _ => Tint::Plain,
        }
    }
}

/// per-line extras that change how a chat line looks, grouped so a new face
/// only has to carry one field.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Extra {
    /// the text of the message being replied to (sanitized, may be empty).
    pub reply_text: String,
    /// `/me` — rendered in the user's color, italic.
    pub action: bool,
    /// the user's first message in this channel.
    pub first: bool,
    /// redeemed "highlight my message" — rendered reverse-video.
    pub highlight: bool,
    /// wall-clock seconds when the line landed (0 = unknown) — for `HH:MM`.
    pub ts: u64,
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
    /// the sender's heatsync paint-space id (twitch user id, `kick_<id>`,
    /// `yt_<UC id>`), when the relay sent one — keys name paints and colors.
    pub uid: Option<String>,
    pub extra: Extra,
    /// this line's own emote refs (see [`emote::segments_with`]), built once
    /// when the line lands; None for lines that carry none (history, system).
    pub emotes: Option<std::sync::Arc<emote::EmoteSet>>,
    /// leading labels (mention origin, feed `[OP]`, `[whisper]`…); empty on
    /// ordinary chat.
    pub tags: Vec<Tag>,
    pub heat: f64,
}

/// a channel's pinned message — one line in a bar above chat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pin {
    pub id: String,
    pub user: String,
    pub text: String,
}

impl Message {
    /// the per-message ref set, empty when the line carries none.
    pub fn refs(&self) -> &emote::EmoteSet {
        self.emotes.as_deref().unwrap_or(emote::EmoteSet::none())
    }
}

/// live state for a single channel. bounded ring buffer — a raid can flood
/// forever and we never grow past `cap` (mele is 8GB; resource-conscious).
/// heat is a decaying counter driven by message arrivals.
#[derive(Clone, Debug)]
pub struct Channel {
    /// a real channel, or one of the virtual tabs (see [`TabKind`]).
    pub kind: TabKind,
    pub name: String,
    /// primary source — sends target it, and it leads the tab label.
    pub platform: Platform,
    /// further sources merged into this tab (a `+`-joined channel). their
    /// lines interleave into the one ring in arrival order.
    pub extra: Vec<(Platform, String)>,
    /// the unsaved "stream i'm looking at" tab: one slot, never persisted or
    /// shared, promoted to a saved tab by keeping it.
    pub temp: bool,
    pub heat: f64,
    /// wall-clock ms of the last heat update — decay is computed against it.
    pub last_ms: u64,
    pub messages: VecDeque<Message>,
    /// live lines recorded since the tab was last on screen.
    unread: u32,
    /// one of them pinged the user.
    pinged: bool,
    /// one of them was a stream event.
    evented: bool,
    /// the channel's pinned message, while it has one.
    pin: Option<Pin>,
    /// id of the pin the user dismissed — it stays hidden until a new one lands.
    pin_dismissed: Option<String>,
    /// lines ever appended at the live end — a monotonic clock for "how many
    /// arrived since", unaffected by the ring evicting old ones.
    pub seq: u64,
    cap: usize,
}

/// what a tab has waiting, for its color: nothing, chat, a stream event, or a ping.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unread {
    None,
    Chat,
    /// a stream event (went live / offline / game switch) landed unseen.
    Event,
    Ping,
}

impl Channel {
    pub fn new(name: &str, platform: Platform, cap: usize) -> Channel {
        Channel {
            kind: TabKind::Channel,
            name: name.to_string(),
            platform,
            extra: Vec::new(),
            temp: false,
            heat: 0.0,
            last_ms: 0,
            messages: VecDeque::with_capacity(cap),
            unread: 0,
            pinged: false,
            evented: false,
            pin: None,
            pin_dismissed: None,
            seq: 0,
            cap,
        }
    }

    /// a virtual tab. it joins nothing: `subs()` is empty, so no line ever
    /// matches it and no part / emote / history call is made for it.
    pub fn special(kind: TabKind, cap: usize) -> Channel {
        Channel {
            kind,
            ..Channel::new(kind.name(), Platform::Twitch, cap)
        }
    }

    /// append a line to a virtual tab: counts as unread, but keeps the row's
    /// own heat (a feed row's heat is the server's, not ours).
    pub fn deliver(&mut self, msg: Message) {
        if self.messages.len() == self.cap {
            self.messages.pop_front();
        }
        self.messages.push_back(msg);
        self.seq += 1;
        self.unread = self.unread.saturating_add(1);
    }

    /// the tab's unread state.
    pub fn unread(&self) -> Unread {
        if self.pinged {
            Unread::Ping
        } else if self.evented {
            Unread::Event
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

    /// a pin arrived (`Some`) or was cleared (`None`).
    pub fn set_pin(&mut self, pin: Option<Pin>) {
        self.pin = pin;
    }

    /// the pin to show, unless the user dismissed this very one.
    pub fn visible_pin(&self) -> Option<&Pin> {
        self.pin
            .as_ref()
            .filter(|p| self.pin_dismissed.as_deref() != Some(p.id.as_str()))
    }

    /// hide the current pin (a newer one will show again). false = none shown.
    pub fn dismiss_pin(&mut self) -> bool {
        let id = self.visible_pin().map(|p| p.id.clone());
        let some = id.is_some();
        if some {
            self.pin_dismissed = id;
        }
        some
    }

    /// flag the tab as holding an unseen stream event.
    pub fn flag_event(&mut self) {
        self.evented = true;
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

    /// slot a line in by its own time (`extra.ts`, seconds) — a replayed or
    /// back-dated event, not news: no heat, no unread, no ping. it lands after
    /// the last line that is not newer than it. false = it would sit older than
    /// everything in a full ring, so it is dropped.
    pub fn insert_by_ts(&mut self, msg: Message) -> bool {
        let at = self
            .messages
            .iter()
            .rposition(|m| m.extra.ts <= msg.extra.ts)
            .map_or(0, |i| i + 1);
        if self.messages.len() == self.cap {
            if at == 0 {
                return false;
            }
            self.messages.pop_front();
            self.messages.insert(at - 1, msg);
        } else {
            self.messages.insert(at, msg);
        }
        true
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
        self.evented = false;
    }

    /// every source feeding this tab, primary first.
    pub fn subs(&self) -> impl Iterator<Item = (Platform, &str)> {
        // a virtual tab has no sources — it must never match a line or be parted.
        let own = (self.kind == TabKind::Channel).then_some((self.platform, self.name.as_str()));
        own.into_iter()
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
        c.flag_event();
        assert_eq!(c.unread(), Unread::Event);
        c.record(msg("a", "again"), 1);
        assert_eq!(
            c.unread(),
            Unread::Event,
            "chat does not downgrade an event"
        );
        c.ping();
        assert_eq!(c.unread(), Unread::Ping);
        c.record(msg("b", "yo"), 2);
        assert_eq!(
            c.unread(),
            Unread::Ping,
            "a later plain line does not downgrade a ping"
        );
        c.mark_seen();
        assert_eq!(c.unread(), Unread::None);
    }

    #[test]
    fn a_dismissed_pin_stays_hidden_until_a_new_one_lands() {
        let pin = |id: &str| Pin {
            id: id.into(),
            user: "u".into(),
            text: "t".into(),
        };
        let mut c = Channel::new("x", Platform::Twitch, 8);
        assert!(!c.dismiss_pin(), "nothing to dismiss");
        c.set_pin(Some(pin("a")));
        assert_eq!(c.visible_pin().map(|p| p.id.as_str()), Some("a"));
        assert!(c.dismiss_pin());
        assert!(c.visible_pin().is_none());
        c.set_pin(Some(pin("a")));
        assert!(c.visible_pin().is_none(), "same pin re-sent stays hidden");
        c.set_pin(Some(pin("b")));
        assert!(c.visible_pin().is_some());
        c.set_pin(None);
        assert!(c.visible_pin().is_none());
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
        assert_eq!(
            c.apply(&del(proto::Target::Msg("id1".into()), "deleted")),
            1
        );
        assert_eq!(gone_after(&c), vec![None, Some("deleted".into()), None]);
        assert_eq!(
            c.apply(&del(proto::Target::Msg("id1".into()), "deleted")),
            0,
            "already marked"
        );
    }

    #[test]
    fn timeout_marks_all_of_a_users_chat_lines_not_events() {
        let mut c = Channel::new("x", Platform::Twitch, 8);
        c.record(msg("Troll", "a"), 1);
        c.record(msg("other", "b"), 1);
        let mut ev = msg("troll", "");
        ev.note = Some(Note {
            kind: NoteKind::Sub,
            what: "subbed".into(),
        });
        c.record(ev, 1);
        c.record(msg("TROLL", "c"), 1);
        assert_eq!(
            c.apply(&del(proto::Target::User("troll".into()), "timed out 10m")),
            2
        );
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
            extra: Default::default(),
            emotes: None,
            tags: Vec::new(),
            heat: 0.0,
            uid: None,
        }
    }

    #[test]
    fn insert_by_ts_lands_in_time_order_without_heat_or_unread() {
        let at = |t: u64, text: &str| {
            let mut m = msg("u", text);
            m.extra.ts = t;
            m
        };
        let mut ch = Channel::new("c", Platform::Twitch, 3);
        ch.system(at(10, "a"));
        ch.system(at(30, "c"));
        assert!(ch.insert_by_ts(at(20, "b")));
        let texts = |ch: &Channel| {
            ch.messages
                .iter()
                .map(|m| m.text.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(texts(&ch), ["a", "b", "c"]);
        assert_eq!((ch.heat, ch.unread()), (0.0, Unread::None));
        // full ring: older than everything is dropped, anything else evicts the oldest
        assert!(!ch.insert_by_ts(at(5, "old")));
        assert!(ch.insert_by_ts(at(25, "mid")));
        assert_eq!(texts(&ch), ["b", "mid", "c"]);
        assert!(ch.insert_by_ts(at(40, "new")));
        assert_eq!(texts(&ch), ["mid", "c", "new"]);
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

    #[test]
    fn special_tabs_join_nothing_and_match_nothing() {
        let mut c = Channel::special(TabKind::Mentions, 10);
        assert_eq!(c.kind, TabKind::Mentions);
        assert_eq!(c.subs().count(), 0);
        assert!(!c.matches(Platform::Twitch, "mentions")); // not even its own name
        let mut m = msg("bob", "hey");
        m.heat = 42.0;
        c.deliver(m);
        assert_eq!(c.messages[0].heat, 42.0, "row keeps its own heat");
        assert_eq!(c.heat, 0.0);
        assert_eq!(c.unread(), Unread::Chat);
        c.ping();
        assert_eq!(c.unread(), Unread::Ping);
    }

    #[test]
    fn tab_kinds_parse_and_pin_in_order() {
        assert_eq!(TabKind::parse("Feed"), Some(TabKind::Feed));
        assert_eq!(TabKind::parse("xqc"), None);
        assert_eq!(TabKind::parse("channel"), None);
        assert!(TabKind::Mentions < TabKind::Feed && TabKind::Feed < TabKind::Whispers);
        assert!(!TabKind::Channel.is_special());
    }
}
