//! HeatSync `/ws` relay protocol — parse inbound frames, build outbound ones.
//! anon read is allowed, so no auth is required to consume chat. content and
//! usernames are sanitized HERE, at the trust boundary, before anything else
//! touches them (terminal-injection defense).

use serde_json::{json, Value};

use crate::{sanitize, Badge, Note, NoteKind, Platform};

/// a normalized chat line, ready for a channel buffer.
#[derive(Clone, Debug, PartialEq)]
pub struct ChatLine {
    pub platform: Platform,
    pub channel: String,
    pub user: String,
    pub color: Option<String>,
    pub badges: Vec<Badge>,
    pub reply_to: Option<String>,
    /// set when this line is an event notice, not plain chat.
    pub note: Option<Note>,
    pub content: String,
}

/// what an inbound frame turned into.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Chat(ChatLine),
    Backfill(Vec<ChatLine>),
    /// authentication outcome (true = ok).
    Auth(bool),
    /// result of an outbound send: ok, or an error reason.
    SendResult {
        ok: bool,
        error: Option<String>,
    },
    Pong,
    Ignore,
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
        "irc:message" => v
            .get("message")
            .and_then(|m| match m.get("type").and_then(Value::as_str) {
                Some("usernotice") => usernotice_line(m, v.get("channel")),
                Some("clearchat") => clearchat_line(m, v.get("channel")),
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
        "stream:online" | "stream:offline" | "stream:update" | "stream:raid"
        | "stream:redeem" | "stream:hype-start" | "stream:hype-end" | "moment:spike" => {
            stream_line(&v).map(Event::Chat).unwrap_or(Event::Ignore)
        }
        "kick-sub-event" => kick_sub_line(&v).map(Event::Chat).unwrap_or(Event::Ignore),
        "kick-kicks-event" => kick_kicks_line(&v).map(Event::Chat).unwrap_or(Event::Ignore),
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
    let user = sanitize::clean(user_raw);
    let content = sanitize::clean(content_raw);
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
    } else if m
        .get("isHighlighted")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        Some(Note {
            kind: NoteKind::Redeem,
            what: "highlighted".into(),
        })
    } else {
        None
    };
    Some(ChatLine {
        platform,
        channel,
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
        content,
    })
}

/// a `#rrggbb`/`#rgb` user color, if the object carries a valid one.
fn color_of(m: &Value) -> Option<String> {
    m.get("color")
        .and_then(Value::as_str)
        .filter(|s| s.starts_with('#') && (s.len() == 7 || s.len() == 4))
        .map(|s| s.to_string())
}

/// sanitized string field, empty when absent.
fn field(v: &Value, k: &str) -> String {
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
fn fmt_secs(s: u64) -> String {
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
fn note_line(
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
        user,
        color,
        badges: Vec::new(),
        reply_to: None,
        note: Some(Note { kind, what }),
        content,
    })
}

/// twitch USERNOTICE → inline event. the system message is the headline;
/// `content` (a resub message) stays chat text under it.
fn usernotice_line(m: &Value, channel: Option<&Value>) -> Option<ChatLine> {
    let sub = m.get("subType").and_then(Value::as_str).unwrap_or("");
    let kind = match sub {
        "sub" | "resub" => NoteKind::Sub,
        "subgift" | "submysterygift" | "giftpaidupgrade" | "primepaidupgrade"
        | "standardpayforward" | "communitypayforward" => NoteKind::Gift,
        "raid" | "unraid" => NoteKind::Raid,
        _ => NoteKind::Notice, // announcement, viewermilestone, future subtypes
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

/// twitch CLEARCHAT → ban / timeout / full clear.
fn clearchat_line(m: &Value, channel: Option<&Value>) -> Option<ChatLine> {
    let target = field(m, "targetUsername");
    let what = if target.is_empty() {
        "chat cleared".to_string()
    } else {
        match m.get("banDuration").and_then(Value::as_u64) {
            Some(s) => format!("timed out {}", fmt_secs(s)),
            None => "banned".to_string(),
        }
    };
    let channel = channel.and_then(Value::as_str).unwrap_or("").to_string();
    note_line(
        Platform::Twitch,
        channel,
        target,
        None,
        NoteKind::Mod,
        what,
        String::new(),
    )
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

/// stream lifecycle / hype / redemption / heat-spike frames. these carry
/// their platform + channel — the receiver routes them like chat.
fn stream_line(v: &Value) -> Option<ChatLine> {
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
    match v.get("type").and_then(Value::as_str).unwrap_or("") {
        "stream:online" => {
            let game = field(v, "game");
            let what = if game.is_empty() {
                "live".to_string()
            } else {
                format!("live — {game}")
            };
            note_line(platform, channel, String::new(), None, NoteKind::Live, what, field(v, "title"))
        }
        "stream:offline" => note_line(
            platform,
            channel,
            String::new(),
            None,
            NoteKind::Offline,
            "offline".into(),
            String::new(),
        ),
        "stream:update" => {
            let (game, prev) = (field(v, "game"), field(v, "prevGame"));
            let (title, prev_t) = (field(v, "title"), field(v, "prevTitle"));
            if !game.is_empty() && game != prev {
                let what = if prev.is_empty() {
                    format!("→ {game}")
                } else {
                    format!("{prev} → {game}")
                };
                let t = if title != prev_t { title } else { String::new() };
                note_line(platform, channel, String::new(), None, NoteKind::Category, what, t)
            } else if !title.is_empty() && title != prev_t {
                note_line(
                    platform,
                    channel,
                    String::new(),
                    None,
                    NoteKind::Category,
                    "title".into(),
                    title,
                )
            } else {
                None
            }
        }
        "stream:raid" => {
            let target = field(v, "target");
            if target.is_empty() {
                return None;
            }
            let viewers = n("viewers");
            let what = if viewers > 0 {
                format!("raiding {target} — {viewers} viewers")
            } else {
                format!("raiding {target}")
            };
            note_line(platform, channel, String::new(), None, NoteKind::Raid, what, String::new())
        }
        "stream:redeem" => {
            let title = field(v, "title");
            let cost = n("cost");
            let what = match (title.is_empty(), cost) {
                (false, c) if c > 0 => format!("redeemed {title} ({c})"),
                (false, _) => format!("redeemed {title}"),
                _ => "redeemed".to_string(),
            };
            note_line(platform, channel, field(v, "user"), None, NoteKind::Redeem, what, String::new())
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
            note_line(platform, channel, String::new(), None, NoteKind::Spike, what, String::new())
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
    note_line(Platform::Kick, channel, user, None, kind, what, String::new())
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
        "error" => (NoteKind::Notice, "connection error".to_string(), field(v, "error")),
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
        user,
        color: color_of(m),
        badges: Vec::new(),
        reply_to: None,
        note,
        content,
    })
}

/// badges in either platform shape → normalized [`Badge`]s, deduped, order kept.
/// twitch: `"broadcaster/1,subscriber/12"` (a comma string, name before `/`);
/// kick and the archive API: `[{ "name": "moderator", "version": "1" }, …]`.
/// anything else → empty. public because the archive backfill reuses it.
pub fn badges_from(v: Option<&Value>) -> Vec<Badge> {
    let mut out = Vec::new();
    let mut push = |name: &str| {
        if let Some(b) = Badge::from_name(name) {
            if !out.contains(&b) {
                out.push(b);
            }
        }
    };
    match v {
        Some(Value::String(s)) => {
            for part in s.split(',') {
                push(part.split('/').next().unwrap_or("").trim());
            }
        }
        Some(Value::Array(arr)) => {
            for b in arr {
                if let Some(name) = b.get("name").and_then(Value::as_str) {
                    push(name);
                }
            }
        }
        _ => {}
    }
    out
}

/// outbound: subscribe to a channel's live chat. youtube subscribes by watch
/// url (the server resolves it); our channel name IS the video id.
pub fn join(platform: Platform, channel: &str) -> String {
    match platform {
        Platform::Twitch => json!({ "type": "irc:join", "channel": channel }),
        Platform::Kick => {
            json!({ "type": "channel:join", "platform": "kick", "channel": channel })
        }
        Platform::Youtube => json!({
            "type": "youtube:subscribe",
            "url": format!("https://www.youtube.com/watch?v={channel}"),
        }),
    }
    .to_string()
}

/// outbound: unsubscribe.
pub fn part(platform: Platform, channel: &str) -> String {
    match platform {
        Platform::Twitch => json!({ "type": "irc:part", "channel": channel }),
        Platform::Kick => {
            json!({ "type": "channel:leave", "platform": "kick", "channel": channel })
        }
        Platform::Youtube => json!({ "type": "youtube:unsubscribe", "videoId": channel }),
    }
    .to_string()
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
                user: "ChatUser".into(),
                color: Some("#ff8700".into()),
                badges: vec![],
                reply_to: None,
                note: None,
                content: "hello GAMBA".into(),
            })
        );
    }

    #[test]
    fn twitch_badge_string_parses_and_dedupes() {
        let raw = r##"{"type":"irc:message","channel":"c","message":{
            "username":"u","content":"hi",
            "badges":"broadcaster/1,subscriber/12,sub-gifter/5,broadcaster/1"}}"##;
        match parse(raw) {
            Event::Chat(l) => {
                assert_eq!(l.badges, vec![Badge::Broadcaster, Badge::Subscriber]);
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
            Event::Chat(l) => assert_eq!(l.badges, vec![Badge::Moderator, Badge::Og]),
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
    fn join_frames_are_correct() {
        assert_eq!(
            join(Platform::Twitch, "xqc"),
            r#"{"channel":"xqc","type":"irc:join"}"#
        );
        assert!(join(Platform::Kick, "x").contains("channel:join"));
        let yt = join(Platform::Youtube, "dQw4w9WgXcQ");
        assert!(yt.contains("youtube:subscribe"));
        assert!(yt.contains("watch?v=dQw4w9WgXcQ"));
        assert!(part(Platform::Youtube, "dQw4w9WgXcQ").contains(r#""videoId":"dQw4w9WgXcQ""#));
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
    fn usernotice_announcement_keeps_author_and_text() {
        let raw = r##"{"type":"irc:message","channel":"c","message":{
            "type":"usernotice","subType":"announcement","username":"modguy",
            "displayName":"ModGuy","content":"drops are live","systemMessage":""}}"##;
        match parse(raw) {
            Event::Chat(l) => {
                assert_eq!(l.user, "ModGuy");
                let n = l.note.expect("note");
                assert_eq!(n.kind, NoteKind::Notice);
                assert_eq!(n.what, "announced");
                assert_eq!(l.content, "drops are live");
            }
            _ => panic!("expected chat"),
        }
    }

    #[test]
    fn clearchat_maps_to_timeout_ban_and_clear() {
        let timeout = r#"{"type":"irc:message","channel":"c","message":{
            "type":"clearchat","targetUsername":"troll","banDuration":600}}"#;
        match parse(timeout) {
            Event::Chat(l) => {
                assert_eq!(l.user, "troll");
                assert_eq!(l.note.unwrap().what, "timed out 10m");
            }
            _ => panic!(),
        }
        let ban = r#"{"type":"irc:message","channel":"c","message":{
            "type":"clearchat","targetUsername":"troll"}}"#;
        match parse(ban) {
            Event::Chat(l) => assert_eq!(l.note.unwrap().what, "banned"),
            _ => panic!(),
        }
        let clear = r#"{"type":"irc:message","channel":"c","message":{"type":"clearchat"}}"#;
        match parse(clear) {
            Event::Chat(l) => {
                assert_eq!(l.user, "");
                let n = l.note.unwrap();
                assert_eq!(n.kind, NoteKind::Mod);
                assert_eq!(n.what, "chat cleared");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn roomstate_and_clearmsg_stay_silent() {
        let rs = r#"{"type":"irc:message","channel":"c","message":{
            "type":"roomstate","emoteOnly":false,"slow":0}}"#;
        assert_eq!(parse(rs), Event::Ignore);
        let cm = r#"{"type":"irc:message","channel":"c","message":{
            "type":"clearmsg","targetMsgId":"x"}}"#;
        assert_eq!(parse(cm), Event::Ignore);
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
                assert_eq!(n.what, "live — Just Chatting");
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
                assert_eq!(n.what, "Just Chatting → Elden Ring");
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
                assert_eq!(n.what, "offline");
            }
            _ => panic!(),
        }
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
                assert_eq!(l.note.unwrap().what, "raiding friend — 500 viewers");
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
        assert_eq!(split_headline(&mut user, "announcement time"), "announcement time");
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
}
