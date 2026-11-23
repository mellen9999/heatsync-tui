//! your own heatsync emotes from the keyboard: `/emote add|rm|mv|undo` and the
//! rows behind `/emotes`. every call goes through the heatsync login to the
//! `/api/cli/emotes` routes (scope `account`), which run the same code as the
//! site, so slot limits, one-emote-per-name and the cdn allowlist are the
//! server's to enforce. nothing is cached: `rm` and `mv` read your list fresh,
//! so a stale name can never remove the wrong slot.

use crate::app::{App, Feed};
use crate::hsauth::{self, explain, Client, Outcome};
use heatsync_core::emote::Emote;
use heatsync_core::i18n::{tr, trf};
use heatsync_core::slash::EmoteOp;
use serde_json::{json, Value};

/// the server's inventory is 5000 slots; a longer list is hostile.
const MAX_ROWS: usize = 5000;
/// the table's own slot bound.
const MAX_SLOT: u64 = 50_000;
/// the longest name the server keeps.
const MAX_NAME: usize = 100;

/// one row of your inventory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slot {
    pub slot: u32,
    pub name: String,
    pub source: String,
}

/// `GET /api/cli/emotes` → rows by slot. a row with no slot or no name is
/// dropped, names lose control bytes, and the list is capped.
pub fn parse_slots(v: &Value) -> Vec<Slot> {
    let rows = v.get("emotes").and_then(Value::as_array);
    let mut out: Vec<Slot> = rows
        .into_iter()
        .flatten()
        .take(MAX_ROWS)
        .filter_map(|r| {
            let slot = r.get("slot_number")?.as_u64().filter(|s| *s <= MAX_SLOT)? as u32;
            let s = |k: &str| r.get(k).and_then(Value::as_str);
            let name: String = s("custom_name")
                .or_else(|| s("name"))?
                .chars()
                .filter(|c| !c.is_control())
                .take(MAX_NAME)
                .collect();
            if name.trim().is_empty() {
                return None;
            }
            let source: String = s("source")
                .unwrap_or_default()
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .take(32)
                .collect();
            Some(Slot { slot, name, source })
        })
        .collect();
    out.sort_by_key(|s| s.slot);
    out.dedup_by_key(|s| s.slot);
    out
}

/// the slot that holds exactly this name.
pub fn slot_of(rows: &[Slot], name: &str) -> Option<u32> {
    rows.iter().find(|s| s.name == name).map(|s| s.slot)
}

/// a name that can be an emote word: one token, a sane length.
pub fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.chars().count() <= MAX_NAME
        && !n.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// the add request body for an emote as the focused tab draws it.
pub fn add_body(e: &Emote) -> Value {
    let mut b = json!({
        "emoteUrl": e.url,
        "emoteName": e.name,
        "customName": e.name,
        "source": if e.provider.is_empty() { "unknown" } else { e.provider.as_str() },
        "zeroWidth": e.zero_width,
    });
    if !e.id.is_empty() && e.id.len() <= 200 {
        b["sourceId"] = json!(e.id);
    }
    b
}

/// server refusals that are codes, not words.
fn plain(err: &str) -> Option<&'static str> {
    match err {
        "slot_limit_reached" => Some(tr("inventory.slots_full")),
        _ => None,
    }
}

/// a non-2xx answer → plain words. a missing `account` permission names the fix.
fn refused(st: u16, v: &Value, base: &str) -> String {
    if hsauth::scope_note(v, "account").is_some() {
        return tr("inventory.no_scope").into();
    }
    let err = v.get("error").and_then(Value::as_str).unwrap_or("");
    // the router's own 404 (no such route) is `{"error":"not found","path":…}`:
    // this server does not have the routes yet. a handler's 404 about a slot
    // has its own words and no `path`.
    if st == 404 && (err.is_empty() || v.get("path").is_some()) {
        return tr("inventory.not_deployed").into();
    }
    match plain(err) {
        Some(w) => w.into(),
        None => explain(st, v, base),
    }
}

impl Client {
    fn emote_call(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value, String> {
        match self.call(method, path, body)? {
            (200, v) => Ok(v),
            (st, v) => Err(refused(st, &v, self.base())),
        }
    }

    /// your inventory, by slot.
    pub fn emotes(&self) -> Result<Vec<Slot>, String> {
        self.emote_call("GET", "/api/cli/emotes", None)
            .map(|v| parse_slots(&v))
    }

    /// add the emote as it is drawn: what the server did with it.
    pub fn emote_add(&self, e: &Emote) -> Result<Added, String> {
        let v = self.emote_call("POST", "/api/cli/emotes", Some(add_body(e)))?;
        Ok(added_from(&v, &e.name))
    }

    pub fn emote_rename(&self, slot: u32, new: &str) -> Result<String, String> {
        let v = self.emote_call(
            "PUT",
            &format!("/api/cli/emotes/{slot}/rename"),
            Some(json!({ "newName": new })),
        )?;
        Ok(echoed(&v, new))
    }

    pub fn emote_remove(&self, slot: u32) -> Result<(), String> {
        self.emote_call("DELETE", &format!("/api/cli/emotes/{slot}"), None)
            .map(drop)
    }

    /// undo the last change; what the server says it did, when it says.
    pub fn emote_undo(&self) -> Result<Option<String>, String> {
        let v = self.emote_call("POST", "/api/cli/emotes/undo", None)?;
        Ok(v.get("action")
            .and_then(Value::as_str)
            .map(|a| a.chars().filter(|c| !c.is_control()).take(80).collect()))
    }
}

/// what an add did: the name it landed under, the emote a full inventory gave
/// up to make room, or that it was already there.
#[derive(Debug, PartialEq, Eq)]
pub struct Added {
    pub name: String,
    pub replaced: Option<String>,
    pub already: bool,
}

fn added_from(v: &Value, fallback: &str) -> Added {
    let clean = |s: &str| -> String {
        s.chars()
            .filter(|c| !c.is_control())
            .take(MAX_NAME)
            .collect()
    };
    Added {
        name: echoed(v, fallback),
        replaced: v
            .get("evicted")
            .and_then(|e| e.get("name"))
            .and_then(Value::as_str)
            .map(clean)
            .filter(|n| !n.is_empty()),
        already: v.get("alreadyExists").and_then(Value::as_bool) == Some(true),
    }
}

/// an undo that reverted many emotes at once ("removed 300 imported emotes"):
/// this client has no redo for those.
fn bulk(what: &str) -> bool {
    what.split_whitespace()
        .nth(1)
        .and_then(|n| n.parse::<u32>().ok())
        .is_some_and(|n| n > 1)
}

/// the name the server kept (it cleans names), else the one we sent.
fn echoed(v: &Value, fallback: &str) -> String {
    v.get("name")
        .and_then(Value::as_str)
        .filter(|n| valid_name(n))
        .unwrap_or(fallback)
        .to_string()
}

fn done(text: String) -> Outcome {
    Outcome { ok: true, text }
}

fn failed(text: String) -> Outcome {
    Outcome { ok: false, text }
}

/// run one change against the account and say what happened. blocking: callers
/// keep it off the UI thread. `add` is the emote the name draws as (None = the
/// tab draws nothing by that name).
pub fn run_op(c: &Client, op: &EmoteOp, add: Option<&Emote>) -> Outcome {
    match op {
        EmoteOp::Add(name) => match add {
            None => failed(trf("inventory.add_no_emote", &[("name", name)])),
            Some(e) => match c.emote_add(e) {
                Ok(a) if a.already => done(trf("inventory.already_yours", &[("name", &a.name)])),
                Ok(Added {
                    name,
                    replaced: Some(old),
                    ..
                }) => done(trf(
                    "inventory.added_replaced",
                    &[("name", &name), ("old", &old)],
                )),
                Ok(a) => done(trf("inventory.added", &[("name", &a.name)])),
                Err(w) => failed(trf("inventory.add_failed", &[("name", name), ("why", &w)])),
            },
        },
        EmoteOp::Rm(name) => {
            let go = || -> Result<(), String> {
                let rows = c.emotes()?;
                let slot = slot_of(&rows, name)
                    .ok_or_else(|| trf("inventory.not_yours", &[("name", name)]))?;
                c.emote_remove(slot)
            };
            match go() {
                Ok(()) => done(trf("inventory.removed", &[("name", name)])),
                Err(w) => failed(trf("inventory.rm_failed", &[("name", name), ("why", &w)])),
            }
        }
        EmoteOp::Mv(old, new) => {
            if !valid_name(new) {
                return failed(trf("inventory.bad_name", &[("name", new)]));
            }
            let go = || -> Result<String, String> {
                let rows = c.emotes()?;
                let slot = slot_of(&rows, old)
                    .ok_or_else(|| trf("inventory.not_yours", &[("name", old)]))?;
                c.emote_rename(slot, new)
            };
            match go() {
                Ok(n) => done(trf("inventory.renamed", &[("old", old), ("new", &n)])),
                Err(w) => failed(trf("inventory.mv_failed", &[("name", old), ("why", &w)])),
            }
        }
        EmoteOp::Undo => match c.emote_undo() {
            Ok(Some(what)) if bulk(&what) => done(trf("inventory.undone_bulk", &[("what", &what)])),
            Ok(Some(what)) => done(trf("inventory.undone", &[("what", &what)])),
            Ok(None) => done(tr("inventory.undone_plain").into()),
            Err(w) => failed(trf("inventory.undo_failed", &[("why", &w)])),
        },
    }
}

/// the login, when this run has one and is live (the demo never calls out).
pub(crate) fn client(app: &App) -> Option<Client> {
    matches!(app.feed, Feed::Live { .. })
        .then(|| app.hs.clone())
        .flatten()
}

/// the lines whose emotes `/emote add` looks through (the screen's backlog).
const RECENT: usize = 60;

/// the emote `name` draws as, by the law's own reading: a pick a recent line
/// carries (newest first) beats the tab's set; the set beats a sender's guess;
/// a sender's own emote is what's left. a taken-down pick draws nothing and is
/// never added.
pub fn drawn_as<'a>(
    set: &'a heatsync_core::emote::EmoteSet,
    recent: impl Iterator<Item = &'a heatsync_core::emote::EmoteSet>,
    name: &str,
) -> Option<Emote> {
    use heatsync_core::emote::RefKind;
    let mut sender = None;
    for e in recent.filter_map(|r| r.get(name)) {
        if e.kind == RefKind::Gone || e.url.is_empty() {
            continue;
        }
        if e.kind == RefKind::Pick {
            return Some(e.clone());
        }
        sender.get_or_insert(e);
    }
    set.get(name).or(sender).cloned()
}

/// `/emote …`: checks, then the call on its own thread; the words land as the
/// tab's note, like a mod action's.
pub(crate) fn emote_cmd(app: &mut App, op: EmoteOp) {
    let Some(c) = client(app) else {
        app.status = Some(tr("input.talk_no_login").into());
        return;
    };
    let Some(ch) = app.channels.get(app.focus) else {
        return;
    };
    // the emote a name draws as in this tab: its set, else one a recent line
    // brought along (a sender's own emote is never in the set)
    let drawn = match (&op, app.emotes.get(app.focus)) {
        (EmoteOp::Add(n), Some(set)) => drawn_as(
            set,
            ch.messages.iter().rev().take(RECENT).map(|m| m.refs()),
            n,
        ),
        _ => None,
    };
    let (platform, name) = (ch.platform, ch.name.clone());
    hsauth::spawn_note(app.hs_tx.clone(), platform, name, move || {
        run_op(&c, &op, drawn.as_ref())
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hsauth::testkit::{client, Fake};

    fn emote(name: &str) -> Emote {
        Emote {
            name: name.into(),
            url: "https://cdn.7tv.app/emote/01ABC/2x.webp".into(),
            provider: "7tv".into(),
            id: "01ABC".into(),
            animated: false,
            zero_width: false,
            global: false,
            kind: Default::default(),
        }
    }

    fn list(rows: &[(u32, &str)]) -> Value {
        json!({"emotes": rows.iter().map(|(s, n)| json!({"slot_number": s, "custom_name": n, "source": "7tv"})).collect::<Vec<_>>()})
    }

    fn seen(f: &Fake) -> Vec<(String, String, Option<Value>)> {
        f.seen
            .lock()
            .unwrap()
            .iter()
            .map(|(m, u, _, b)| (m.clone(), u.replace("https://h.test", ""), b.clone()))
            .collect()
    }

    #[test]
    fn rows_are_read_bounded_and_clean() {
        let v = json!({"emotes": [
            {"slot_number": 9, "custom_name": "b\u{1b}ad", "source": "7tv"},
            {"slot_number": 2, "name": "plain", "source": "bt tv!"},
            {"slot_number": 2, "custom_name": "dupe"},
            {"slot_number": 999999, "custom_name": "huge"},
            {"custom_name": "noslot"},
            {"slot_number": 3},
            {"slot_number": 4, "custom_name": "   "},
            7, null
        ]});
        let rows = parse_slots(&v);
        assert_eq!(
            rows,
            vec![
                Slot {
                    slot: 2,
                    name: "plain".into(),
                    source: "bttv".into()
                },
                Slot {
                    slot: 9,
                    name: "bad".into(),
                    source: "7tv".into()
                },
            ]
        );
        let many = json!({"emotes": (0..MAX_ROWS + 50).map(|i| json!({"slot_number": i, "custom_name": format!("n{i}")})).collect::<Vec<_>>()});
        assert_eq!(parse_slots(&many).len(), MAX_ROWS);
        assert!(parse_slots(&json!({"emotes": "x"})).is_empty());
    }

    #[test]
    fn the_add_body_is_the_sites_add() {
        let b = add_body(&emote("catJAM"));
        assert_eq!(b["emoteUrl"], "https://cdn.7tv.app/emote/01ABC/2x.webp");
        assert_eq!(
            (b["emoteName"].clone(), b["customName"].clone()),
            (json!("catJAM"), json!("catJAM"))
        );
        assert_eq!(
            (b["source"].clone(), b["sourceId"].clone()),
            (json!("7tv"), json!("01ABC"))
        );
        let mut e = emote("x");
        e.provider.clear();
        e.id.clear();
        let b = add_body(&e);
        assert_eq!(b["source"], "unknown");
        assert!(b.get("sourceId").is_none());
    }

    #[test]
    fn add_says_what_it_added_and_posts_the_emote() {
        let f = Fake::new(vec![Ok((
            200,
            json!({"success": true, "slot": 4, "name": "catJAM"}),
        ))]);
        let o = run_op(
            &client(&f),
            &EmoteOp::Add("catJAM".into()),
            Some(&emote("catJAM")),
        );
        assert_eq!(
            o,
            Outcome {
                ok: true,
                text: "added catJAM to your emotes".into()
            }
        );
        let s = seen(&f);
        assert_eq!(
            (s[0].0.as_str(), s[0].1.as_str()),
            ("POST", "/api/cli/emotes")
        );
        assert_eq!(s[0].2.as_ref().unwrap()["emoteName"], "catJAM");
    }

    fn add_answer(body: Value) -> Outcome {
        let f = Fake::new(vec![Ok((200, body))]);
        run_op(
            &client(&f),
            &EmoteOp::Add("catJAM".into()),
            Some(&emote("catJAM")),
        )
    }

    #[test]
    fn a_full_inventory_says_which_emote_made_room() {
        let o = add_answer(
            json!({"success": true, "slot": 4, "name": "catJAM", "evicted": {"slot": 9, "name": "old\u{1b}one"}}),
        );
        assert!(o.ok);
        assert_eq!(
            o.text,
            "added catJAM to your emotes (replaced oldone — your slots were full)"
        );
        // no eviction: the plain line
        let o = add_answer(json!({"success": true, "slot": 4, "name": "catJAM", "evicted": null}));
        assert_eq!(o.text, "added catJAM to your emotes");
    }

    #[test]
    fn an_emote_you_already_had_is_not_reported_as_added() {
        let o = add_answer(
            json!({"success": true, "slot": 4, "name": "catJAM", "alreadyExists": true}),
        );
        assert!(o.ok);
        assert_eq!(o.text, "catJAM was already yours");
    }

    #[test]
    fn a_server_without_the_routes_says_so_not_404() {
        for call in [
            EmoteOp::Add("catJAM".into()),
            EmoteOp::Rm("catJAM".into()),
            EmoteOp::Mv("catJAM".into(), "x".into()),
            EmoteOp::Undo,
        ] {
            // the server's real body for a missing route (server.ts onError)
            let f = Fake::new(vec![Ok((
                404,
                json!({"error": "not found", "path": "/api/cli/emotes"}),
            ))]);
            let o = run_op(&client(&f), &call, Some(&emote("catJAM")));
            assert!(
                !o.ok && o.text.contains("can't manage emotes yet"),
                "{}",
                o.text
            );
            assert!(!o.text.contains("404"), "{}", o.text);
        }
        // a handler's "not found" about a slot is not a missing route
        let f = Fake::new(vec![Ok((404, json!({"error": "Emote not found in slot"})))]);
        let o = run_op(&client(&f), &EmoteOp::Undo, None);
        assert!(!o.text.contains("can't manage"), "{}", o.text);
        // a handler's own 404 keeps its words
        let f = Fake::new(vec![Ok((404, json!({"error": "no emote in that slot"})))]);
        let o = run_op(&client(&f), &EmoteOp::Undo, None);
        assert!(o.text.contains("no emote in that slot"), "{}", o.text);
    }

    #[test]
    fn undo_does_not_promise_a_redo_it_cannot_give() {
        let f = Fake::new(vec![Ok((
            200,
            json!({"success": true, "action": "removed 300 imported emotes"}),
        ))]);
        let o = run_op(&client(&f), &EmoteOp::Undo, None);
        assert_eq!(
            o.text,
            "undid: removed 300 imported emotes (redo on heatsync.org — this terminal has no redo)"
        );
        let f = Fake::new(vec![Ok((
            200,
            json!({"success": true, "action": "removed Pog"}),
        ))]);
        assert_eq!(
            run_op(&client(&f), &EmoteOp::Undo, None).text,
            "undid: removed Pog"
        );
    }

    #[test]
    fn a_senders_own_emote_in_a_recent_line_can_be_added() {
        use heatsync_core::emote::EmoteSet;
        let room = EmoteSet::from_list([emote("Kappa")]);
        let line = EmoteSet::from_list([emote("TheirOwn")]);
        let older = EmoteSet::from_list([Emote {
            url: "https://cdn.7tv.app/emote/OLD/2x.webp".into(),
            ..emote("TheirOwn")
        }]);
        // the set misses, the newest line that has it answers
        let got = drawn_as(&room, [&line, &older].into_iter(), "TheirOwn").unwrap();
        assert_eq!(got.url, line.get("TheirOwn").unwrap().url);
        // the set wins when it has the name
        assert_eq!(
            drawn_as(&room, [&line].into_iter(), "Kappa").unwrap().name,
            "Kappa"
        );
        assert!(drawn_as(&room, [&line].into_iter(), "Nope").is_none());
    }

    #[test]
    fn add_takes_what_a_visible_line_draws_by_the_laws_order() {
        use heatsync_core::emote::{EmoteSet, RefKind};
        let url = |u: &str| Emote {
            url: u.into(),
            ..emote("Pog")
        };
        let room = EmoteSet::from_list([url("https://room/pog")]);
        let with = |e: Emote| EmoteSet::from_list([e]);
        let pick = with(Emote {
            kind: RefKind::Pick,
            ..url("https://pick/pog")
        });
        let guess = with(url("https://sender/pog")); // a relay guess: below the room
        let gone = with(Emote::gone("Pog").unwrap());
        // a line's pick beats the room, even when a newer line only guessed
        let got =
            |lines: &[&EmoteSet]| drawn_as(&room, lines.iter().copied(), "Pog").map(|e| e.url);
        assert_eq!(got(&[&guess, &pick]).as_deref(), Some("https://pick/pog"));
        // a sender's guess loses to the room
        assert_eq!(got(&[&guess]).as_deref(), Some("https://room/pog"));
        // a taken-down pick is never added: not an empty url, not the pick
        assert_eq!(got(&[&gone]).as_deref(), Some("https://room/pog"));
        let bare = EmoteSet::new();
        assert!(drawn_as(&bare, [&gone].into_iter(), "Pog").is_none());
    }

    #[test]
    fn add_of_a_word_the_tab_does_not_draw_never_calls_out() {
        let f = Fake::new(vec![]);
        let o = run_op(&client(&f), &EmoteOp::Add("nope".into()), None);
        assert!(
            !o.ok && o.text.contains("nope") && o.text.contains("isn't an emote"),
            "{}",
            o.text
        );
        assert!(seen(&f).is_empty());
    }

    #[test]
    fn every_add_refusal_is_plain_words() {
        for (st, body, want) in [
            (
                409,
                json!({"error": "emote name already in inventory", "slot": 3}),
                "already in inventory",
            ),
            (
                403,
                json!({"error": "slot_limit_reached", "max": 5000}),
                "slots are full",
            ),
            (
                400,
                json!({"error": "global emotes cannot be added to personal inventory"}),
                "global emotes",
            ),
            (
                400,
                json!({"error": "emote URL must be from a known CDN"}),
                "known CDN",
            ),
            (
                403,
                json!({"error": "x", "error_code": "scope_missing"}),
                "tick account",
            ),
            (
                401,
                json!({"error": "x", "error_code": "cli_token_invalid"}),
                "heatsync-tui login",
            ),
            (429, json!({"error": "too many"}), "too fast"),
            (500, json!({"error": "boom with internals"}), "heatsync"),
        ] {
            let f = Fake::new(vec![Ok((st, body))]);
            let o = run_op(
                &client(&f),
                &EmoteOp::Add("catJAM".into()),
                Some(&emote("catJAM")),
            );
            assert!(!o.ok, "{st}");
            assert!(
                o.text.starts_with("couldn't add catJAM")
                    || hsauth::is_logged_out(&o.text)
                    || o.text.contains("login"),
                "{}",
                o.text
            );
            assert!(o.text.contains(want), "{st}: {}", o.text);
            assert!(!o.text.contains("internals"), "{}", o.text);
        }
    }

    #[test]
    fn offline_is_said_not_swallowed() {
        let f = Fake::new(vec![Err("can't reach heatsync".into())]);
        let o = run_op(&client(&f), &EmoteOp::Add("a".into()), Some(&emote("a")));
        assert!(
            !o.ok && o.text.contains("can't reach heatsync"),
            "{}",
            o.text
        );
    }

    #[test]
    fn rm_finds_the_slot_by_exact_name_then_deletes_it() {
        let f = Fake::new(vec![
            Ok((200, list(&[(3, "Kappa"), (7, "kappa")]))),
            Ok((200, json!({"success": true, "slot": 7}))),
        ]);
        let o = run_op(&client(&f), &EmoteOp::Rm("kappa".into()), None);
        assert_eq!(
            o.text,
            "removed kappa (/emote undo takes back the last change)"
        );
        let s = seen(&f);
        assert_eq!(
            (s[0].0.as_str(), s[0].1.as_str()),
            ("GET", "/api/cli/emotes")
        );
        assert_eq!(
            (s[1].0.as_str(), s[1].1.as_str()),
            ("DELETE", "/api/cli/emotes/7")
        );
    }

    #[test]
    fn rm_of_something_you_do_not_have_deletes_nothing() {
        let f = Fake::new(vec![Ok((200, list(&[(3, "Kappa")])))]);
        let o = run_op(&client(&f), &EmoteOp::Rm("Pog".into()), None);
        assert!(
            !o.ok && o.text.contains("you don't have an emote called Pog"),
            "{}",
            o.text
        );
        assert_eq!(seen(&f).len(), 1);
    }

    #[test]
    fn rm_reports_a_failed_delete_and_a_failed_list() {
        let f = Fake::new(vec![
            Ok((200, list(&[(3, "a")]))),
            Ok((404, json!({"error": "Emote not found in slot"}))),
        ]);
        let o = run_op(&client(&f), &EmoteOp::Rm("a".into()), None);
        assert!(
            !o.ok && o.text.contains("couldn't remove a") && o.text.contains("not found"),
            "{}",
            o.text
        );
        let f = Fake::new(vec![Ok((500, json!({})))]);
        let o = run_op(&client(&f), &EmoteOp::Rm("a".into()), None);
        assert!(!o.ok && o.text.contains("couldn't remove a"), "{}", o.text);
    }

    #[test]
    fn mv_renames_the_slot_and_shows_the_name_the_server_kept() {
        let f = Fake::new(vec![
            Ok((200, list(&[(5, "old")]))),
            Ok((200, json!({"success": true, "slot": 5, "name": "New"}))),
        ]);
        let o = run_op(&client(&f), &EmoteOp::Mv("old".into(), "new".into()), None);
        assert_eq!(o.text, "renamed old → New");
        let s = seen(&f);
        assert_eq!(
            (s[1].0.as_str(), s[1].1.as_str()),
            ("PUT", "/api/cli/emotes/5/rename")
        );
        assert_eq!(s[1].2, Some(json!({"newName": "new"})));
    }

    #[test]
    fn mv_refuses_a_bad_new_name_and_a_taken_one_in_words() {
        let f = Fake::new(vec![]);
        let o = run_op(
            &client(&f),
            &EmoteOp::Mv("a".into(), "two words".into()),
            None,
        );
        assert!(!o.ok && o.text.contains("one word"), "{}", o.text);
        assert!(seen(&f).is_empty());
        let f = Fake::new(vec![
            Ok((200, list(&[(5, "a")]))),
            Ok((409, json!({"error": "Name already used in another slot"}))),
        ]);
        let o = run_op(&client(&f), &EmoteOp::Mv("a".into(), "b".into()), None);
        assert!(
            !o.ok && o.text.contains("couldn't rename a") && o.text.contains("already used"),
            "{}",
            o.text
        );
        let f = Fake::new(vec![Ok((200, list(&[])))]);
        let o = run_op(&client(&f), &EmoteOp::Mv("a".into(), "b".into()), None);
        assert!(
            o.text.contains("you don't have an emote called a"),
            "{}",
            o.text
        );
    }

    #[test]
    fn undo_says_what_it_undid_or_why_not() {
        let f = Fake::new(vec![Ok((
            200,
            json!({"success": true, "action": "removed \"catJAM\"", "canUndo": false}),
        ))]);
        let o = run_op(&client(&f), &EmoteOp::Undo, None);
        assert_eq!(o.text, "undid: removed \"catJAM\"");
        assert_eq!(seen(&f)[0].0, "POST");
        let f = Fake::new(vec![Ok((200, json!({"success": true})))]);
        assert_eq!(
            run_op(&client(&f), &EmoteOp::Undo, None).text,
            "undid your last emote change"
        );
        let f = Fake::new(vec![Ok((400, json!({"error": "nothing to undo"})))]);
        let o = run_op(&client(&f), &EmoteOp::Undo, None);
        assert!(!o.ok && o.text.contains("nothing to undo"), "{}", o.text);
    }

    #[test]
    fn the_token_never_reaches_a_message() {
        let f = Fake::new(vec![Ok((
            401,
            json!({"error": "hscli_SECRET bad", "error_code": "cli_token_invalid"}),
        ))]);
        let o = run_op(&client(&f), &EmoteOp::Undo, None);
        assert!(!o.text.contains("SECRET"), "{}", o.text);
    }
}
