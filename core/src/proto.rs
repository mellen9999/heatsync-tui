//! HeatSync `/ws` relay protocol — parse inbound frames, build outbound ones.
//! anon read is allowed, so no auth is required to consume chat. content and
//! usernames are sanitized HERE, at the trust boundary, before anything else
//! touches them (terminal-injection defense).

use serde_json::{json, Value};

use crate::{sanitize, Platform};

/// a normalized chat line, ready for a channel buffer.
#[derive(Clone, Debug, PartialEq)]
pub struct ChatLine {
    pub platform: Platform,
    pub channel: String,
    pub user: String,
    pub color: Option<String>,
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
    SendResult { ok: bool, error: Option<String> },
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
        "authenticated" => Event::Auth(true),
        "authentication_failed" => Event::Auth(false),
        "chat:send_kick_result" | "chat:send_youtube_result" => Event::SendResult {
            ok: v.get("ok").and_then(Value::as_bool).unwrap_or(false),
            error: v.get("error").and_then(Value::as_str).map(|s| s.to_string()),
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
    let channel = channel
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Some(ChatLine {
        platform,
        channel,
        user,
        color,
        content,
    })
}

/// outbound: subscribe to a channel's live chat.
pub fn join(platform: Platform, channel: &str) -> String {
    match platform {
        Platform::Twitch => json!({ "type": "irc:join", "channel": channel }),
        Platform::Kick => {
            json!({ "type": "channel:join", "platform": "kick", "channel": channel })
        }
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
                content: "hello GAMBA".into(),
            })
        );
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
        assert_eq!(join(Platform::Twitch, "xqc"), r#"{"channel":"xqc","type":"irc:join"}"#);
        assert!(join(Platform::Kick, "x").contains("channel:join"));
    }
}
