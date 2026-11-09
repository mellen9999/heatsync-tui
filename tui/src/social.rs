//! the account calls behind the virtual tabs — feed posts, dms, whispers and
//! mention hits — over the heatsync login (`/api/cli/*`). everything here is
//! blocking and runs on its own thread; the answers come back as [`Msg`].

use heatsync_core::i18n::{tr, trf};
use heatsync_core::social::{self, Contact, Hit, Row};
use serde_json::Value;

use crate::hsauth::{explain, Client};

/// conversations whose recent messages load when a tab opens, and how many
/// messages each contributes.
const CONVOS: usize = 8;
const PER_CONVO: usize = 15;

/// what a finished call tells the UI.
pub(crate) enum Msg {
    /// whisper + dm history, merged oldest-first, and who it taught us to address.
    History {
        contacts: Vec<Contact>,
        rows: Vec<Row>,
    },
    Hits(Vec<Hit>),
    /// the follow list, refetched after `follows:changed`.
    Follows(Vec<Contact>),
    /// a whisper / dm went out to them — remembered so the next one skips any lookup.
    Sent(Contact),
    /// the name matched nobody: why, and the command to put back in the composer.
    Missing(String, String),
    /// a line for the status bar (a send's outcome, or why a load failed).
    Say(String),
}

/// an answer that is not 200 → plain words. `chat` = a send (needs the chat
/// permission), otherwise a read (needs account).
fn fail(base: &str, st: u16, v: &Value, chat: bool) -> String {
    if v.get("needs_consent").and_then(Value::as_bool) == Some(true) {
        return trf("input.social_consent", &[("base", base)]);
    }
    let relink = v.get("relink_required").and_then(Value::as_bool) == Some(true);
    if st == 403 && !relink && v.get("error_code").and_then(Value::as_str) == Some("scope_missing")
    {
        return if chat {
            tr("input.social_no_chat_scope").into()
        } else {
            tr("input.social_no_read_scope").into()
        };
    }
    explain(st, v, base)
}

impl Client {
    fn read(&self, path: &str) -> Result<Value, String> {
        match self.call("GET", path, None)? {
            (200, v) => Ok(v),
            (st, v) => Err(fail(self.base(), st, &v, false)),
        }
    }

    /// the people you follow, as dm-addressable contacts.
    pub(crate) fn follow_contacts(&self) -> Result<Vec<Contact>, String> {
        self.read("/api/cli/following")
            .map(|v| social::following_contacts(&v))
    }

    fn write(&self, path: &str, body: Value) -> Result<Value, String> {
        match self.call("POST", path, Some(body))? {
            (200, v) => Ok(v),
            (st, v) => Err(fail(self.base(), st, &v, true)),
        }
    }

    /// post to the heatsync feed; a reply names the post it answers.
    pub(crate) fn feed_post(&self, text: &str, reply_to: Option<&str>) -> Result<(), String> {
        self.write("/api/cli/messages", social::post_body(text, reply_to))
            .map(|_| ())
    }

    pub(crate) fn whisper(&self, to: &Contact, text: &str) -> Result<(), String> {
        self.write(
            "/api/cli/twitch/whisper",
            social::whisper_body(&to.id, text),
        )
        .map(|_| ())
    }

    pub(crate) fn dm(&self, to: &Contact, text: &str) -> Result<(), String> {
        self.write("/api/cli/dm", social::dm_body(&to.id, text))
            .map(|_| ())
    }

    /// whisper and dm conversations, plus the newest messages of the busiest few.
    /// a half answer (whispers but no dms) still counts; only both failing is an error.
    pub(crate) fn history(&self) -> Result<(Vec<Contact>, Vec<Row>), String> {
        let w = self.read("/api/cli/whispers").map(|v| {
            let c = social::whisper_contacts(&v);
            let rows = self.threads(&c, "whispers", social::whisper_thread);
            (c, rows)
        });
        let d = self.read("/api/cli/dm").map(|v| {
            let c = social::dm_contacts(&v);
            let rows = self.threads(&c, "dm", social::dm_thread);
            (c, rows)
        });
        let (mut contacts, mut rows) = (Vec::new(), Vec::new());
        let (mut err, mut any_ok) = (None, false);
        // follows teach `/dm` ids for people you haven't messaged yet.
        let follows = self
            .read("/api/cli/following")
            .map(|v| social::following_contacts(&v))
            .unwrap_or_default();
        for r in [w, d] {
            match r {
                Ok((c, r)) => {
                    any_ok = true;
                    contacts.extend(c);
                    rows.extend(r);
                }
                Err(e) => err = Some(e),
            }
        }
        if let (false, Some(e)) = (any_ok, err) {
            return Err(e);
        }
        for f in follows {
            if !contacts.iter().any(|c| c.dm && c.id == f.id) {
                contacts.push(f);
            }
        }
        rows.sort_by_key(|r| r.at);
        Ok((contacts, rows))
    }

    fn threads(
        &self,
        who: &[Contact],
        route: &str,
        parse: fn(&Value, &Contact) -> Vec<Row>,
    ) -> Vec<Row> {
        who.iter()
            .take(CONVOS)
            .filter_map(|c| {
                let v = self
                    .read(&format!("/api/cli/{route}/{}?limit={PER_CONVO}", c.id))
                    .ok()?;
                Some(parse(&v, c))
            })
            .flatten()
            .collect()
    }

    /// who `login` is on twitch / heatsync. `Ok(None)` = unknown, or not a name
    /// worth asking about (the login goes into a URL, so only plain names do).
    pub(crate) fn lookup(&self, login: &str, dm: bool) -> Result<Option<Contact>, String> {
        let plain = !login.is_empty()
            && login.len() <= 25
            && login
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_');
        if !plain {
            return Ok(None);
        }
        match self.call(
            "GET",
            &format!("/api/cli/users/lookup?login={login}&platform=twitch"),
            None,
        )? {
            (200, v) => Ok(social::lookup_contact(&v, login, dm)),
            (404, _) => Ok(None),
            (st, v) => Err(fail(self.base(), st, &v, false)),
        }
    }

    /// what your mention rules caught (the server keeps hits while you're away).
    pub(crate) fn mention_hits(&self) -> Result<Vec<Hit>, String> {
        self.read("/api/cli/mention-rules/hits?limit=100")
            .map(|v| social::hits(&v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hsauth::testkit::{client, Fake};
    use serde_json::json;

    fn c(id: &str, login: &str, dm: bool) -> Contact {
        Contact {
            name: login.into(),
            login: login.into(),
            id: id.into(),
            dm,
        }
    }

    #[test]
    fn a_feed_post_sends_the_documented_body_with_the_bearer() {
        let f = Fake::new(vec![
            Ok((200, json!({"success": true}))),
            Ok((200, json!({}))),
        ]);
        let cl = client(&f);
        cl.feed_post("gm", None).unwrap();
        cl.feed_post("agreed", Some("abc12")).unwrap();
        let seen = f.seen.lock().unwrap();
        assert_eq!(seen[0].0, "POST");
        assert_eq!(seen[0].1, "https://h.test/api/cli/messages");
        assert_eq!(seen[0].2.as_deref(), Some("hscli_SECRET"));
        assert_eq!(seen[0].3, Some(json!({"content": "gm"})));
        assert_eq!(
            seen[1].3,
            Some(json!({"content": "agreed", "reply_to": "abc12"}))
        );
    }

    #[test]
    fn sends_hit_the_right_routes() {
        let f = Fake::new(vec![
            Ok((200, json!({"ok": true}))),
            Ok((200, json!({"success": true}))),
        ]);
        let cl = client(&f);
        cl.whisper(&c("42", "bob", false), "hi").unwrap();
        cl.dm(&c("u_7", "cat", true), "yo").unwrap();
        let seen = f.seen.lock().unwrap();
        assert_eq!(seen[0].1, "https://h.test/api/cli/twitch/whisper");
        assert_eq!(
            seen[0].3,
            Some(json!({"to_user_id": "42", "message": "hi"}))
        );
        assert_eq!(seen[1].1, "https://h.test/api/cli/dm");
        assert_eq!(seen[1].3, Some(json!({"toUserId": "u_7", "content": "yo"})));
    }

    #[test]
    fn lookup_asks_by_login_and_reads_404_as_nobody() {
        let f = Fake::new(vec![
            Ok((200, json!({"twitch_id": "42", "display_name": "Bob"}))),
            Ok((404, json!({"error": "unknown"}))),
            Ok((200, json!({"twitch_id": "42"}))),
        ]);
        let cl = client(&f);
        assert_eq!(cl.lookup("Bob", false).unwrap().unwrap().id, "42");
        assert!(cl.lookup("ghost", false).unwrap().is_none());
        // a heatsync dm needs the heatsync id: a twitch-only answer is nobody
        assert!(cl.lookup("bob", true).unwrap().is_none());
        // never put a weird name in a URL
        assert!(cl.lookup("a/../b", false).unwrap().is_none());
        assert!(cl.lookup("", false).unwrap().is_none());
        let seen = f.seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert_eq!(
            seen[0].1,
            "https://h.test/api/cli/users/lookup?login=Bob&platform=twitch"
        );
    }

    #[test]
    fn refusals_read_in_plain_words() {
        let f = Fake::new(vec![
            Ok((403, json!({"needs_consent": true}))),
            Ok((403, json!({"error_code": "scope_missing"}))),
            Ok((
                401,
                json!({"relink_required": true, "scope_pack": "whispers"}),
            )),
            Ok((403, json!({"error_code": "scope_missing"}))),
            Ok((401, json!({"error_code": "cli_token_invalid"}))),
        ]);
        let cl = client(&f);
        assert!(cl
            .feed_post("x", None)
            .unwrap_err()
            .contains("accept the terms"));
        assert!(cl.mention_hits().unwrap_err().contains("tick account"));
        let w = cl.whisper(&c("1", "a", false), "x").unwrap_err();
        assert!(
            w.contains("allow whispers") && w.contains("scopes=whispers"),
            "{w}"
        );
        assert!(cl
            .dm(&c("1", "a", true), "x")
            .unwrap_err()
            .contains("chat permission"));
        assert!(crate::hsauth::is_logged_out(
            &cl.mention_hits().unwrap_err()
        ));
    }

    #[test]
    fn history_loads_threads_for_each_conversation_and_merges_by_time() {
        let f = Fake::new(vec![
            Ok((
                200,
                json!({"conversations": [{"other_twitch_id": "42", "other_twitch_login": "bob"}]}),
            )),
            Ok((
                200,
                json!({"messages": [{"from_twitch_id": "42", "from_twitch_login": "bob", "content": "w", "created_at": "2026-10-04T20:00:10.000Z"}]}),
            )),
            Ok((
                200,
                json!({"conversations": [{"other_user_id": "12", "username": "cat"}]}),
            )),
            Ok((
                200,
                json!({"messages": [{"from_user_id": "12", "content": "d", "created_at": "2026-10-04T20:00:05.000Z"}]}),
            )),
            Ok((
                200,
                json!({"following": [{"id": "u9", "username": "streamer"}, {"id": "12", "username": "cat"}]}),
            )),
        ]);
        let (contacts, rows) = client(&f).history().unwrap();
        // 42 (whisper), 12 (dm), u9 (follow) — 12 is not doubled by the follow list
        assert_eq!(contacts.len(), 3);
        let texts: Vec<_> = rows.iter().map(|r| r.line.line.content.as_str()).collect();
        assert_eq!(texts, ["d", "w"]);
        let seen = f.seen.lock().unwrap();
        assert_eq!(seen[1].1, "https://h.test/api/cli/whispers/42?limit=15");
        assert_eq!(seen[3].1, "https://h.test/api/cli/dm/12?limit=15");
    }

    #[test]
    fn history_survives_one_half_failing_but_not_both() {
        let f = Fake::new(vec![
            Ok((200, json!({"conversations": []}))),
            Ok((403, json!({"error_code": "scope_missing"}))),
            Ok((403, json!({"error_code": "scope_missing"}))),
        ]);
        assert!(client(&f).history().is_ok());
        let g = Fake::new(vec![
            Ok((403, json!({"error_code": "scope_missing"}))),
            Ok((403, json!({"error_code": "scope_missing"}))),
            Ok((403, json!({"error_code": "scope_missing"}))),
        ]);
        assert!(client(&g).history().is_err());
    }
}
