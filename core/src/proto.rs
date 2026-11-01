//! HeatSync `/ws` relay protocol — parse inbound frames, build outbound ones.
//! anon read is allowed, so no auth is required to consume chat. content and
//! usernames are sanitized HERE, at the trust boundary, before anything else
//! touches them (terminal-injection defense).

use serde_json::{json, Value};

use crate::emote::{self, Emote};
use crate::{sanitize, Badge, Extra, Note, NoteKind, Pin, Platform, TabKind, Tag, TagKind, Tint};

/// a normalized chat line, ready for a channel buffer.
#[derive(Clone, Debug, PartialEq)]
pub struct ChatLine {
    pub platform: Platform,
    pub channel: String,
    /// the platform's own message id — what a later delete points at.
    pub id: Option<String>,
    pub user: String,
    pub color: Option<String>,
    pub badges: Vec<Badge>,
    pub reply_to: Option<String>,
    /// set when this line is an event notice, not plain chat.
    pub note: Option<Note>,
    /// sender id in paint space (see `Message::uid`).
    pub uid: Option<String>,
    pub extra: Extra,
    /// raw stream lifecycle facts behind a go-live / offline / game-switch
    /// note — what `streamev` pairs and dedupes on.
    pub stream: Option<StreamEv>,
    pub content: String,
    /// this message's own emote refs (relay `hsEmotes`, twitch `emotes` tag,
    /// kick `[emote:id:name]` tokens) — highest priority when parsing, see
    /// [`emote::segments_with`].
    pub emotes: Vec<Emote>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamKind {
    Online,
    Offline,
    Update,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StreamEv {
    pub kind: StreamKind,
    pub game: String,
    pub prev_game: String,
    /// from a followed-channel push (`follow:stream:*`), not a joined room.
    pub follow: bool,
}

/// a line bound for a virtual tab: the row itself plus its labels. `heat` is
/// the server's number (feed rows only).
#[derive(Clone, Debug, PartialEq)]
pub struct SpecialLine {
    pub tab: TabKind,
    pub line: ChatLine,
    pub tags: Vec<Tag>,
    pub heat: f64,
    /// who a whisper / dm came from, with the id a reply needs.
    pub peer: Option<crate::social::Contact>,
}

/// whose lines a moderation event removes.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// one message, by platform id.
    Msg(String),
    /// every line from one user (login or display name, case-insensitive).
    User(String),
    /// the whole channel.
    All,
}

/// a moderation event: the lines stay on screen, struck through and tagged
/// with `label` ("deleted", "timed out 10m", "banned", "cleared").
#[derive(Clone, Debug, PartialEq)]
pub struct Delete {
    pub platform: Platform,
    /// the tab's channel; empty = every channel of `platform` (youtube's
    /// delete frame is keyed by message id alone).
    pub channel: String,
    pub target: Target,
    pub label: String,
}

/// what an inbound frame turned into.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Chat(ChatLine),
    Backfill(Vec<ChatLine>),
    /// lines removed by a mod, plus the mod-log note to show (clearchat has
    /// one; a single deleted message doesn't).
    Delete {
        line: Option<ChatLine>,
        del: Delete,
    },
    /// a mention-rule hit, whisper, DM or feed row for a virtual tab.
    Special(SpecialLine),
    /// a feed row's heat moved (`heat:updated`).
    FeedHeat {
        id: String,
        heat: f64,
    },
    /// a feed row was hidden by moderation (`message:hidden`).
    FeedHidden {
        id: String,
    },
    /// several events from one frame, in order.
    Batch(Vec<Event>),
    /// authentication outcome (true = ok).
    Auth(bool),
    /// result of an outbound send: ok, or an error reason.
    SendResult {
        ok: bool,
        error: Option<String>,
    },
    /// followed channels already live at connect (`follow:live:snapshot`) —
    /// catch-up, never lines.
    FollowLive(Vec<(Platform, String)>),
    /// the user's last day of follow events (`follow:history`), oldest first:
    /// each a follow stream line plus its unix-ms time.
    FollowHistory(Vec<(ChatLine, u64)>),
    /// `follow:colors`: channel login (lowercase) → `#rrggbb`.
    FollowColors(Vec<(String, String)>),
    /// a channel pin was set (`Some`) or cleared (`None`).
    Pin(Platform, String, Option<Pin>),
    /// a twitch ROOMSTATE: the channel's current chat modes.
    Room(String, RoomState),
    Pong,
    /// `profile:color`: a user's picked name color, by paint-space id.
    Color {
        /// paint-space id, when the push named one.
        id: Option<String>,
        /// the user's linked usernames, lowercased (platform not stated).
        names: Vec<String>,
        color: String,
    },
    /// `cosmetic:changed`: paints changed for these paint-space ids.
    Cosmetic(Vec<String>),
    Ignore,
}

/// a channel's chat modes as one ROOMSTATE carried them: `None` = the tag was
/// absent, i.e. unchanged (twitch's later ROOMSTATEs list only what changed).
/// `followers` is minutes (-1 = off, 0 = any follower).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RoomState {
    pub slow: Option<u32>,
    pub followers: Option<i32>,
    pub emote_only: Option<bool>,
    pub subs_only: Option<bool>,
}

/// parse one inbound text frame. unknown / malformed frames become `Ignore`
/// rather than erroring — a hostile or new server can't crash the client.
pub fn parse(raw: &str) -> Event {
    let v: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return Event::Ignore,
    };
    match v.get("type").and_then(Value::as_str).unwrap_or("") {
        // the irc:message frame multiplexes every twitch IRC record type —
        // chat, usernotice (subs/gifts/raids/announcements), moderation.
        // roomstate/clearmsg are state we don't surface as lines.
        "irc:message" if is_irc(&v, "clearmsg") => clearmsg_event(&v),
        "irc:message" if is_irc(&v, "clearchat") => clearchat_event(&v),
        "irc:message" if is_irc(&v, "roomstate") => roomstate_event(&v),
        "irc:message" => v
            .get("message")
            .and_then(|m| match m.get("type").and_then(Value::as_str) {
                Some("usernotice") => usernotice_line(m, v.get("channel")),
                Some("notice") => notice_line(m, v.get("channel")),
                Some("privmsg") | None => line_from(m, Platform::Twitch, v.get("channel")),
                _ => None,
            })
            .map(Event::Chat)
            .unwrap_or(Event::Ignore),
        // stream lifecycle + hype + redemptions, all platforms (the frame
        // carries its platform). twitch stream:sub / stream:sub-gift are
        // deliberately NOT parsed — the IRC usernotice already covers every
        // channel, and connected broadcasters would double-post.
        "stream:online" | "stream:offline" | "stream:update" | "stream:raid" | "stream:redeem"
        | "stream:hype-start" | "stream:hype-end" | "moment:spike" => {
            stream_line(&v).map(Event::Chat).unwrap_or(Event::Ignore)
        }
        "follow:stream:online" | "follow:stream:offline" | "follow:stream:update" => {
            stream_line(&v).map(Event::Chat).unwrap_or(Event::Ignore)
        }
        "follow:history" => follow_history(&v),
        "follow:colors" => follow_colors(&v),
        "pin:set" | "pin:clear" => pin_event(&v),
        "follow:live:snapshot" => follow_snapshot(&v),
        "kick-sub-event" => kick_sub_line(&v).map(Event::Chat).unwrap_or(Event::Ignore),
        "kick-kicks-event" => kick_kicks_line(&v)
            .map(Event::Chat)
            .unwrap_or(Event::Ignore),
        "youtube:status" => yt_status_line(&v).map(Event::Chat).unwrap_or(Event::Ignore),
        "kick-chat-message" => v
            .get("data")
            .and_then(|d| line_from(d, Platform::Kick, d.get("channel")))
            .map(Event::Chat)
            .unwrap_or(Event::Ignore),
        "kick-chat-backfill" => {
            let lines = v
                .get("messages")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|d| line_from(d, Platform::Kick, d.get("channel")))
                        .collect()
                })
                .unwrap_or_default();
            Event::Backfill(lines)
        }
        // youtube arrives batched — live and replay share one frame type, so a
        // batch is always a Backfill (the receiver records lines in order).
        "youtube:chat" => {
            let video = v.get("videoId").and_then(Value::as_str).unwrap_or("");
            let lines = v
                .get("messages")
                .and_then(Value::as_array)
                .map(|arr| arr.iter().filter_map(|m| yt_line(m, video)).collect())
                .unwrap_or_default();
            Event::Backfill(lines)
        }
        "youtube:delete" => youtube_delete_event(&v),
        "mention:rule-match" => mention_event(&v),
        "whisper:new" => whisper_event(&v, TabKind::Whispers),
        "dm:new" => whisper_event(&v, TabKind::Whispers),
        "new-message" => feed_row(&v).map(Event::Special).unwrap_or(Event::Ignore),
        "heat:updated" => match (
            str_or_num(&v, &["messageId", "message_id", "id"]),
            v.get("newHeat").and_then(Value::as_f64),
        ) {
            (Some(id), Some(heat)) => Event::FeedHeat { id, heat },
            _ => Event::Ignore,
        },
        "message:hidden" => match str_or_num(&v, &["id", "messageId"]) {
            Some(id) => Event::FeedHidden { id },
            None => Event::Ignore,
        },
        "authenticated" => Event::Auth(true),
        "authentication_failed" => Event::Auth(false),
        "chat:send_kick_result" | "chat:send_youtube_result" => Event::SendResult {
            ok: v.get("ok").and_then(Value::as_bool).unwrap_or(false),
            error: v
                .get("error")
                .and_then(Value::as_str)
                .map(|s| s.to_string()),
        },
        "pong" => Event::Pong,
        // a user's name color changed on heatsync (paint space id).
        "profile:color" => {
            let id = v
                .get("userId")
                .and_then(Value::as_str)
                .and_then(|s| paint_id("", s));
            // bounded: a hostile frame can't make us hold a wall of names
            let names: Vec<String> = v
                .get("usernames")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .filter(|s| !s.is_empty() && s.len() <= 64)
                        .map(|s| sanitize::clean(s).to_lowercase())
                        .take(16)
                        .collect()
                })
                .unwrap_or_default();
            match v.get("color").and_then(color_str) {
                Some(color) if id.is_some() || !names.is_empty() => Event::Color {
                    id,
                    names,
                    color: color.to_string(),
                },
                _ => Event::Ignore,
            }
        }
        // paints / badges changed for these ids — refetch what's cached.
        "cosmetic:changed" => Event::Cosmetic(
            v.get("ids")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .filter_map(|s| paint_id("", s))
                        .collect()
                })
                .unwrap_or_default(),
        ),
        _ => Event::Ignore,
    }
}

/// pull a ChatLine out of a message object. drops lines with empty user+text.
fn line_from(m: &Value, platform: Platform, channel: Option<&Value>) -> Option<ChatLine> {
    let user_raw = m
        .get("displayName")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| m.get("username").and_then(Value::as_str))
        .unwrap_or("");
    let content_raw = m.get("content").and_then(Value::as_str).unwrap_or("");
    // native emote positions / tokens read the RAW text (positions are in the
    // sender's code points), before sanitizing shifts anything.
    let (content_raw, emotes) = msg_emotes(m, platform, content_raw);
    let user = sanitize::clean(user_raw);
    let content = sanitize::clean(&content_raw);
    if user.is_empty() && content.is_empty() {
        return None;
    }
    let color = color_of(m);
    let channel = channel.and_then(Value::as_str).unwrap_or("").to_string();
    // a chat line can still be an event: cheered bits, a reward redemption
    // with text, a highlighted message. the note marks it, the text stays chat.
    let note = if let Some(bits) = m.get("bits").and_then(Value::as_u64).filter(|b| *b > 0) {
        Some(Note {
            kind: NoteKind::Cheer,
            what: format!("cheered {bits} bits"),
        })
    } else if m
        .get("isRedemption")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        Some(Note {
            kind: NoteKind::Redeem,
            what: "redeemed".into(),
        })
    } else {
        None
    };
    Some(ChatLine {
        platform,
        channel,
        id: id_of(m),
        user,
        color,
        badges: badges_from(m.get("badges")),
        reply_to: m
            .get("replyTo")
            .and_then(|r| r.get("username"))
            .and_then(Value::as_str)
            .map(sanitize::clean)
            .filter(|s| !s.is_empty()),
        note,
        extra: extra_of(m),
        stream: None,
        content,
        uid: uid_of(m, platform),
        emotes,
    })
}

/// a `{name: {url, provider, zeroWidth}}` ref map → refs (relay `hsEmotes`,
/// feed `emote_refs`). cw stubs ({name, cw}, no url) carry nothing to draw —
/// the word stays text rather than leak the hidden image.
fn refs_from(v: Option<&Value>) -> Vec<Emote> {
    let Some(map) = v.and_then(Value::as_object) else {
        return Vec::new();
    };
    map.iter()
        .take(MAX_REFS)
        .filter_map(|(name, r)| {
            let url = r.get("url").and_then(Value::as_str)?;
            let provider = r
                .get("provider")
                .and_then(Value::as_str)
                .filter(|p| !p.is_empty())
                .unwrap_or("heatsync");
            let zw = r.get("zeroWidth").and_then(Value::as_bool).unwrap_or(false);
            emote::msg_ref(name, url, provider, zw)
        })
        .collect()
}

/// most refs one message may carry — a hostile relay can't make us hold more.
const MAX_REFS: usize = 64;

/// a message's emote refs, plus its text (kick tokens are rewritten to names).
/// order is priority: the relay's `hsEmotes` (the sender's inventory emotes,
/// resolved server-side) first, then the platform's native ids.
fn msg_emotes(m: &Value, platform: Platform, content: &str) -> (String, Vec<Emote>) {
    let mut refs: Vec<Emote> = Vec::new();
    let mut add = |e: Emote| {
        if refs.len() < MAX_REFS && !refs.iter().any(|r| r.name == e.name) {
            refs.push(e);
        }
    };
    for e in refs_from(m.get("hsEmotes")) {
        add(e);
    }
    let mut text = content.to_string();
    match platform {
        Platform::Twitch => {
            if let Some(tag) = m.get("emotes").and_then(Value::as_str) {
                for e in emote::twitch_native(tag, content) {
                    add(e);
                }
            }
            // relays that pre-resolve the tag send name → url instead
            if let Some(map) = m.get("twitchEmotes").and_then(Value::as_object) {
                for (name, url) in map.iter().take(MAX_REFS) {
                    if let Some(e) = url
                        .as_str()
                        .and_then(|u| emote::msg_ref(name, u, "twitch", false))
                    {
                        add(e);
                    }
                }
            }
        }
        Platform::Kick => {
            let (rewritten, native) = emote::kick_native(content);
            text = rewritten;
            for e in native {
                add(e);
            }
        }
        _ => {}
    }
    (text, refs)
}

/// a `#rrggbb`/`#rgb` user color, if the object carries a valid one.
fn color_of(m: &Value) -> Option<String> {
    m.get("color")
        .and_then(Value::as_str)
        .filter(|s| s.starts_with('#') && (s.len() == 7 || s.len() == 4))
        .map(|s| s.to_string())
}

/// a `#rrggbb` string.
fn color_str(v: &Value) -> Option<&str> {
    v.as_str().filter(|s| {
        s.len() == 7 && s.starts_with('#') && s[1..].bytes().all(|b| b.is_ascii_hexdigit())
    })
}

/// a paint-space id: `prefix` + a safe token (the server's id charset), or
/// `None` for anything that isn't one.
fn paint_id(prefix: &str, raw: &str) -> Option<String> {
    let ok = !raw.is_empty()
        && raw.len() <= 64
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    ok.then(|| format!("{prefix}{raw}"))
}

/// the sender's paint-space id: twitch's bare user id, kick's `kick_<id>`
/// (kick ids overlap twitch's range, so they must never go bare). the relay
/// sends `userId`; kick frames may name it `senderId`.
fn uid_of(m: &Value, platform: Platform) -> Option<String> {
    let id = ["userId", "senderId"]
        .iter()
        .find_map(|k| match m.get(*k)? {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })?;
    match platform {
        Platform::Twitch => paint_id("", &id),
        Platform::Kick => paint_id("kick_", &id),
        Platform::Youtube => None,
    }
}

/// reply snippet + action / first / highlight flags off a message object.
fn extra_of(m: &Value) -> Extra {
    let flag = |k: &str| m.get(k).and_then(Value::as_bool).unwrap_or(false);
    Extra {
        reply_text: m
            .get("replyTo")
            .map(|r| field(r, "content"))
            .unwrap_or_default(),
        action: flag("isAction"),
        first: flag("isFirstMsg"),
        highlight: flag("isHighlighted"),
        ts: 0,
    }
}

/// sanitized string field, empty when absent.
pub(crate) fn field(v: &Value, k: &str) -> String {
    sanitize::clean(v.get(k).and_then(Value::as_str).unwrap_or(""))
}

/// platform system messages open with the actor's name ("RealCK3 subscribed
/// at Tier 1…"). strip it so the colored username isn't printed twice; a
/// headline that doesn't open with the actor is self-contained (raid counts,
/// mystery-gift totals), so the separate actor is dropped instead.
fn split_headline(user: &mut String, what: &str) -> String {
    if user.is_empty() || what.is_empty() {
        return what.to_string();
    }
    if let Some(rest) = what.strip_prefix(user.as_str()) {
        // only a word boundary counts — user "a" must not eat "announcement".
        if rest.chars().next().is_some_and(|c| !c.is_alphanumeric()) {
            let rest = rest.trim_start_matches(':').trim_start();
            if !rest.is_empty() {
                return rest.to_string();
            }
        }
    }
    user.clear();
    what.to_string()
}

/// terse duration for timeouts: 45s, 10m, 2h, 1d.
pub(crate) fn fmt_secs(s: u64) -> String {
    if s >= 86_400 && s.is_multiple_of(86_400) {
        format!("{}d", s / 86_400)
    } else if s >= 3_600 && s.is_multiple_of(3_600) {
        format!("{}h", s / 3_600)
    } else if s >= 60 && s.is_multiple_of(60) {
        format!("{}m", s / 60)
    } else {
        format!("{s}s")
    }
}

/// a bare note line — no chat text, just the event.
pub(crate) fn note_line(
    platform: Platform,
    channel: String,
    user: String,
    color: Option<String>,
    kind: NoteKind,
    what: String,
    content: String,
) -> Option<ChatLine> {
    if what.is_empty() && content.is_empty() {
        return None;
    }
    Some(ChatLine {
        platform,
        channel,
        id: None,
        user,
        color,
        badges: Vec::new(),
        reply_to: None,
        note: Some(Note { kind, what }),
        extra: Extra::default(),
        stream: None,
        content,
        uid: None,
        emotes: Vec::new(),
    })
}

/// twitch USERNOTICE → inline event. the system message is the headline;
/// `content` (a resub message) stays chat text under it.
fn usernotice_line(m: &Value, channel: Option<&Value>) -> Option<ChatLine> {
    let sub = m.get("subType").and_then(Value::as_str).unwrap_or("");
    let kind = match sub {
        "sub" | "resub" => NoteKind::Sub,
        "subgift"
        | "submysterygift"
        | "giftpaidupgrade"
        | "primepaidupgrade"
        | "standardpayforward"
        | "communitypayforward" => NoteKind::Gift,
        "raid" | "unraid" => NoteKind::Raid,
        "announcement" => NoteKind::Announce(Tint::from_name(&field(m, "announceColor"))),
        "watchstreak" => NoteKind::Streak,
        "viewermilestone" => NoteKind::Milestone,
        "charitydonation" | "charity_donation" => NoteKind::Charity,
        "ritual" | "new_chatter" => NoteKind::Ritual,
        _ => NoteKind::Notice, // future subtypes
    };
    let mut user = {
        let d = field(m, "displayName");
        if d.is_empty() {
            field(m, "username")
        } else {
            d
        }
    };
    let text = field(m, "content");
    let sys = field(m, "systemMessage");
    let mut what = split_headline(&mut user, &sys);
    if what.is_empty() && sub == "announcement" {
        what = "announced".into();
    }
    let channel = channel.and_then(Value::as_str).unwrap_or("").to_string();
    let color = color_of(m);
    note_line(Platform::Twitch, channel, user, color, kind, what, text)
}

/// a pin object (`pin:set`'s `pin`, or `/api/pin`'s) → [`Pin`]; none without text.
pub fn pin_from(p: &Value) -> Option<Pin> {
    let user = {
        let d = field(p, "display_name");
        if d.is_empty() {
            field(p, "username")
        } else {
            d
        }
    };
    let text = field(p, "content");
    (!text.is_empty()).then(|| Pin {
        id: field(p, "id"),
        user,
        text,
    })
}

fn platform_of(s: &str) -> Option<Platform> {
    match s {
        "twitch" => Some(Platform::Twitch),
        "kick" => Some(Platform::Kick),
        "youtube" => Some(Platform::Youtube),
        _ => None,
    }
}

fn pin_event(v: &Value) -> Event {
    let (Some(p), ch) = (platform_of(&field(v, "platform")), field(v, "channel")) else {
        return Event::Ignore;
    };
    if ch.is_empty() {
        return Event::Ignore;
    }
    match v.get("type").and_then(Value::as_str) {
        Some("pin:set") => match v.get("pin").and_then(pin_from) {
            Some(pin) => Event::Pin(p, ch, Some(pin)),
            None => Event::Ignore,
        },
        _ => Event::Pin(p, ch, None),
    }
}

fn roomstate_event(v: &Value) -> Event {
    let (Some(m), Some(ch)) = (v.get("message"), v.get("channel").and_then(Value::as_str)) else {
        return Event::Ignore;
    };
    let flag = |k: &str| m.get(k).and_then(Value::as_bool);
    let n = |k: &str| m.get(k).and_then(Value::as_i64);
    Event::Room(
        ch.to_string(),
        RoomState {
            slow: n("slow").map(|x| x.clamp(0, u32::MAX as i64) as u32),
            followers: n("followersOnly").map(|x| x.clamp(-1, i32::MAX as i64) as i32),
            emote_only: flag("emoteOnly"),
            subs_only: flag("subsOnly"),
        },
    )
}

/// first present key as a string (numbers allowed — ids arrive either way).
pub(crate) fn str_or_num(v: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| match v.get(*k)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    })
}

pub(crate) fn platform_or_twitch(s: &str) -> Platform {
    match s {
        "kick" => Platform::Kick,
        "youtube" | "yt" => Platform::Youtube,
        _ => Platform::Twitch,
    }
}

/// a bare chat line for a virtual tab.
pub(crate) fn special_chat(
    platform: Platform,
    channel: String,
    id: Option<String>,
    user: String,
    color: Option<String>,
    content: String,
    emotes: Vec<Emote>,
) -> ChatLine {
    ChatLine {
        platform,
        channel,
        id,
        user,
        color,
        badges: Vec::new(),
        reply_to: None,
        note: None,
        content,
        emotes,
        stream: None,
        extra: Extra::default(),
        uid: None,
    }
}

/// `mention:rule-match` {ruleId, pattern, channel, platform, username, snippet,
/// url} → a mentions-tab line. the url is the server's deep link; we have no
/// use for it in a terminal.
fn mention_event(v: &Value) -> Event {
    let platform = platform_or_twitch(v.get("platform").and_then(Value::as_str).unwrap_or(""));
    let channel = field(v, "channel");
    let user = field(v, "username");
    let snippet = field(v, "snippet");
    if user.is_empty() && snippet.is_empty() {
        return Event::Ignore;
    }
    let tags = vec![
        Tag::platform(),
        Tag::new(&format!("[{channel}]"), TagKind::Origin),
    ];
    Event::Special(SpecialLine {
        tab: TabKind::Mentions,
        line: special_chat(platform, channel, None, user, None, snippet, Vec::new()),
        tags,
        heat: 0.0,
        peer: None,
    })
}

/// `whisper:new` {from_twitch_login, from_display_name, from_color,
/// to_twitch_login, content, created_at} and `dm:new` {from_display_name,
/// from_color, content, created_at, from_user_id}. the frame type picks the
/// label; both land in the whispers tab.
fn whisper_event(v: &Value, tab: TabKind) -> Event {
    let is_dm = v.get("type").and_then(Value::as_str) == Some("dm:new");
    let name = {
        let d = field(v, "from_display_name");
        if d.is_empty() {
            field(v, "from_twitch_login")
        } else {
            d
        }
    };
    let content = field(v, "content");
    if name.is_empty() && content.is_empty() {
        return Event::Ignore;
    }
    let color = v
        .get("from_color")
        .and_then(Value::as_str)
        .filter(|s| s.starts_with('#') && (s.len() == 7 || s.len() == 4))
        .map(str::to_string);
    let peer = if is_dm {
        str_or_num(v, &["from_user_id"])
    } else {
        str_or_num(v, &["from_twitch_id"])
    }
    .and_then(|id| {
        let login = match field(v, "from_twitch_login") {
            l if l.is_empty() => name.clone(),
            l => l,
        };
        crate::social::contact(name.clone(), &login, id, is_dm)
    });
    let tag = if is_dm {
        Tag::new("[DM]", TagKind::Dm)
    } else {
        Tag::new("[whisper]", TagKind::Whisper)
    };
    Event::Special(SpecialLine {
        tab,
        line: special_chat(
            Platform::Twitch,
            String::new(),
            None,
            name,
            color,
            content,
            Vec::new(),
        ),
        tags: vec![tag],
        heat: 0.0,
        peer,
    })
}

/// the heatsync feed's `new-message` row (flat payload; `GET /api/messages`
/// rows share the shape): base36_id, content, username / display_name,
/// user_color, platform, heat, is_op, is_thread_op, emote_refs.
pub fn feed_row(v: &Value) -> Option<SpecialLine> {
    let id = str_or_num(v, &["base36_id", "id"])?;
    let user = {
        let d = field(v, "display_name");
        if d.is_empty() {
            field(v, "username")
        } else {
            d
        }
    };
    let content = field(v, "content");
    if user.is_empty() && content.is_empty() {
        return None;
    }
    let flag = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
    let role = if flag("is_thread_op") {
        Tag::new("[OP]", TagKind::ThreadOp)
    } else if flag("is_op") {
        Tag::new("[OP]", TagKind::Op)
    } else {
        Tag::new("[RE]", TagKind::Reply)
    };
    let emotes = refs_from(v.get("emote_refs").or_else(|| v.get("emoteRefs")));
    Some(SpecialLine {
        tab: TabKind::Feed,
        line: special_chat(
            platform_or_twitch(v.get("platform").and_then(Value::as_str).unwrap_or("")),
            String::new(),
            Some(id),
            user,
            color_of(&json!({"color": v.get("user_color").cloned().unwrap_or(Value::Null)})),
            content,
            emotes,
        ),
        tags: vec![role, Tag::new("", TagKind::Heat)],
        heat: v.get("heat").and_then(Value::as_f64).unwrap_or(0.0),
        peer: None,
    })
}

fn is_irc(v: &Value, kind: &str) -> bool {
    v.get("message")
        .and_then(|m| m.get("type"))
        .and_then(Value::as_str)
        == Some(kind)
}

/// a message's platform id, if the frame carries one.
fn id_of(m: &Value) -> Option<String> {
    m.get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 128)
        .map(str::to_string)
}

/// twitch CLEARMSG → one message struck through.
fn clearmsg_event(v: &Value) -> Event {
    let m = &v["message"];
    let id = field(m, "targetMsgId");
    if id.is_empty() {
        return Event::Ignore;
    }
    Event::Delete {
        line: None,
        del: Delete {
            platform: Platform::Twitch,
            channel: field(v, "channel"),
            target: Target::Msg(id),
            label: "deleted".into(),
        },
    }
}

/// twitch CLEARCHAT → ban / timeout / full clear: a mod-log note, and the
/// target's lines struck through.
fn clearchat_event(v: &Value) -> Event {
    let m = &v["message"];
    let target = field(m, "targetUsername");
    let (label, t) = if target.is_empty() {
        ("cleared".to_string(), Target::All)
    } else {
        let label = match m.get("banDuration").and_then(Value::as_u64) {
            Some(s) => format!("timed out {}", fmt_secs(s)),
            None => "banned".to_string(),
        };
        (label, Target::User(target.clone()))
    };
    let channel = field(v, "channel");
    let line = note_line(
        Platform::Twitch,
        channel.clone(),
        target,
        None,
        NoteKind::Mod,
        if matches!(t, Target::All) {
            "chat cleared".into()
        } else {
            label.clone()
        },
        String::new(),
    );
    Event::Delete {
        line,
        del: Delete {
            platform: Platform::Twitch,
            channel,
            target: t,
            label,
        },
    }
}

/// youtube delete frame: message ids only (no author handle we can match).
fn youtube_delete_event(v: &Value) -> Event {
    let ids: Vec<&str> = v
        .get("messageIds")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let dels: Vec<Event> = ids
        .into_iter()
        .map(|id| Event::Delete {
            line: None,
            del: Delete {
                platform: Platform::Youtube,
                channel: field(v, "videoId"),
                target: Target::Msg(id.to_string()),
                label: "deleted".into(),
            },
        })
        .collect();
    Event::Batch(dels)
}

/// twitch NOTICE → server line ("this room is now in slow mode").
fn notice_line(m: &Value, channel: Option<&Value>) -> Option<ChatLine> {
    let text = field(m, "content");
    if text.is_empty() {
        return None;
    }
    let channel = channel.and_then(Value::as_str).unwrap_or("").to_string();
    note_line(
        Platform::Twitch,
        channel,
        String::new(),
        None,
        NoteKind::Notice,
        text,
        String::new(),
    )
}

fn stream_ev(kind: StreamKind, game: String, prev_game: String, follow: bool) -> StreamEv {
    StreamEv {
        kind,
        game,
        prev_game,
        follow,
    }
}

/// `follow:live:snapshot` → the (platform, channel) pairs already live.
fn follow_snapshot(v: &Value) -> Event {
    let live = v
        .get("streams")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|s| {
                    let platform = match field(s, "platform").to_lowercase().as_str() {
                        "twitch" => Platform::Twitch,
                        "kick" => Platform::Kick,
                        "youtube" => Platform::Youtube,
                        _ => return None,
                    };
                    let user = field(s, "username").to_lowercase();
                    (!user.is_empty()).then_some((platform, user))
                })
                .collect()
        })
        .unwrap_or_default();
    Event::FollowLive(live)
}

/// `follow:history` → the stored events as follow stream lines. only the three
/// lifecycle kinds count (stored `type` mirrors `follow:stream:*`, bare
/// `stream:*` is accepted too); a bad row never costs the others.
fn follow_history(v: &Value) -> Event {
    let mut out: Vec<(ChatLine, u64)> = v
        .get("events")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|e| {
                    let ty = e.get("type").and_then(Value::as_str)?;
                    let ty = ty.strip_prefix("follow:").unwrap_or(ty);
                    if !matches!(ty, "stream:online" | "stream:offline" | "stream:update") {
                        return None;
                    }
                    let at = e.get("time").and_then(Value::as_f64).filter(|t| *t > 0.0)?;
                    Some((stream_line_as(e, ty, true)?, at as u64))
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort_by_key(|(_, at)| *at); // stable: oldest first
    if out.is_empty() {
        Event::Ignore
    } else {
        Event::FollowHistory(out)
    }
}

/// `follow:colors` → (login, `#rrggbb`) pairs; anything else in the map is dropped.
fn follow_colors(v: &Value) -> Event {
    let colors: Vec<(String, String)> = v
        .get("colors")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, c)| {
                    let c = color_str(c)?;
                    (!k.is_empty()).then(|| (k.to_lowercase(), c.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();
    if colors.is_empty() {
        Event::Ignore
    } else {
        Event::FollowColors(colors)
    }
}

/// stream lifecycle / hype / redemption / heat-spike frames. these carry
/// their platform + channel — the receiver routes them like chat.
fn stream_line(v: &Value) -> Option<ChatLine> {
    let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
    match ty.strip_prefix("follow:") {
        Some(t) => stream_line_as(v, t, true),
        None => stream_line_as(v, ty, false),
    }
}

/// [`stream_line`] with the (prefix-free) kind and follow flag already decided.
fn stream_line_as(v: &Value, ty: &str, follow: bool) -> Option<ChatLine> {
    let platform = match v.get("platform").and_then(Value::as_str)? {
        "twitch" => Platform::Twitch,
        "kick" => Platform::Kick,
        "youtube" => Platform::Youtube,
        _ => return None,
    };
    let channel = field(v, "channel");
    if channel.is_empty() {
        return None;
    }
    let n = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
    let color = if follow { color_of(v) } else { None };
    match ty {
        "stream:online" => {
            let game = field(v, "game");
            let what = if game.is_empty() {
                "went live".to_string()
            } else {
                format!("went live — {game}")
            };
            let mut l = note_line(
                platform,
                channel,
                String::new(),
                color,
                NoteKind::Live,
                what,
                field(v, "title"),
            )?;
            l.stream = Some(stream_ev(StreamKind::Online, game, String::new(), follow));
            Some(l)
        }
        "stream:offline" => {
            let mut l = note_line(
                platform,
                channel,
                String::new(),
                color,
                NoteKind::Offline,
                "went offline".into(),
                String::new(),
            )?;
            l.stream = Some(stream_ev(
                StreamKind::Offline,
                String::new(),
                String::new(),
                follow,
            ));
            Some(l)
        }
        // only a new game is worth a line; a title-only change is dropped
        // (the extension drops it too).
        "stream:update" => {
            let (game, prev) = (field(v, "game"), field(v, "prevGame"));
            if game.is_empty() || game == prev {
                return None;
            }
            let mut l = note_line(
                platform,
                channel,
                String::new(),
                color,
                NoteKind::Category,
                format!("switched to {game}"),
                String::new(),
            )?;
            l.stream = Some(stream_ev(StreamKind::Update, game, prev, follow));
            Some(l)
        }
        "stream:raid" => {
            let target = field(v, "target");
            if target.is_empty() {
                return None;
            }
            let viewers = n("viewers");
            let what = if viewers > 0 {
                format!("raided {target} with {viewers} viewers")
            } else {
                format!("raided {target}")
            };
            note_line(
                platform,
                channel,
                String::new(),
                None,
                NoteKind::Raid,
                what,
                String::new(),
            )
        }
        "stream:redeem" => {
            let title = field(v, "title");
            let cost = n("cost");
            let what = match (title.is_empty(), cost) {
                (false, c) if c > 0 => format!("redeemed {title} ({c})"),
                (false, _) => format!("redeemed {title}"),
                _ => "redeemed".to_string(),
            };
            note_line(
                platform,
                channel,
                field(v, "user"),
                None,
                NoteKind::Redeem,
                what,
                String::new(),
            )
        }
        "stream:hype-start" => note_line(
            platform,
            channel,
            String::new(),
            None,
            NoteKind::Spike,
            format!("hype train — level {}", n("level").max(1)),
            String::new(),
        ),
        "stream:hype-end" => note_line(
            platform,
            channel,
            String::new(),
            None,
            NoteKind::Spike,
            format!("hype train ended — level {}", n("level").max(1)),
            String::new(),
        ),
        "moment:spike" => {
            let rate = v.get("rate").and_then(Value::as_f64)?;
            let base = v.get("baseline").and_then(Value::as_f64).unwrap_or(0.0);
            let what = if base > 0.0 {
                format!("chat spike — {rate:.0}/s (avg {base:.0})")
            } else {
                format!("chat spike — {rate:.0}/s")
            };
            note_line(
                platform,
                channel,
                String::new(),
                None,
                NoteKind::Spike,
                what,
                String::new(),
            )
        }
        _ => None,
    }
}

/// kick subscription webhook → sub / resub / gift bomb. the `message` field
/// is kick's system text ("slowlux resubscribed for 39 months!").
fn kick_sub_line(v: &Value) -> Option<ChatLine> {
    let channel = field(v, "channel");
    if channel.is_empty() {
        return None;
    }
    let mut user = field(v, "username");
    let et = v.get("eventType").and_then(Value::as_str).unwrap_or("");
    let kind = if et == "gift" {
        NoteKind::Gift
    } else {
        NoteKind::Sub
    };
    let msg = field(v, "message");
    let mut what = split_headline(&mut user, &msg);
    if what.is_empty() {
        what = match et {
            "renewal" => match v.get("months").and_then(Value::as_u64) {
                Some(mo) if mo > 0 => format!("resubscribed — {mo} months"),
                _ => "resubscribed".into(),
            },
            "gift" => {
                let n = v
                    .get("giftees")
                    .and_then(Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(0);
                if n > 1 {
                    format!("gifted {n} subs")
                } else {
                    "gifted a sub".into()
                }
            }
            _ => "subscribed".into(),
        };
    }
    note_line(
        Platform::Kick,
        channel,
        user,
        None,
        kind,
        what,
        String::new(),
    )
}

/// kick "kicks" gifted (kick's bits) → cheer with the user's message.
fn kick_kicks_line(v: &Value) -> Option<ChatLine> {
    let channel = field(v, "channel");
    if channel.is_empty() {
        return None;
    }
    let amount = v.get("amount").and_then(Value::as_u64).unwrap_or(0);
    let what = if amount > 0 {
        format!("gifted {amount} kicks")
    } else {
        "gifted kicks".to_string()
    };
    note_line(
        Platform::Kick,
        channel,
        field(v, "username"),
        None,
        NoteKind::Cheer,
        what,
        field(v, "message"),
    )
}

/// youtube poller status → connected (with the channel's actual name — the
/// tab label is just a video id) / ended / error.
fn yt_status_line(v: &Value) -> Option<ChatLine> {
    let video = v.get("videoId").and_then(Value::as_str).unwrap_or("");
    if video.is_empty() {
        return None;
    }
    let (kind, what, text) = match v.get("status").and_then(Value::as_str).unwrap_or("") {
        "ended" => (NoteKind::Offline, "stream ended".to_string(), String::new()),
        "connected" => {
            let name = field(v, "channelName");
            let what = if name.is_empty() {
                "connected".to_string()
            } else {
                format!("connected — {name}")
            };
            (NoteKind::Notice, what, field(v, "title"))
        }
        "error" => (
            NoteKind::Notice,
            "connection error".to_string(),
            field(v, "error"),
        ),
        _ => return None,
    };
    note_line(
        Platform::Youtube,
        video.to_string(),
        String::new(),
        None,
        kind,
        what,
        text,
    )
}

/// one message out of a `youtube:chat` batch. field names differ from the
/// twitch/kick shape (`user`/`text`, not `username`/`content`); paid and
/// membership types become inline notes (`amount` is a display string like
/// "$5.00", passed through as-is).
fn yt_line(m: &Value, video: &str) -> Option<ChatLine> {
    let mut user = field(m, "user");
    let content = field(m, "text");
    let amount = field(m, "amount");
    let note = match m.get("type").and_then(Value::as_str).unwrap_or("message") {
        "superchat" => Some(Note {
            kind: NoteKind::Cheer,
            what: if amount.is_empty() {
                "superchat".into()
            } else {
                format!("superchat {amount}")
            },
        }),
        "supersticker" => Some(Note {
            kind: NoteKind::Cheer,
            what: if amount.is_empty() {
                "supersticker".into()
            } else {
                format!("supersticker {amount}")
            },
        }),
        "membership" => {
            let sys = field(m, "systemMsg");
            let what = split_headline(&mut user, &sys);
            Some(Note {
                kind: NoteKind::Sub,
                what: if what.is_empty() {
                    "became a member".into()
                } else {
                    what
                },
            })
        }
        "giftpurchase" | "giftredemption" => {
            let sys = field(m, "systemMsg");
            let what = split_headline(&mut user, &sys);
            Some(Note {
                kind: NoteKind::Gift,
                what: if what.is_empty() {
                    "gifted memberships".into()
                } else {
                    what
                },
            })
        }
        _ => None,
    };
    if user.is_empty() && content.is_empty() && note.is_none() {
        return None;
    }
    Some(ChatLine {
        platform: Platform::Youtube,
        channel: video.to_string(),
        id: id_of(m),
        user,
        color: color_of(m),
        badges: Vec::new(),
        reply_to: None,
        note,
        extra: Extra::default(),
        stream: None,
        content,
        uid: m
            .get("authorChannelId")
            .and_then(Value::as_str)
            .and_then(|c| paint_id("yt_", c)),
        emotes: Vec::new(),
    })
}

/// badges in either platform shape → normalized [`Badge`]s, deduped, order kept.
/// twitch: `"broadcaster/1,subscriber/12"` (a comma string, name before `/`);
/// kick and the archive API: `[{ "name": "moderator", "version": "1" }, …]`.
/// anything else → empty. public because the archive backfill reuses it.
pub fn badges_from(v: Option<&Value>) -> Vec<Badge> {
    let mut out: Vec<Badge> = Vec::new();
    let mut push = |name: &str, ver: u16| {
        if let Some(b) = Badge::from_name(name, ver) {
            if !out.iter().any(|e| e.kind == b.kind) {
                out.push(b);
            }
        }
    };
    match v {
        Some(Value::String(s)) => {
            for part in s.split(',') {
                let (name, ver) = part.split_once('/').unwrap_or((part, ""));
                push(name.trim(), ver.trim().parse().unwrap_or(0));
            }
        }
        Some(Value::Array(arr)) => {
            for b in arr {
                if let Some(name) = b.get("name").and_then(Value::as_str) {
                    let ver = match b.get("version") {
                        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
                        Some(Value::String(s)) => s.parse().unwrap_or(0),
                        _ => 0,
                    };
                    push(name, ver.min(u16::MAX as u64) as u16);
                }
            }
        }
        _ => {}
    }
    out
}

/// outbound: subscribe to a channel's live chat. youtube subscribes by watch
/// url (the server resolves it); our channel name IS the video id.
/// twitch needs BOTH: `irc:join` for chat, `channel:join` for the room the
/// server pushes stream:online/offline/update/raid/redeem into.
pub fn join(platform: Platform, channel: &str) -> Vec<String> {
    match platform {
        Platform::Twitch => vec![
            json!({ "type": "channel:join", "platform": "twitch", "channel": channel }),
            json!({ "type": "irc:join", "channel": channel }),
        ],
        Platform::Kick => {
            vec![json!({ "type": "channel:join", "platform": "kick", "channel": channel })]
        }
        Platform::Youtube => vec![json!({
            "type": "youtube:subscribe",
            "url": format!("https://www.youtube.com/watch?v={channel}"),
        })],
    }
    .iter()
    .map(Value::to_string)
    .collect()
}

/// outbound: unsubscribe.
pub fn part(platform: Platform, channel: &str) -> Vec<String> {
    match platform {
        Platform::Twitch => vec![
            json!({ "type": "channel:leave", "platform": "twitch", "channel": channel }),
            json!({ "type": "irc:part", "channel": channel }),
        ],
        Platform::Kick => {
            vec![json!({ "type": "channel:leave", "platform": "kick", "channel": channel })]
        }
        Platform::Youtube => vec![json!({ "type": "youtube:unsubscribe", "videoId": channel })],
    }
    .iter()
    .map(Value::to_string)
    .collect()
}

/// outbound: subscribe to the heatsync site feed ("new") — its `new-message`,
/// `heat:updated` and `message:hidden` frames only reach joined sockets.
pub fn feed_join() -> String {
    json!({ "type": "feed:join", "feed": "new" }).to_string()
}

pub fn feed_leave() -> String {
    json!({ "type": "feed:leave", "feed": "new" }).to_string()
}

/// outbound: keepalive, send every ~10s (server replies `pong`).
pub fn heartbeat() -> String {
    json!({ "type": "presence:heartbeat" }).to_string()
}

/// outbound: authenticate a session so sends are accepted. server replies
/// `authenticated` / `authentication_failed`.
pub fn authenticate(token: &str) -> String {
    json!({ "type": "authenticate", "token": token }).to_string()
}

/// outbound: post a message to a kick channel (relayed via the user's extension).
/// text is 1–500 chars server-side; `req` correlates the result frame.
/// note: twitch has NO send path — the relay ingests twitch read-only.
pub fn send_kick(channel: &str, text: &str, req: u64) -> String {
    json!({
        "type": "chat:send_kick",
        "channel": channel,
        "text": text,
        "reqId": req.to_string(),
    })
    .to_string()
}

/// outbound: post to a youtube live chat (relayed via the user's extension).
/// text is 1–200 chars server-side; requires an authenticated session.
pub fn send_youtube(video: &str, text: &str, req: u64) -> String {
    json!({
        "type": "chat:send_youtube",
        "videoId": video,
        "text": text,
        "reqId": req.to_string(),
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BadgeKind;

    #[test]
    fn parses_twitch_message() {
        let raw = r##"{"type":"irc:message","channel":"xqc","message":{
            "username":"chat_user","displayName":"ChatUser","content":"hello GAMBA",
            "color":"#ff8700","id":"abc"}}"##;
        let ev = parse(raw);
        assert_eq!(
            ev,
            Event::Chat(ChatLine {
                platform: Platform::Twitch,
                channel: "xqc".into(),
                id: Some("abc".into()),
                user: "ChatUser".into(),
                color: Some("#ff8700".into()),
                badges: vec![],
                reply_to: None,
                note: None,
                extra: Default::default(),
                stream: None,
                content: "hello GAMBA".into(),
                uid: None,
                emotes: vec![],
            })
        );
    }

    fn chat(raw: &str) -> ChatLine {
        match parse(raw) {
            Event::Chat(l) => l,
            e => panic!("expected chat, got {e:?}"),
        }
    }

    fn pairs(l: &ChatLine) -> Vec<(&str, &str, &str, bool)> {
        l.emotes
            .iter()
            .map(|e| {
                (
                    e.name.as_str(),
                    e.url.as_str(),
                    e.provider.as_str(),
                    e.zero_width,
                )
            })
            .collect()
    }

    #[test]
    fn twitch_emotes_tag_becomes_refs() {
        let l = chat(
            r##"{"type":"irc:message","channel":"c","message":{
            "username":"u","content":"Kappa hi LUL","emotes":"25:0-4/425618:9-11"}}"##,
        );
        assert_eq!(l.content, "Kappa hi LUL");
        assert_eq!(
            pairs(&l),
            vec![
                (
                    "Kappa",
                    "https://static-cdn.jtvnw.net/emoticons/v2/25/default/dark/2.0",
                    "twitch",
                    false
                ),
                (
                    "LUL",
                    "https://static-cdn.jtvnw.net/emoticons/v2/425618/default/dark/2.0",
                    "twitch",
                    false
                ),
            ]
        );
    }

    #[test]
    fn twitch_emote_positions_are_code_points() {
        // two astral emoji precede the emote: utf-16 / byte offsets would miss it
        let l = chat(
            "{\"type\":\"irc:message\",\"channel\":\"c\",\"message\":{\"username\":\"u\",\
             \"content\":\"\u{1f600}\u{1f600} Kappa\",\"emotes\":\"25:3-7\"}}",
        );
        assert_eq!(l.emotes.len(), 1);
        assert_eq!(l.emotes[0].name, "Kappa");
    }

    #[test]
    fn twitch_pre_resolved_map_is_accepted() {
        let l = chat(
            r##"{"type":"irc:message","channel":"c","message":{"username":"u","content":"hi Sub",
            "twitchEmotes":{"Sub":"https://static-cdn.jtvnw.net/emoticons/v2/9/default/dark/2.0","Bad":"http://x/y"}}}"##,
        );
        assert_eq!(l.emotes.len(), 1);
        assert_eq!(l.emotes[0].name, "Sub");
    }

    #[test]
    fn hs_emotes_map_becomes_refs_and_outranks_native() {
        let l = chat(
            r##"{"type":"irc:message","channel":"c","message":{
            "username":"u","content":"Kappa nope",
            "emotes":"25:0-4",
            "hsEmotes":{
              "Kappa":{"url":"https://cdn.hs/k.webp","provider":"heatsync"},
              "notL":{"url":"https://cdn.7tv.app/n.webp","provider":"7tv","zeroWidth":true},
              "Hidden":{"cw":"sexual"},
              "Evil":{"url":"javascript:alert(1)"},
              "two words":{"url":"https://cdn.hs/x.webp"}}}}"##,
        );
        // inventory Kappa first (native Kappa dedupes behind it); stubs/bad dropped
        assert_eq!(
            pairs(&l),
            vec![
                ("Kappa", "https://cdn.hs/k.webp", "heatsync", false),
                ("notL", "https://cdn.7tv.app/n.webp", "7tv", true),
            ]
        );
    }

    #[test]
    fn kick_emote_tokens_are_rewritten_and_ref_d() {
        let l = chat(
            r##"{"type":"kick-chat-message","data":{"channel":"c","username":"u",
            "content":"hi [emote:37226:KEKW][emote:1:Clap] go"}}"##,
        );
        assert_eq!(l.content, "hi KEKW Clap go");
        assert_eq!(
            pairs(&l),
            vec![
                (
                    "KEKW",
                    "https://files.kick.com/emotes/37226/fullsize",
                    "kick",
                    false
                ),
                (
                    "Clap",
                    "https://files.kick.com/emotes/1/fullsize",
                    "kick",
                    false
                ),
            ]
        );
    }

    #[test]
    fn kick_tokens_are_kick_only_and_hs_emotes_work_there_too() {
        // a twitch user typing the token stays literal
        let l = chat(
            r##"{"type":"irc:message","channel":"c","message":{"username":"u","content":"[emote:1:X]"}}"##,
        );
        assert_eq!(l.content, "[emote:1:X]");
        assert!(l.emotes.is_empty());
        let l = chat(
            r##"{"type":"kick-chat-message","data":{"channel":"c","username":"u","content":"hi Pog",
            "hsEmotes":{"Pog":{"url":"https://cdn.hs/p.webp","provider":"7tv"}}}}"##,
        );
        assert_eq!(l.emotes.len(), 1);
    }

    #[test]
    fn ref_count_is_capped() {
        let map: Vec<String> = (0..200)
            .map(|i| format!(r#""E{i}":{{"url":"https://cdn.hs/{i}.webp"}}"#))
            .collect();
        let raw = format!(
            r#"{{"type":"irc:message","channel":"c","message":{{"username":"u","content":"x","hsEmotes":{{{}}}}}}}"#,
            map.join(",")
        );
        assert_eq!(chat(&raw).emotes.len(), MAX_REFS);
    }

    fn special(raw: &str) -> SpecialLine {
        match parse(raw) {
            Event::Special(s) => s,
            e => panic!("expected special, got {e:?}"),
        }
    }

    #[test]
    fn mention_rule_match_becomes_a_mentions_line() {
        let s = special(
            r##"{"type":"mention:rule-match","ruleId":7,"pattern":"mellen","channel":"xqc",
            "platform":"kick","username":"bob","snippet":"hey mellen look","url":"https://heatsync.org/x"}"##,
        );
        assert_eq!(s.tab, TabKind::Mentions);
        assert_eq!(s.line.platform, Platform::Kick);
        assert_eq!(s.line.user, "bob");
        assert_eq!(s.line.content, "hey mellen look");
        assert_eq!(
            s.tags,
            vec![Tag::platform(), Tag::new("[xqc]", TagKind::Origin)]
        );
        assert!(matches!(
            parse(r#"{"type":"mention:rule-match"}"#),
            Event::Ignore
        ));
    }

    #[test]
    fn whisper_and_dm_land_in_the_whispers_tab_with_their_labels() {
        let w = special(
            r##"{"type":"whisper:new","from_twitch_login":"bob","from_display_name":"Bob",
            "from_color":"#ff0000","to_twitch_login":"me","content":"psst","created_at":"x"}"##,
        );
        assert_eq!((w.tab, w.line.user.as_str()), (TabKind::Whispers, "Bob"));
        assert_eq!(w.line.color.as_deref(), Some("#ff0000"));
        assert_eq!(w.tags, vec![Tag::new("[whisper]", TagKind::Whisper)]);
        let d = special(
            r##"{"type":"dm:new","from_display_name":"Al","from_color":"nope","content":"yo","from_user_id":3}"##,
        );
        assert_eq!(d.tags, vec![Tag::new("[DM]", TagKind::Dm)]);
        assert_eq!(d.line.color, None); // malformed color dropped
                                        // login stands in when there's no display name
        let l = special(r#"{"type":"whisper:new","from_twitch_login":"bob","content":"x"}"#);
        assert_eq!(l.line.user, "bob");
    }

    #[test]
    fn feed_row_parses_roles_heat_and_refs() {
        let s = special(
            r##"{"type":"new-message","base36_id":"00004a","content":"hi Pog","username":"u",
            "display_name":"U","user_color":"#FF4500","platform":"twitch","heat":42.5,
            "is_op":true,"is_thread_op":true,"reply_to":null,
            "emote_refs":{"Pog":{"url":"https://cdn.hs/p.webp","provider":"7tv"}}}"##,
        );
        assert_eq!(s.tab, TabKind::Feed);
        assert_eq!(s.line.id.as_deref(), Some("00004a"));
        assert_eq!(s.line.user, "U");
        assert_eq!(s.line.color.as_deref(), Some("#FF4500"));
        assert_eq!(s.heat, 42.5);
        assert_eq!(s.tags[0], Tag::new("[OP]", TagKind::ThreadOp));
        assert_eq!(s.tags[1].kind, TagKind::Heat);
        assert_eq!(s.line.emotes.len(), 1);
        for (flags, want) in [
            (r#""is_op":true"#, TagKind::Op),
            (r#""is_op":false"#, TagKind::Reply),
            (r#""x":1"#, TagKind::Reply),
        ] {
            let raw = format!(
                r#"{{"type":"new-message","base36_id":"1","content":"c","username":"u",{flags}}}"#
            );
            assert_eq!(special(&raw).tags[0].kind, want, "{flags}");
        }
        // no id → nothing to update later → dropped
        assert!(matches!(
            parse(r#"{"type":"new-message","content":"c","username":"u"}"#),
            Event::Ignore
        ));
    }

    #[test]
    fn feed_heat_and_hidden_name_the_row() {
        assert_eq!(
            parse(r#"{"type":"heat:updated","messageId":"00004a","newHeat":9.5}"#),
            Event::FeedHeat {
                id: "00004a".into(),
                heat: 9.5
            }
        );
        assert_eq!(
            parse(r#"{"type":"heat:updated","messageId":77,"newHeat":1}"#),
            Event::FeedHeat {
                id: "77".into(),
                heat: 1.0
            }
        );
        assert_eq!(
            parse(r#"{"type":"message:hidden","id":"00004a"}"#),
            Event::FeedHidden {
                id: "00004a".into()
            }
        );
        assert!(matches!(parse(r#"{"type":"heat:updated"}"#), Event::Ignore));
    }

    #[test]
    fn special_text_is_sanitized() {
        let s = special(
            "{\"type\":\"mention:rule-match\",\"channel\":\"c\",\"username\":\"u\",\
             \"snippet\":\"hi\\u001b]0;x\\u0007\"}",
        );
        assert_eq!(s.line.content, "hi]0;x");
    }

    #[test]
    fn twitch_badge_string_parses_and_dedupes() {
        let raw = r##"{"type":"irc:message","channel":"c","message":{
            "username":"u","content":"hi",
            "badges":"broadcaster/1,subscriber/12,sub-gifter/5,broadcaster/1"}}"##;
        match parse(raw) {
            Event::Chat(l) => {
                assert_eq!(
                    l.badges.iter().map(|b| b.kind).collect::<Vec<_>>(),
                    [
                        BadgeKind::Broadcaster,
                        BadgeKind::Subscriber,
                        BadgeKind::Gifter
                    ]
                );
                assert_eq!(l.badges[1].ver, 12);
            }
            _ => panic!("expected chat"),
        }
    }

    #[test]
    fn kick_badge_array_parses() {
        let raw = r##"{"type":"kick-chat-message","data":{
            "channel":"c","username":"u","content":"hi",
            "badges":[{"name":"moderator","version":"1"},{"name":"og","version":"1"},
                      {"name":"mystery","version":"1"}]}}"##;
        match parse(raw) {
            Event::Chat(l) => assert_eq!(
                l.badges.iter().map(|b| b.kind).collect::<Vec<_>>(),
                [BadgeKind::Moderator, BadgeKind::Og, BadgeKind::Other('m')]
            ),
            _ => panic!("expected chat"),
        }
    }

    #[test]
    fn reply_to_username_is_extracted_and_sanitized() {
        let raw = "{\"type\":\"irc:message\",\"channel\":\"c\",\"message\":{
            \"username\":\"u\",\"content\":\"hi\",
            \"replyTo\":{\"username\":\"tar\\u001bget\",\"content\":\"orig\"}}}";
        match parse(raw) {
            Event::Chat(l) => assert_eq!(l.reply_to.as_deref(), Some("target")),
            _ => panic!("expected chat"),
        }
    }

    #[test]
    fn parses_kick_message_and_sanitizes() {
        // control chars arrive JSON-escaped (backslash-u) — valid JSON, then
        // stripped by sanitize: \u001b => ESC, \u0007 => BEL.
        let raw = "{\"type\":\"kick-chat-message\",\"data\":{\
            \"channel\":\"trainwreckstv\",\"username\":\"u\",\"displayName\":\"U\",\
            \"content\":\"hi\\u001b]0;evil\\u0007\",\"color\":\"#53fc18\"}}";
        let ev = parse(raw);
        match ev {
            Event::Chat(l) => {
                assert_eq!(l.platform, Platform::Kick);
                assert_eq!(l.content, "hi]0;evil"); // ESC + BEL stripped
                assert!(!l.content.contains('\u{1b}'));
            }
            _ => panic!("expected chat"),
        }
    }

    #[test]
    fn backfill_yields_all_lines() {
        let raw = r#"{"type":"kick-chat-backfill","channel":"c","replay":true,"messages":[
            {"channel":"c","username":"a","content":"one"},
            {"channel":"c","username":"b","content":"two"}]}"#;
        match parse(raw) {
            Event::Backfill(v) => assert_eq!(v.len(), 2),
            _ => panic!("expected backfill"),
        }
    }

    #[test]
    fn junk_is_ignored_not_fatal() {
        assert_eq!(parse("not json"), Event::Ignore);
        assert_eq!(parse(r#"{"type":"whatever"}"#), Event::Ignore);
        assert_eq!(parse(r#"{"type":"pong"}"#), Event::Pong);
    }

    #[test]
    fn pin_frames_set_and_clear() {
        let set = r#"{"type":"pin:set","platform":"twitch","channel":"c","pin":{
            "id":"p1","message_id":"m","username":"bob","display_name":"Bob",
            "content":"read the rules","ts":1,"pinned_at":2}}"#;
        match parse(set) {
            Event::Pin(Platform::Twitch, c, Some(p)) => {
                assert_eq!(c, "c");
                assert_eq!((p.id.as_str(), p.user.as_str()), ("p1", "Bob"));
                assert_eq!(p.text, "read the rules");
            }
            e => panic!("{e:?}"),
        }
        assert_eq!(
            parse(r#"{"type":"pin:clear","platform":"kick","channel":"c"}"#),
            Event::Pin(Platform::Kick, "c".into(), None)
        );
        for bad in [
            r#"{"type":"pin:set","platform":"mars","channel":"c","pin":{"content":"x"}}"#,
            r#"{"type":"pin:set","platform":"twitch","channel":"c","pin":{"content":""}}"#,
            r#"{"type":"pin:set","platform":"twitch","channel":"c"}"#,
            r#"{"type":"pin:clear","platform":"twitch"}"#,
        ] {
            assert_eq!(parse(bad), Event::Ignore, "{bad}");
        }
        let anon = serde_json::json!({"id":"i","username":"bob","content":"hey\u{1b}[2J"});
        assert_eq!(pin_from(&anon).unwrap().text, "hey[2J");
    }

    #[test]
    fn badges_table_keeps_version_and_falls_back_to_first_letter() {
        let got = badges_from(Some(&json!(
            "subscriber/12,sub-gifter/5,partner/1,hype-train/1,enchantedbigboi/1,moderator/1,subscriber/6,/1"
        )));
        let seen: Vec<(char, u16)> = got.iter().map(|b| (b.glyph(), b.ver)).collect();
        assert_eq!(
            seen,
            [('S', 12), ('✦', 5), ('✓', 1), ('H', 1), ('E', 1), ('M', 1)],
            "dupe subscriber dropped, nameless dropped"
        );
        let kick = badges_from(Some(&json!([
            {"name":"og","version":"1"},
            {"name":"sub_gifter","version":3},
            {"name":"mystery"}
        ])));
        let seen: Vec<(char, u16)> = kick.iter().map(|b| (b.glyph(), b.ver)).collect();
        assert_eq!(seen, [('O', 1), ('✦', 3), ('M', 0)]);
        assert!(badges_from(Some(&json!(7))).is_empty());
    }

    #[test]
    fn feed_frames_are_correct() {
        assert_eq!(feed_join(), r#"{"feed":"new","type":"feed:join"}"#);
        assert_eq!(feed_leave(), r#"{"feed":"new","type":"feed:leave"}"#);
    }

    #[test]
    fn join_frames_are_correct() {
        assert_eq!(
            join(Platform::Twitch, "xqc"),
            [
                r#"{"channel":"xqc","platform":"twitch","type":"channel:join"}"#,
                r#"{"channel":"xqc","type":"irc:join"}"#,
            ]
        );
        assert_eq!(
            part(Platform::Twitch, "xqc"),
            [
                r#"{"channel":"xqc","platform":"twitch","type":"channel:leave"}"#,
                r#"{"channel":"xqc","type":"irc:part"}"#,
            ]
        );
        assert_eq!(join(Platform::Kick, "x").len(), 1);
        assert!(join(Platform::Kick, "x")[0].contains("channel:join"));
        let yt = join(Platform::Youtube, "dQw4w9WgXcQ");
        assert_eq!(yt.len(), 1);
        assert!(yt[0].contains("youtube:subscribe"));
        assert!(yt[0].contains("watch?v=dQw4w9WgXcQ"));
        assert!(part(Platform::Youtube, "dQw4w9WgXcQ")[0].contains(r#""videoId":"dQw4w9WgXcQ""#));
    }

    #[test]
    fn youtube_batch_parses_chat_superchat_and_membership() {
        let raw = r##"{"type":"youtube:chat","videoId":"vid123abc","messages":[
            {"user":"alice","text":"hi chat","color":"#ff0000"},
            {"type":"superchat","user":"bob","text":"take my money","amount":"$5.00"},
            {"type":"membership","user":"carl","text":"","systemMsg":"carl: Member for 2 months"},
            {"user":"","text":""}]}"##;
        match parse(raw) {
            Event::Backfill(lines) => {
                assert_eq!(lines.len(), 3, "empty line dropped");
                assert_eq!(lines[0].platform, Platform::Youtube);
                assert_eq!(lines[0].channel, "vid123abc");
                assert_eq!(lines[0].user, "alice");
                assert_eq!(lines[0].color.as_deref(), Some("#ff0000"));
                assert!(lines[0].note.is_none());
                let sc = lines[1].note.as_ref().expect("superchat note");
                assert_eq!(sc.kind, NoteKind::Cheer);
                assert_eq!(sc.what, "superchat $5.00");
                assert_eq!(lines[1].content, "take my money");
                let mem = lines[2].note.as_ref().expect("membership note");
                assert_eq!(mem.kind, NoteKind::Sub);
                assert_eq!(mem.what, "Member for 2 months");
                assert_eq!(lines[2].user, "carl");
            }
            _ => panic!("expected backfill"),
        }
    }

    #[test]
    fn usernotice_resub_strips_actor_from_headline_and_keeps_message() {
        let raw = r##"{"type":"irc:message","channel":"xqc","message":{
            "type":"usernotice","subType":"resub","username":"realck3","displayName":"RealCK3",
            "content":"what the heck",
            "systemMessage":"RealCK3 subscribed at Tier 1. They've subscribed for 44 months!",
            "color":"#3AEEFF"}}"##;
        match parse(raw) {
            Event::Chat(l) => {
                assert_eq!(l.user, "RealCK3");
                let n = l.note.expect("note");
                assert_eq!(n.kind, NoteKind::Sub);
                assert_eq!(
                    n.what,
                    "subscribed at Tier 1. They've subscribed for 44 months!"
                );
                assert_eq!(l.content, "what the heck");
                assert_eq!(l.color.as_deref(), Some("#3AEEFF"));
            }
            _ => panic!("expected chat"),
        }
    }

    #[test]
    fn usernotice_raid_headline_is_self_contained_so_actor_drops() {
        let raw = r##"{"type":"irc:message","channel":"c","message":{
            "type":"usernotice","subType":"raid","username":"someraider",
            "displayName":"SomeRaider","content":"",
            "systemMessage":"15 raiders from SomeRaider have joined!"}}"##;
        match parse(raw) {
            Event::Chat(l) => {
                assert_eq!(l.user, "");
                let n = l.note.expect("note");
                assert_eq!(n.kind, NoteKind::Raid);
                assert_eq!(n.what, "15 raiders from SomeRaider have joined!");
            }
            _ => panic!("expected chat"),
        }
    }

    #[test]
    fn usernotice_kinds_and_announcement_tints() {
        use crate::Tint;
        for (sub, color, want) in [
            ("announcement", "PURPLE", NoteKind::Announce(Tint::Magenta)),
            ("announcement", "ORANGE", NoteKind::Announce(Tint::Yellow)),
            ("announcement", "BLUE", NoteKind::Announce(Tint::Blue)),
            ("announcement", "GREEN", NoteKind::Announce(Tint::Green)),
            ("announcement", "PRIMARY", NoteKind::Announce(Tint::Plain)),
            ("announcement", "", NoteKind::Announce(Tint::Plain)),
            ("watchstreak", "", NoteKind::Streak),
            ("viewermilestone", "", NoteKind::Milestone),
            ("charitydonation", "", NoteKind::Charity),
            ("ritual", "", NoteKind::Ritual),
            ("new_chatter", "", NoteKind::Ritual),
            ("somethingnew", "", NoteKind::Notice),
        ] {
            let raw = format!(
                r#"{{"type":"irc:message","channel":"c","message":{{"type":"usernotice",
                "subType":"{sub}","announceColor":"{color}","username":"u","displayName":"U",
                "systemMessage":"U did a thing"}}}}"#
            );
            match parse(&raw) {
                Event::Chat(l) => assert_eq!(l.note.unwrap().kind, want, "{sub} {color}"),
                e => panic!("{sub}: {e:?}"),
            }
        }
    }

    #[test]
    fn reply_action_first_and_highlight_ride_along() {
        let raw = r#"{"type":"irc:message","channel":"c","message":{
            "username":"u","displayName":"U","content":"yeah","isAction":true,
            "isFirstMsg":true,"isHighlighted":true,
            "replyTo":{"username":"Tar","content":"is it \u001b[31mreal","messageId":"m1"}}}"#;
        match parse(raw) {
            Event::Chat(l) => {
                assert_eq!(l.reply_to.as_deref(), Some("Tar"));
                assert_eq!(l.extra.reply_text, "is it [31mreal");
                assert!(l.extra.action && l.extra.first && l.extra.highlight);
                assert!(l.note.is_none(), "highlight is a style, not a note");
            }
            e => panic!("{e:?}"),
        }
        // kick shape: replyTo without content still yields the user.
        let kick = r#"{"type":"kick-chat-message","data":{"channel":"c","username":"u",
            "content":"hi","replyTo":{"messageId":"m","username":"bob","content":"orig"}}}"#;
        match parse(kick) {
            Event::Chat(l) => {
                assert_eq!(l.reply_to.as_deref(), Some("bob"));
                assert_eq!(l.extra.reply_text, "orig");
                assert!(!l.extra.action);
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn usernotice_announcement_keeps_author_and_text() {
        let raw = r##"{"type":"irc:message","channel":"c","message":{
            "type":"usernotice","subType":"announcement","username":"modguy",
            "displayName":"ModGuy","content":"drops are live","systemMessage":""}}"##;
        match parse(raw) {
            Event::Chat(l) => {
                assert_eq!(l.user, "ModGuy");
                let n = l.note.expect("note");
                assert_eq!(n.kind, NoteKind::Announce(crate::Tint::Plain));
                assert_eq!(n.what, "announced");
                assert_eq!(l.content, "drops are live");
            }
            _ => panic!("expected chat"),
        }
    }

    fn del(raw: &str) -> (Option<ChatLine>, Delete) {
        match parse(raw) {
            Event::Delete { line, del } => (line, del),
            e => panic!("expected delete, got {e:?}"),
        }
    }

    #[test]
    fn clearchat_maps_to_timeout_ban_and_clear() {
        let (l, d) = del(r#"{"type":"irc:message","channel":"c","message":{
            "type":"clearchat","targetUsername":"troll","banDuration":600}}"#);
        let l = l.unwrap();
        assert_eq!(l.user, "troll");
        assert_eq!(l.note.unwrap().what, "timed out 10m");
        assert_eq!(d.target, Target::User("troll".into()));
        assert_eq!(d.label, "timed out 10m");
        assert_eq!(d.channel, "c");

        let (l, d) = del(r#"{"type":"irc:message","channel":"c","message":{
            "type":"clearchat","targetUsername":"troll"}}"#);
        assert_eq!(l.unwrap().note.unwrap().what, "banned");
        assert_eq!(d.label, "banned");

        let (l, d) = del(r#"{"type":"irc:message","channel":"c","message":{"type":"clearchat"}}"#);
        let l = l.unwrap();
        assert_eq!(l.user, "");
        let n = l.note.unwrap();
        assert_eq!(n.kind, NoteKind::Mod);
        assert_eq!(n.what, "chat cleared");
        assert_eq!(d.target, Target::All);
    }

    #[test]
    fn clearmsg_targets_one_message_by_id() {
        let (l, d) = del(r#"{"type":"irc:message","channel":"c","message":{
            "type":"clearmsg","targetMsgId":"abc-123","login":"troll"}}"#);
        assert!(l.is_none(), "a single delete adds no mod-log line");
        assert_eq!(d.target, Target::Msg("abc-123".into()));
        assert_eq!(d.platform, Platform::Twitch);
        assert_eq!(d.label, "deleted");
    }

    #[test]
    fn youtube_delete_batches_message_ids() {
        match parse(r#"{"type":"youtube:delete","videoId":"v1","messageIds":["a","b"]}"#) {
            Event::Batch(es) => {
                assert_eq!(es.len(), 2);
                match &es[1] {
                    Event::Delete { del, .. } => {
                        assert_eq!(del.target, Target::Msg("b".into()));
                        assert_eq!(del.channel, "v1");
                        assert_eq!(del.platform, Platform::Youtube);
                    }
                    e => panic!("{e:?}"),
                }
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn kick_and_youtube_lines_carry_their_ids() {
        let raw = r##"{"type":"kick-chat-message","data":{
            "id":"k-1","channel":"c","username":"u","content":"hi"}}"##;
        match parse(raw) {
            Event::Chat(l) => assert_eq!(l.id.as_deref(), Some("k-1")),
            e => panic!("{e:?}"),
        }
        let raw = r##"{"type":"youtube:chat","videoId":"v","messages":[{"id":"y-9","user":"a","text":"x"}]}"##;
        match parse(raw) {
            Event::Backfill(l) => assert_eq!(l[0].id.as_deref(), Some("y-9")),
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn roomstate_is_state_not_a_line_and_clearmsg_needs_a_target() {
        let rs = r#"{"type":"irc:message","channel":"c","message":{
            "type":"roomstate","emoteOnly":false,"slow":0}}"#;
        assert!(matches!(parse(rs), Event::Room(..)));
        let cm = r#"{"type":"irc:message","channel":"c","message":{
            "type":"clearmsg","targetMsgId":""}}"#;
        assert_eq!(parse(cm), Event::Ignore, "no target id, nothing to strike");
    }

    #[test]
    fn privmsg_bits_become_a_cheer_note() {
        let raw = r##"{"type":"irc:message","channel":"c","message":{
            "type":"privmsg","username":"u","content":"Cheer100 gg","bits":100}}"##;
        match parse(raw) {
            Event::Chat(l) => {
                let n = l.note.expect("note");
                assert_eq!(n.kind, NoteKind::Cheer);
                assert_eq!(n.what, "cheered 100 bits");
                assert_eq!(l.content, "Cheer100 gg");
            }
            _ => panic!("expected chat"),
        }
    }

    #[test]
    fn stream_frames_route_by_platform_and_channel() {
        let online = r##"{"type":"stream:online","platform":"kick","channel":"xqc",
            "game":"Just Chatting","title":"BIG DAY"}"##;
        match parse(online) {
            Event::Chat(l) => {
                assert_eq!(l.platform, Platform::Kick);
                assert_eq!(l.channel, "xqc");
                let n = l.note.unwrap();
                assert_eq!(n.kind, NoteKind::Live);
                assert_eq!(n.what, "went live — Just Chatting");
                assert_eq!(l.content, "BIG DAY");
            }
            _ => panic!(),
        }
        let update = r##"{"type":"stream:update","platform":"twitch","channel":"c",
            "isLive":true,"game":"Elden Ring","title":"t","prevGame":"Just Chatting","prevTitle":"t"}"##;
        match parse(update) {
            Event::Chat(l) => {
                let n = l.note.unwrap();
                assert_eq!(n.kind, NoteKind::Category);
                assert_eq!(n.what, "switched to Elden Ring");
                assert_eq!(l.content, "", "unchanged title not repeated");
            }
            _ => panic!(),
        }
        let redeem = r##"{"type":"stream:redeem","platform":"twitch","channel":"c",
            "user":"viewer","title":"Hydrate","cost":500}"##;
        match parse(redeem) {
            Event::Chat(l) => {
                assert_eq!(l.user, "viewer");
                assert_eq!(l.note.unwrap().what, "redeemed Hydrate (500)");
            }
            _ => panic!(),
        }
        match parse(r#"{"type":"stream:offline","platform":"twitch","channel":"c"}"#) {
            Event::Chat(l) => {
                let n = l.note.unwrap();
                assert_eq!(n.kind, NoteKind::Offline);
                assert_eq!(n.what, "went offline");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn title_only_update_and_same_game_make_no_line() {
        let t = r#"{"type":"stream:update","platform":"twitch","channel":"c",
            "game":"A","prevGame":"A","title":"new","prevTitle":"old"}"#;
        assert_eq!(parse(t), Event::Ignore);
        let e = r#"{"type":"stream:update","platform":"twitch","channel":"c",
            "game":"","prevGame":"A","title":"new","prevTitle":"old"}"#;
        assert_eq!(parse(e), Event::Ignore);
    }

    #[test]
    fn online_without_game_and_first_game_update() {
        let on = r#"{"type":"stream:online","platform":"twitch","channel":"c","game":""}"#;
        match parse(on) {
            Event::Chat(l) => assert_eq!(l.note.unwrap().what, "went live"),
            _ => panic!(),
        }
        let up = r#"{"type":"stream:update","platform":"twitch","channel":"c",
            "game":"X","prevGame":""}"#;
        match parse(up) {
            Event::Chat(l) => {
                assert_eq!(l.note.unwrap().what, "switched to X");
                let s = l.stream.unwrap();
                assert_eq!(
                    (s.kind, s.game.as_str(), s.prev_game.as_str()),
                    (StreamKind::Update, "X", "")
                );
                assert!(!s.follow);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn follow_events_carry_flag_and_color() {
        let raw = r##"{"type":"follow:stream:online","platform":"twitch","channel":"cyr",
            "game":"Chess","title":"t","color":"#ff0000"}"##;
        match parse(raw) {
            Event::Chat(l) => {
                assert_eq!(l.color.as_deref(), Some("#ff0000"));
                assert_eq!(l.note.unwrap().what, "went live — Chess");
                let s = l.stream.unwrap();
                assert!(s.follow);
                assert_eq!(s.kind, StreamKind::Online);
            }
            _ => panic!(),
        }
        match parse(r#"{"type":"follow:stream:offline","platform":"kick","channel":"cyr"}"#) {
            Event::Chat(l) => {
                assert_eq!(l.platform, Platform::Kick);
                assert!(l.stream.unwrap().follow);
            }
            _ => panic!(),
        }
        match parse(
            r#"{"type":"follow:stream:update","platform":"twitch","channel":"c","game":"B","prevGame":"A"}"#,
        ) {
            Event::Chat(l) => assert_eq!(l.note.unwrap().what, "switched to B"),
            _ => panic!(),
        }
    }

    #[test]
    fn follow_history_becomes_ordered_follow_lines() {
        let raw = r##"{"type":"follow:history","events":[
          {"type":"follow:stream:offline","platform":"kick","channel":"b","time":2000000},
          {"type":"follow:stream:online","platform":"twitch","channel":"a","game":"Doom","title":"t","color":"#ff0000","time":1000000},
          {"type":"follow:stream:update","platform":"twitch","channel":"a","game":"Quake","prevGame":"Doom","time":1500000},
          {"type":"follow:stream:update","platform":"twitch","channel":"a","game":"Quake","prevGame":"Quake","time":1600000},
          {"type":"stream:raid","platform":"twitch","channel":"a","time":1700000},
          {"type":"follow:stream:online","platform":"twitch","channel":"a","time":0},
          {"type":"follow:stream:online","platform":"nope","channel":"a","time":5},
          {"type":"follow:stream:online","channel":"a","time":5},
          7]}"##;
        match parse(raw) {
            Event::FollowHistory(h) => {
                let got: Vec<_> = h
                    .iter()
                    .map(|(l, at)| {
                        let n = l.note.as_ref().unwrap();
                        (
                            l.channel.as_str(),
                            n.what.as_str(),
                            *at,
                            l.stream.as_ref().unwrap().follow,
                        )
                    })
                    .collect();
                assert_eq!(
                    got,
                    vec![
                        ("a", "went live — Doom", 1_000_000, true),
                        ("a", "switched to Quake", 1_500_000, true),
                        ("b", "went offline", 2_000_000, true),
                    ]
                );
                assert_eq!(h[0].0.color.as_deref(), Some("#ff0000"));
                assert_eq!(h[0].0.content, "t");
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn follow_colors_keep_only_valid_hex_lowercased() {
        let raw = r##"{"type":"follow:colors","colors":{"Cyr":"#ff00aa","bad":"red","short":"#fff","n":5}}"##;
        assert_eq!(
            parse(raw),
            Event::FollowColors(vec![("cyr".into(), "#ff00aa".into())])
        );
    }

    #[test]
    fn follow_history_and_colors_garbage_is_ignored() {
        for raw in [
            r#"{"type":"follow:history"}"#,
            r#"{"type":"follow:history","events":"x"}"#,
            r#"{"type":"follow:history","events":[]}"#,
            r#"{"type":"follow:history","events":[{"type":"follow:stream:online"}]}"#,
            r#"{"type":"follow:colors"}"#,
            r#"{"type":"follow:colors","colors":[1,2]}"#,
            r#"{"type":"follow:colors","colors":{}}"#,
        ] {
            assert_eq!(parse(raw), Event::Ignore, "{raw}");
        }
    }

    #[test]
    fn follow_snapshot_lists_live_channels() {
        let raw = r#"{"type":"follow:live:snapshot","streams":[
            {"username":"Cyr","platform":"twitch","viewerCount":5},
            {"username":"xqc","platform":"KICK"},
            {"username":"","platform":"twitch"},
            {"username":"y","platform":"mars"}]}"#;
        assert_eq!(
            parse(raw),
            Event::FollowLive(vec![
                (Platform::Twitch, "cyr".into()),
                (Platform::Kick, "xqc".into())
            ])
        );
    }

    #[test]
    fn spike_and_raid_frames_read_terse() {
        let spike = r##"{"type":"moment:spike","platform":"twitch","channel":"c",
            "rate":34,"baseline":4.2,"title":"x"}"##;
        match parse(spike) {
            Event::Chat(l) => {
                let n = l.note.unwrap();
                assert_eq!(n.kind, NoteKind::Spike);
                assert_eq!(n.what, "chat spike — 34/s (avg 4)");
            }
            _ => panic!(),
        }
        let raid = r##"{"type":"stream:raid","platform":"twitch","channel":"c",
            "target":"friend","viewers":500}"##;
        match parse(raid) {
            Event::Chat(l) => {
                assert_eq!(l.note.unwrap().what, "raided friend with 500 viewers");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn twitch_eventsub_subs_are_dropped_as_usernotice_dupes() {
        assert_eq!(
            parse(r#"{"type":"stream:sub","platform":"twitch","channel":"c","user":"u"}"#),
            Event::Ignore
        );
        assert_eq!(
            parse(
                r#"{"type":"stream:sub-gift","platform":"twitch","channel":"c","user":"u","count":5}"#
            ),
            Event::Ignore
        );
    }

    #[test]
    fn kick_sub_events_use_system_text_with_fallbacks() {
        // live-captured shape: message is kick's own system sentence.
        let renewal = r##"{"type":"kick-sub-event","channel":"xqc","eventType":"renewal",
            "username":"slowlux","months":39,"message":"slowlux resubscribed for 39 months!"}"##;
        match parse(renewal) {
            Event::Chat(l) => {
                assert_eq!(l.platform, Platform::Kick);
                assert_eq!(l.user, "slowlux");
                let n = l.note.unwrap();
                assert_eq!(n.kind, NoteKind::Sub);
                assert_eq!(n.what, "resubscribed for 39 months!");
            }
            _ => panic!(),
        }
        let gift = r##"{"type":"kick-sub-event","channel":"c","eventType":"gift",
            "username":"rich","gifter":"rich","giftees":["a","b","c"],"message":""}"##;
        match parse(gift) {
            Event::Chat(l) => {
                let n = l.note.unwrap();
                assert_eq!(n.kind, NoteKind::Gift);
                assert_eq!(n.what, "gifted 3 subs");
            }
            _ => panic!(),
        }
        let kicks = r##"{"type":"kick-kicks-event","channel":"c","username":"fan",
            "amount":100,"giftName":"Kicks","message":"take it"}"##;
        match parse(kicks) {
            Event::Chat(l) => {
                let n = l.note.unwrap();
                assert_eq!(n.kind, NoteKind::Cheer);
                assert_eq!(n.what, "gifted 100 kicks");
                assert_eq!(l.content, "take it");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn youtube_status_maps_to_connected_and_ended() {
        let conn = r##"{"type":"youtube:status","videoId":"vid123abc","status":"connected",
            "channelName":"Northernlion","title":"the best stream"}"##;
        match parse(conn) {
            Event::Chat(l) => {
                assert_eq!(l.platform, Platform::Youtube);
                assert_eq!(l.channel, "vid123abc");
                let n = l.note.unwrap();
                assert_eq!(n.kind, NoteKind::Notice);
                assert_eq!(n.what, "connected — Northernlion");
                assert_eq!(l.content, "the best stream");
            }
            _ => panic!(),
        }
        let ended = r#"{"type":"youtube:status","videoId":"vid123abc","status":"ended"}"#;
        match parse(ended) {
            Event::Chat(l) => {
                let n = l.note.unwrap();
                assert_eq!(n.kind, NoteKind::Offline);
                assert_eq!(n.what, "stream ended");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn headline_split_respects_word_boundaries() {
        let mut user = "a".to_string();
        assert_eq!(
            split_headline(&mut user, "announcement time"),
            "announcement time"
        );
        assert_eq!(user, "", "non-prefixed headline drops the actor");
        let mut user = "Ann".to_string();
        assert_eq!(split_headline(&mut user, "Ann subscribed!"), "subscribed!");
        assert_eq!(user, "Ann");
    }

    #[test]
    fn timeout_durations_format_terse() {
        assert_eq!(fmt_secs(45), "45s");
        assert_eq!(fmt_secs(600), "10m");
        assert_eq!(fmt_secs(7200), "2h");
        assert_eq!(fmt_secs(86_400), "1d");
        assert_eq!(fmt_secs(90), "90s");
    }

    #[test]
    fn send_youtube_frame_shape() {
        let f = send_youtube("vid123abc", "hi", 7);
        assert!(f.contains(r#""type":"chat:send_youtube""#));
        assert!(f.contains(r#""videoId":"vid123abc""#));
        assert!(f.contains(r#""reqId":"7""#));
    }
    #[test]
    fn sender_ids_land_in_paint_space() {
        let tw = r#"{"type":"irc:message","channel":"c","message":{
            "username":"u","content":"x","userId":"12345"}}"#;
        let Event::Chat(l) = parse(tw) else { panic!() };
        assert_eq!(l.uid.as_deref(), Some("12345"));
        // kick ids overlap twitch's range — they never go bare
        let kick = r#"{"type":"kick-chat-message","data":{"channel":"c","username":"u",
            "content":"x","senderId":99}}"#;
        let Event::Chat(l) = parse(kick) else {
            panic!()
        };
        assert_eq!(l.uid.as_deref(), Some("kick_99"));
        let yt = r#"{"type":"youtube:chat","videoId":"v","messages":[
            {"user":"u","text":"x","authorChannelId":"UCabc-1"}]}"#;
        let Event::Backfill(ls) = parse(yt) else {
            panic!()
        };
        assert_eq!(ls[0].uid.as_deref(), Some("yt_UCabc-1"));
        let bad = r#"{"type":"irc:message","channel":"c","message":{
            "username":"u","content":"x","userId":"1,2"}}"#;
        let Event::Chat(l) = parse(bad) else { panic!() };
        assert_eq!(l.uid, None);
    }

    #[test]
    fn color_and_cosmetic_pushes_parse() {
        let c = r##"{"type":"profile:color","userId":"7","usernames":["a"],"color":"#aabbcc"}"##;
        assert_eq!(
            parse(c),
            Event::Color {
                id: Some("7".into()),
                names: vec!["a".into()],
                color: "#aabbcc".into()
            }
        );
        let bad = r#"{"type":"profile:color","userId":"7","color":"red"}"#;
        assert_eq!(parse(bad), Event::Ignore);
        let names =
            r##"{"type":"profile:color","usernames":["MixedCase","kicker"],"color":"#010203"}"##;
        assert_eq!(
            parse(names),
            Event::Color {
                id: None,
                names: vec!["mixedcase".into(), "kicker".into()],
                color: "#010203".into()
            }
        );
        let empty = r##"{"type":"profile:color","usernames":[],"color":"#010203"}"##;
        assert_eq!(parse(empty), Event::Ignore);
        let ch = r#"{"type":"cosmetic:changed","ids":["7","kick_9","bad id"]}"#;
        assert_eq!(
            parse(ch),
            Event::Cosmetic(vec!["7".into(), "kick_9".into()])
        );
    }

    #[test]
    fn live_whispers_and_dms_remember_who_to_answer() {
        let w = special(
            r#"{"type":"whisper:new","from_twitch_id":"42","from_twitch_login":"Bob","from_display_name":"Bob","content":"hi"}"#,
        );
        let p = w.peer.unwrap();
        assert_eq!(
            (p.id.as_str(), p.login.as_str(), p.dm),
            ("42", "bob", false)
        );
        let d = special(
            r#"{"type":"dm:new","from_user_id":12,"from_display_name":"Cat","content":"yo"}"#,
        );
        let p = d.peer.unwrap();
        assert_eq!((p.id.as_str(), p.name.as_str(), p.dm), ("12", "Cat", true));
        // no id in the frame: still shown, just not answerable by name
        let n = special(r#"{"type":"whisper:new","from_display_name":"Bob","content":"hi"}"#);
        assert!(n.peer.is_none());
    }
}
