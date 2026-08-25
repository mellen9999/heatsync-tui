//! HeatSync `/ws` relay protocol — parse inbound frames, build outbound ones.
//! anon read is allowed, so no auth is required to consume chat. content and
//! usernames are sanitized HERE, at the trust boundary, before anything else
//! touches them (terminal-injection defense).

use serde_json::{json, Value};

use crate::{sanitize, Badge, Platform};

/// a normalized chat line, ready for a channel buffer.
#[derive(Clone, Debug, PartialEq)]
pub struct ChatLine {
    pub platform: Platform,
    pub channel: String,
    pub user: String,
    pub color: Option<String>,
    pub badges: Vec<Badge>,
    pub reply_to: Option<String>,
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
        "irc:message" => v
            .get("message")
            .and_then(|m| line_from(m, Platform::Twitch, v.get("channel")))
            .map(Event::Chat)
            .unwrap_or(Event::Ignore),
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
    let color = m
        .get("color")
        .and_then(Value::as_str)
        .filter(|s| s.starts_with('#') && (s.len() == 7 || s.len() == 4))
        .map(|s| s.to_string());
    let channel = channel.and_then(Value::as_str).unwrap_or("").to_string();
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
        content,
    })
}

/// one message out of a `youtube:chat` batch. field names differ from the
/// twitch/kick shape (`user`/`text`, not `username`/`content`); superchat money
/// is folded into the text so it reads in any tier.
fn yt_line(m: &Value, video: &str) -> Option<ChatLine> {
    let user = sanitize::clean(m.get("user").and_then(Value::as_str).unwrap_or(""));
    let mut content = sanitize::clean(m.get("text").and_then(Value::as_str).unwrap_or(""));
    if let Some(amt) = m.get("amount").and_then(Value::as_f64).filter(|a| *a > 0.0) {
        let cur = sanitize::clean(m.get("currency").and_then(Value::as_str).unwrap_or(""));
        content = format!("[{cur} {amt}] {content}");
    }
    if user.is_empty() && content.is_empty() {
        return None;
    }
    let color = m
        .get("color")
        .and_then(Value::as_str)
        .filter(|s| s.starts_with('#') && (s.len() == 7 || s.len() == 4))
        .map(|s| s.to_string());
    Some(ChatLine {
        platform: Platform::Youtube,
        channel: video.to_string(),
        user,
        color,
        badges: Vec::new(),
        reply_to: None,
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
    fn youtube_batch_parses_as_backfill_with_superchat_folded_in() {
        let raw = r##"{"type":"youtube:chat","videoId":"vid123abc","messages":[
            {"user":"alice","text":"hi chat","color":"#ff0000"},
            {"user":"bob","text":"take my money","amount":5.0,"currency":"USD"},
            {"user":"","text":""}]}"##;
        match parse(raw) {
            Event::Backfill(lines) => {
                assert_eq!(lines.len(), 2, "empty line dropped");
                assert_eq!(lines[0].platform, Platform::Youtube);
                assert_eq!(lines[0].channel, "vid123abc");
                assert_eq!(lines[0].user, "alice");
                assert_eq!(lines[0].color.as_deref(), Some("#ff0000"));
                assert_eq!(lines[1].content, "[USD 5] take my money");
            }
            _ => panic!("expected backfill"),
        }
    }

    #[test]
    fn send_youtube_frame_shape() {
        let f = send_youtube("vid123abc", "hi", 7);
        assert!(f.contains(r#""type":"chat:send_youtube""#));
        assert!(f.contains(r#""videoId":"vid123abc""#));
        assert!(f.contains(r#""reqId":"7""#));
    }
}
