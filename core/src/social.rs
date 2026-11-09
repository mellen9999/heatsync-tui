//! the account side of the virtual tabs: dms, whispers and mention-rule hits
//! as the `/api/cli/*` routes return them, and the request bodies for sending.
//! pure — no network; the tui does the calls and feeds the answers here.

use serde_json::{json, Value};

use crate::i18n::{tr, trf};
use crate::proto::{field, platform_or_twitch, special_chat, str_or_num, SpecialLine};
use crate::{Platform, TabKind, Tag, TagKind};

/// someone you can whisper or dm: `id` is the twitch id (whisper) or the
/// heatsync user id (dm) — what the send routes want instead of a name.
#[derive(Clone, Debug, PartialEq)]
pub struct Contact {
    pub name: String,
    /// lowercase match key (the twitch login, or the heatsync username).
    pub login: String,
    pub id: String,
    pub dm: bool,
}

/// a history row with the time it happened, so threads merge chronologically.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub at: i64,
    pub line: SpecialLine,
}

/// one server-side mention-rule hit.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    /// the platform message id, when the rule match knew it (dedupe key).
    pub id: Option<String>,
    pub platform: Platform,
    pub channel: String,
    pub user: String,
    pub text: String,
    pub at: i64,
}

/// `POST /api/cli/messages`: a feed post, or a reply when `reply_to` is set.
pub fn post_body(content: &str, reply_to: Option<&str>) -> Value {
    let mut b = json!({ "content": content });
    if let Some(r) = reply_to.filter(|r| !r.is_empty()) {
        b["reply_to"] = json!(r);
    }
    b
}

/// `POST /api/cli/dm`.
pub fn dm_body(to_user_id: &str, content: &str) -> Value {
    json!({ "toUserId": to_user_id, "content": content })
}

/// `POST /api/cli/twitch/whisper` (the id must be digits).
pub fn whisper_body(to_user_id: &str, message: &str) -> Value {
    json!({ "to_user_id": to_user_id, "message": message })
}

pub(crate) fn contact(name: String, login: &str, id: String, dm: bool) -> Option<Contact> {
    let login = crate::sanitize::clean(login).to_lowercase();
    // ids go into a URL path: a twitch id is digits, a heatsync id letters,
    // digits, `_` and `-` — refuse anything else before it gets there
    let id_ok = !id.is_empty()
        && id.len() <= 50
        && id.bytes().all(|b| {
            b.is_ascii_digit() || (dm && (b.is_ascii_alphabetic() || b == b'_' || b == b'-'))
        });
    (id_ok && !login.is_empty()).then_some(Contact {
        name: if name.is_empty() { login.clone() } else { name },
        login,
        id,
        dm,
    })
}

/// `GET /api/cli/whispers`: who you have whispered with, plus the last person
/// who whispered you (what `/r` would answer).
pub fn whisper_contacts(v: &Value) -> Vec<Contact> {
    let mut out: Vec<Contact> = v
        .get("conversations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let login = field(c, "other_twitch_login");
            let name = field(c, "other_twitch_display");
            contact(name, &login, str_or_num(c, &["other_twitch_id"])?, false)
        })
        .collect();
    if let Some(l) = v.get("last_received") {
        let login = field(l, "login");
        if let Some(c) =
            str_or_num(l, &["twitch_id"]).and_then(|id| contact(String::new(), &login, id, false))
        {
            if !out.iter().any(|o| o.id == c.id) {
                out.push(c);
            }
        }
    }
    out
}

/// `GET /api/cli/dm`.
pub fn dm_contacts(v: &Value) -> Vec<Contact> {
    v.get("conversations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let login = field(c, "username");
            let name = field(c, "display_name");
            contact(name, &login, str_or_num(c, &["other_user_id"])?, true)
        })
        .collect()
}

/// `GET /api/cli/users/lookup` → the id `/w` (twitch) or `/dm` (heatsync) needs.
/// `None` when the person has no account on that side.
pub fn lookup_contact(v: &Value, login: &str, dm: bool) -> Option<Contact> {
    let id = str_or_num(v, &[if dm { "heatsync_user_id" } else { "twitch_id" }])?;
    contact(field(v, "display_name"), login, id, dm)
}

/// `GET /api/cli/following`: the people you follow on heatsync — the only
/// directory a terminal login can read, so it is how `/dm <user>` finds an id
/// for someone you have not messaged yet.
pub fn following_contacts(v: &Value) -> Vec<Contact> {
    v.get("following")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let login = field(c, "username");
            let name = field(c, "display_name");
            contact(name, &login, str_or_num(c, &["id"])?, true)
        })
        .collect()
}

fn at(m: &Value) -> i64 {
    m.get("created_at")
        .and_then(Value::as_str)
        .and_then(crate::iso::parse)
        .unwrap_or(0)
}

fn color(m: &Value) -> Option<String> {
    m.get("from_color")
        .and_then(Value::as_str)
        .filter(|s| s.starts_with('#') && (s.len() == 7 || s.len() == 4))
        .map(str::to_string)
}

fn thread_row(m: &Value, other: &Contact, incoming: bool, name: String) -> Row {
    let (tag, user, color) = match (incoming, other.dm) {
        (true, false) => (tr("social.whisper_in").to_string(), name, color(m)),
        (true, true) => (tr("social.dm_in").to_string(), name, color(m)),
        (false, false) => (
            trf("social.whisper_out", &[("to", &other.login)]),
            tr("social.you").into(),
            None,
        ),
        (false, true) => (
            trf("social.dm_out", &[("to", &other.login)]),
            tr("social.you").into(),
            None,
        ),
    };
    let kind = if other.dm {
        TagKind::Dm
    } else {
        TagKind::Whisper
    };
    Row {
        at: at(m),
        line: SpecialLine {
            tab: TabKind::Whispers,
            line: special_chat(
                Platform::Twitch,
                String::new(),
                None,
                user,
                color,
                field(m, "content"),
                Vec::new(),
            ),
            tags: vec![Tag::new(&tag, kind)],
            heat: 0.0,
            peer: None,
        },
    }
}

/// `GET /api/cli/whispers/:id` → rows, oldest first (the server sends newest first).
pub fn whisper_thread(v: &Value, other: &Contact) -> Vec<Row> {
    let mut rows: Vec<Row> = v
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|m| {
            let incoming = str_or_num(m, &["from_twitch_id"]).as_deref() == Some(other.id.as_str());
            let name = {
                let d = field(m, "from_twitch_display");
                if d.is_empty() {
                    field(m, "from_twitch_login")
                } else {
                    d
                }
            };
            thread_row(m, other, incoming, name)
        })
        .collect();
    rows.sort_by_key(|r| r.at);
    rows
}

/// `GET /api/cli/dm/:id` → rows, oldest first.
pub fn dm_thread(v: &Value, other: &Contact) -> Vec<Row> {
    let mut rows: Vec<Row> = v
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|m| {
            let incoming = str_or_num(m, &["from_user_id"]).as_deref() == Some(other.id.as_str());
            thread_row(m, other, incoming, other.name.clone())
        })
        .collect();
    rows.sort_by_key(|r| r.at);
    rows
}

/// `GET /api/cli/mention-rules/hits` → hits as the server lists them (newest first).
pub fn hits(v: &Value) -> Vec<Hit> {
    v.get("hits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|h| {
            let user = {
                let d = field(h, "display_name");
                if d.is_empty() {
                    field(h, "username")
                } else {
                    d
                }
            };
            let text = field(h, "snippet");
            if user.is_empty() && text.is_empty() {
                return None;
            }
            Some(Hit {
                id: str_or_num(h, &["message_id"]),
                platform: platform_or_twitch(
                    h.get("platform").and_then(Value::as_str).unwrap_or(""),
                ),
                channel: field(h, "channel"),
                user,
                text,
                at: at_of(h, "matched_at"),
            })
        })
        .collect()
}

fn at_of(v: &Value, k: &str) -> i64 {
    v.get(k)
        .and_then(Value::as_str)
        .and_then(crate::iso::parse)
        .unwrap_or(0)
}

/// the contact a typed name means: login or display name, `@` and case ignored.
pub fn resolve<'a>(contacts: &'a [Contact], dm: bool, who: &str) -> Option<&'a Contact> {
    let w = who.trim().trim_start_matches('@').to_lowercase();
    contacts
        .iter()
        .find(|c| c.dm == dm && (c.login == w || c.name.to_lowercase() == w))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wc() -> Contact {
        Contact {
            name: "Bob".into(),
            login: "bob".into(),
            id: "42".into(),
            dm: false,
        }
    }

    #[test]
    fn bodies_carry_exactly_the_documented_fields() {
        assert_eq!(post_body("gm", None), json!({"content": "gm"}));
        assert_eq!(
            post_body("gm", Some("abc12")),
            json!({"content": "gm", "reply_to": "abc12"})
        );
        assert_eq!(post_body("gm", Some("")), json!({"content": "gm"}));
        assert_eq!(
            dm_body("7", "hi"),
            json!({"toUserId": "7", "content": "hi"})
        );
        assert_eq!(
            whisper_body("42", "hi"),
            json!({"to_user_id": "42", "message": "hi"})
        );
    }

    #[test]
    fn whisper_contacts_read_conversations_and_the_last_sender_once() {
        let v = json!({
            "conversations": [
                {"other_twitch_id": "42", "other_twitch_login": "Bob", "other_twitch_display": "Bob", "content": "x"},
                {"other_twitch_id": "x/../9", "other_twitch_login": "evil"},
                {"other_twitch_id": 77, "other_twitch_login": "amy"}
            ],
            "last_received": {"twitch_id": "42", "login": "bob"}
        });
        let c = whisper_contacts(&v);
        let ids: Vec<_> = c.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["42", "77"]); // non-digit id refused, last_received not doubled
        assert_eq!(c[1].login, "amy");
        let only_last = whisper_contacts(
            &json!({"conversations": [], "last_received": {"twitch_id": "5", "login": "Zed"}}),
        );
        assert_eq!(only_last[0].name, "zed");
    }

    #[test]
    fn dm_contacts_read_user_ids() {
        let v = json!({"conversations": [
            {"other_user_id": "12", "username": "cat", "display_name": "Cat"},
            {"username": "no-id"}
        ]});
        let c = dm_contacts(&v);
        assert_eq!(c.len(), 1);
        assert_eq!(
            (c[0].id.as_str(), c[0].name.as_str(), c[0].dm),
            ("12", "Cat", true)
        );
    }

    #[test]
    fn lookup_picks_the_id_for_the_side_asked() {
        let v = json!({"twitch_id": "42", "heatsync_user_id": "u_7", "display_name": "Bob"});
        let w = lookup_contact(&v, "Bob", false).unwrap();
        assert_eq!(
            (w.id.as_str(), w.name.as_str(), w.login.as_str()),
            ("42", "Bob", "bob")
        );
        assert_eq!(lookup_contact(&v, "bob", true).unwrap().id, "u_7");
        let only_twitch = json!({"twitch_id": 42});
        assert!(lookup_contact(&only_twitch, "bob", true).is_none());
        assert!(lookup_contact(&json!({"twitch_id": "a/b"}), "bob", false).is_none());
    }

    #[test]
    fn following_makes_dm_contacts_and_skips_kick_style_ids_it_cannot_route() {
        let v = json!({"following": [
            {"id": "u1", "username": "streamer", "display_name": "Streamer"},
            {"id": "bad/id", "username": "x"}
        ]});
        let c = following_contacts(&v);
        assert_eq!(c.len(), 1);
        assert_eq!((c[0].id.as_str(), c[0].dm), ("u1", true));
    }

    #[test]
    fn a_whisper_thread_reads_oldest_first_with_direction() {
        let v = json!({"messages": [
            {"from_twitch_id": "9", "from_twitch_login": "me", "to_twitch_login": "bob", "content": "second", "created_at": "2026-10-04T20:00:05.000Z"},
            {"from_twitch_id": "42", "from_twitch_login": "bob", "from_twitch_display": "Bob", "from_color": "#ff0000", "content": "first", "created_at": "2026-10-04T20:00:00.000Z"}
        ]});
        let r = whisper_thread(&v, &wc());
        assert_eq!(r[0].line.line.content, "first");
        assert_eq!(r[0].line.line.user, "Bob");
        assert_eq!(r[0].line.line.color.as_deref(), Some("#ff0000"));
        assert_eq!(r[0].line.tags[0].text, "[whisper]");
        assert_eq!(r[1].line.line.user, "you");
        assert_eq!(r[1].line.tags[0].text, "[whisper → bob]");
        assert!(r[0].at < r[1].at);
    }

    #[test]
    fn a_dm_thread_marks_which_side_spoke() {
        let other = Contact {
            name: "Cat".into(),
            login: "cat".into(),
            id: "12".into(),
            dm: true,
        };
        let v = json!({"messages": [
            {"from_user_id": "5", "to_user_id": "12", "content": "yo", "created_at": "2026-10-04T20:00:02.000Z"},
            {"from_user_id": 12, "to_user_id": "5", "content": "hey", "created_at": "2026-10-04T20:00:01.000Z"}
        ]});
        let r = dm_thread(&v, &other);
        assert_eq!(r[0].line.line.user, "Cat");
        assert_eq!(r[0].line.tags[0].text, "[DM]");
        assert_eq!(r[1].line.tags[0].text, "[DM → cat]");
    }

    #[test]
    fn hits_keep_the_message_id_and_skip_empty_rows() {
        let v = json!({"hits": [
            {"id": 1, "rule_id": 3, "platform": "kick", "channel": "xqc", "username": "u", "display_name": "U", "message_id": "m1", "snippet": "hey mellen", "matched_at": "2026-10-04T20:00:00.000Z"},
            {"id": 2, "platform": "twitch", "channel": "c"}
        ], "kept": 200});
        let h = hits(&v);
        assert_eq!(h.len(), 1);
        assert_eq!(
            (
                h[0].id.as_deref(),
                h[0].platform,
                h[0].user.as_str(),
                h[0].at
            ),
            (Some("m1"), Platform::Kick, "U", 1791144000)
        );
    }

    #[test]
    fn resolve_matches_login_or_display_and_the_right_kind() {
        let mut dm = wc();
        dm.dm = true;
        dm.id = "7".into();
        let all = vec![wc(), dm];
        assert_eq!(resolve(&all, false, "@BOB").unwrap().id, "42");
        assert_eq!(resolve(&all, true, "bob").unwrap().id, "7");
        assert!(resolve(&all, false, "nobody").is_none());
    }
}
