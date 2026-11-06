//! speaking from the virtual tabs: feed posts (`/op`, `/re`, the composer on
//! the feed tab), whispers and dms (`/w`, `/dm`, `r` on a whispers row), and
//! the history that loads when the whispers / mentions tabs open. every call
//! runs on its own thread over the heatsync login and answers through
//! [`Msg`], drained once per tick.

use heatsync_core::social::{self, Contact};
use heatsync_core::{Message, Platform, TabKind, TagKind};

use crate::app::{App, Feed};
use crate::hsauth::{self, Client};
use crate::mentions::Mention;
use crate::social::Msg;
use crate::special;

const NO_LOGIN: &str = "not logged in — run: heatsync-tui login";

/// the login, when this run has one and is live (the demo never calls out).
fn client(app: &App) -> Option<Client> {
    matches!(app.feed, Feed::Live { .. })
        .then(|| app.hs.clone())
        .flatten()
}

/// run `f` on its own thread; its answer lands in [`drain`]. a dead login
/// wipes the saved session, as any other call would.
fn spawn<F>(app: &App, c: Client, f: F)
where
    F: FnOnce(&Client) -> Msg + Send + 'static,
{
    let tx = app.special.social_tx.clone();
    std::thread::spawn(move || {
        let m = f(&c);
        if let Msg::Say(t) = &m {
            if hsauth::is_logged_out(t) {
                hsauth::delete_session();
            }
        }
        let _ = tx.send(m);
    });
}

/// whispers tab opened: load conversations and the newest messages of each.
pub(crate) fn load_history(app: &mut App) {
    let Some(c) = client(app) else { return };
    spawn(app, c, |c| match c.history() {
        Ok((contacts, rows)) => Msg::History { contacts, rows },
        Err(e) => Msg::Say(e),
    });
}

/// mentions tab opened: what your rules caught while you were away.
pub(crate) fn load_hits(app: &mut App) {
    let Some(c) = client(app) else { return };
    spawn(app, c, |c| match c.mention_hits() {
        Ok(h) => Msg::Hits(h),
        Err(e) => Msg::Say(e),
    });
}

/// `follows:changed`: refetch the follow list; new people become `/dm`-able.
pub(crate) fn refresh_follows(app: &mut App) {
    if !special::refetch_due(&mut app.special.follows_at) {
        return;
    }
    let Some(c) = client(app) else { return };
    spawn(app, c, |c| match c.follow_contacts() {
        Ok(f) => Msg::Follows(f),
        Err(_) => Msg::Say(String::new()),
    });
}

/// remember who a live whisper / dm came from, so `/w` and `r` can answer them.
pub(crate) fn learn(app: &mut App, c: Contact) {
    let known = app
        .special
        .contacts
        .iter_mut()
        .find(|k| k.dm == c.dm && k.id == c.id);
    match known {
        Some(k) => *k = c,
        None => app.special.contacts.push(c),
    }
}

/// who `who` is: someone we have a thread with, or (whispers only) anyone
/// whose chat line we have seen — those carry their twitch id.
fn find(app: &App, dm: bool, who: &str) -> Option<Contact> {
    if let Some(c) = social::resolve(&app.special.contacts, dm, who) {
        return Some(c.clone());
    }
    if dm {
        return None;
    }
    let w = who.trim().trim_start_matches('@');
    app.channels
        .iter()
        .flat_map(|c| c.messages.iter().rev())
        .filter(|m| m.platform == Platform::Twitch && m.user.eq_ignore_ascii_case(w))
        .find_map(|m| {
            let id = m
                .uid
                .as_deref()
                .filter(|u| u.bytes().all(|b| b.is_ascii_digit()))?;
            Some(Contact {
                name: m.user.clone(),
                login: w.to_lowercase(),
                id: id.to_string(),
                dm: false,
            })
        })
}

/// `/w` and `/dm`. a name we don't know yet is looked up first (and
/// remembered); if that finds nobody the text goes back in the composer.
/// true = the line was taken (the composer clears).
pub(crate) fn direct(app: &mut App, dm: bool, to: &str, text: &str) -> bool {
    let Some(c) = client(app) else {
        app.status = Some(NO_LOGIN.into());
        return false;
    };
    let known = find(app, dm, to);
    let verb = if dm { "dm" } else { "whisper" };
    app.status = Some(match &known {
        Some(w) => format!("{verb} → {}", w.login),
        None => format!("looking up {to}…"),
    });
    let (to, text) = (
        to.trim().trim_start_matches('@').to_string(),
        text.to_string(),
    );
    spawn(app, c, move |c| {
        let who = match known {
            Some(w) => w,
            None => match c.lookup(&to, dm) {
                Ok(Some(w)) => w,
                Ok(None) => {
                    let side = if dm { "heatsync" } else { "twitch" };
                    return Msg::Missing(
                        format!("no {side} user {to}"),
                        format!("/{} {to} {text}", if dm { "dm" } else { "w" }),
                    );
                }
                Err(e) => return Msg::Say(e),
            },
        };
        let r = if who.dm {
            c.dm(&who, &text)
        } else {
            c.whisper(&who, &text)
        };
        match r {
            Ok(()) => Msg::Sent(who),
            Err(e) => Msg::Say(e),
        }
    });
    true
}

/// a feed post: `reply` is the base36 id being answered. true = it went.
pub(crate) fn post(app: &mut App, text: &str, reply: Option<String>) -> bool {
    let Some(c) = client(app) else {
        app.status = Some(NO_LOGIN.into());
        return false;
    };
    app.status = Some(if reply.is_some() {
        "replied → feed".into()
    } else {
        "posted → feed".into()
    });
    let text = text.to_string();
    spawn(app, c, move |c| {
        match c.feed_post(&text, reply.as_deref()) {
            Ok(()) => Msg::Say(String::new()),
            Err(e) => Msg::Say(e),
        }
    });
    true
}

/// the newest top-level post in the feed tab — what `/re` answers.
fn last_op(app: &App) -> Option<String> {
    let i = special::find(app, TabKind::Feed)?;
    app.channels[i]
        .messages
        .iter()
        .rev()
        .find(|m| {
            m.id.is_some()
                && m.tags
                    .iter()
                    .any(|t| matches!(t.kind, TagKind::Op | TagKind::ThreadOp))
        })
        .and_then(|m| m.id.clone())
}

/// `/re <text>`.
pub(crate) fn re(app: &mut App, text: &str) -> bool {
    match last_op(app) {
        Some(id) => post(app, text, Some(id)),
        None => {
            app.status = Some("no post to reply to — open /feed first".into());
            false
        }
    }
}

/// `r` on a whispers / dm row: put `/w <them> ` (or `/dm`) in the composer.
/// None = this row can't be answered.
pub(crate) fn reply_prefill(m: &Message) -> Option<String> {
    let tag = m
        .tags
        .iter()
        .find(|t| matches!(t.kind, TagKind::Whisper | TagKind::Dm))?;
    let verb = if tag.kind == TagKind::Dm { "/dm" } else { "/w" };
    let who = if m.user == "you" {
        // my own line: the tag names who it went to — `[whisper → bob]`
        tag.text
            .split_once('→')?
            .1
            .trim()
            .trim_end_matches(']')
            .trim()
            .to_string()
    } else {
        m.user.clone()
    };
    (!who.is_empty() && !who.contains(char::is_whitespace)).then(|| format!("{verb} {who} "))
}

/// finished calls, once per tick.
pub(crate) fn drain(app: &mut App) {
    while let Ok(m) = app.special.social_rx.try_recv() {
        match m {
            Msg::Say(t) if t.is_empty() => {}
            Msg::Sent(c) => learn(app, c),
            Msg::Missing(why, draft) => {
                app.status = Some(why);
                if app.line.is_empty() {
                    app.line.replace_range(0, 0, &draft);
                }
            }
            Msg::Say(t) => {
                if hsauth::is_logged_out(&t) {
                    app.hs = None;
                }
                app.status = Some(t);
            }
            Msg::History { contacts, rows } => {
                for c in contacts {
                    learn(app, c);
                }
                special::backfill_whispers(app, rows);
            }
            Msg::Follows(found) => {
                for f in found {
                    if !app.special.contacts.iter().any(|c| c.dm && c.id == f.id) {
                        learn(app, f);
                    }
                }
            }
            Msg::Hits(hits) => {
                let rows = hits
                    .into_iter()
                    .map(|h| Mention {
                        platform: h.platform,
                        channel: h.channel,
                        user: h.user,
                        color: None,
                        text: h.text,
                        id: h.id,
                        at: h.at,
                    })
                    .collect();
                special::backfill_mentions(app, rows);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hsauth::testkit::{client as fake_client, Fake};
    use heatsync_core::proto::{self, Event};
    use heatsync_core::Tag;
    use serde_json::json;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// a live-mode app whose heatsync login answers from `f`.
    fn live(f: &Arc<Fake>) -> App {
        let mut a = special::tests::app();
        a.feed = Feed::Live {
            rx: std::sync::mpsc::channel().1,
            start: Instant::now(),
            connected: true,
        };
        a.hs = Some(fake_client(f));
        a
    }

    /// the one answer the spawned call sends back.
    fn answer(a: &App) -> Msg {
        a.special
            .social_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the call answered")
    }

    fn chat_line(a: &mut App, user: &str, uid: &str) {
        let mut m = msg(user, Tag::new("", TagKind::Origin));
        m.tags.clear();
        m.uid = Some(uid.into());
        a.channels[0].deliver(m);
    }

    fn msg(user: &str, tag: Tag) -> Message {
        Message {
            platform: Platform::Twitch,
            user: user.into(),
            text: "x".into(),
            color: None,
            badges: Vec::new(),
            reply_to: None,
            id: None,
            gone: None,
            note: None,
            emotes: None,
            tags: vec![tag],
            extra: Default::default(),
            uid: None,
            heat: 0.0,
        }
    }

    #[test]
    fn r_on_a_row_fills_the_right_command() {
        let w = msg("Bob", Tag::new("[whisper]", TagKind::Whisper));
        assert_eq!(reply_prefill(&w).as_deref(), Some("/w Bob "));
        let d = msg("Cat", Tag::new("[DM]", TagKind::Dm));
        assert_eq!(reply_prefill(&d).as_deref(), Some("/dm Cat "));
        // my own line answers the person it went to
        let mine = msg("you", Tag::new("[whisper → bob]", TagKind::Whisper));
        assert_eq!(reply_prefill(&mine).as_deref(), Some("/w bob "));
        let mine_dm = msg("you", Tag::new("[DM → cat]", TagKind::Dm));
        assert_eq!(reply_prefill(&mine_dm).as_deref(), Some("/dm cat "));
        // not a whisper row at all
        assert_eq!(
            reply_prefill(&msg("x", Tag::new("[OP]", TagKind::Op))),
            None
        );
        // a name with a space can't be typed back as a command
        assert_eq!(
            reply_prefill(&msg("two words", Tag::new("[whisper]", TagKind::Whisper))),
            None
        );
    }

    #[test]
    fn w_finds_the_id_from_a_chat_line_on_screen_and_sends_it() {
        let f = Fake::new(vec![Ok((200, json!({"ok": true})))]);
        let mut a = live(&f);
        chat_line(&mut a, "Bob", "42");
        assert!(direct(&mut a, false, "@bob", "hi"));
        assert!(matches!(answer(&a), Msg::Sent(c) if c.id == "42"));
        let seen = f.seen.lock().unwrap();
        assert_eq!(seen[0].1, "https://h.test/api/cli/twitch/whisper");
        assert_eq!(
            seen[0].3,
            Some(json!({"to_user_id": "42", "message": "hi"}))
        );
    }

    #[test]
    fn w_to_a_stranger_looks_them_up_and_remembers_them() {
        let f = Fake::new(vec![
            Ok((200, json!({"twitch_id": "77", "display_name": "Ghost"}))),
            Ok((200, json!({"ok": true}))),
            Ok((200, json!({"ok": true}))),
        ]);
        let mut a = live(&f);
        assert!(direct(&mut a, false, "ghost", "hi"));
        assert!(matches!(answer(&a), Msg::Sent(_)));
        drain(&mut a); // nothing left; Sent was taken by answer(), so learn by hand
        learn(&mut a, find_sent(&f));
        // the second whisper needs no lookup
        assert!(direct(&mut a, false, "ghost", "again"));
        answer(&a);
        let seen = f.seen.lock().unwrap();
        assert!(seen[0]
            .1
            .contains("/users/lookup?login=ghost&platform=twitch"));
        assert_eq!(
            seen[1].3,
            Some(json!({"to_user_id": "77", "message": "hi"}))
        );
        assert_eq!(seen[2].1, "https://h.test/api/cli/twitch/whisper");
    }

    /// the contact the first send went to, rebuilt from its request body.
    fn find_sent(f: &Arc<Fake>) -> Contact {
        let id = f.seen.lock().unwrap()[1].3.as_ref().unwrap()["to_user_id"]
            .as_str()
            .unwrap()
            .to_string();
        Contact {
            name: "Ghost".into(),
            login: "ghost".into(),
            id,
            dm: false,
        }
    }

    #[test]
    fn an_unknown_name_gives_the_draft_back() {
        let f = Fake::new(vec![Ok((404, json!({}))), Ok((404, json!({})))]);
        let mut a = live(&f);
        assert!(direct(&mut a, false, "ghost", "hi there"));
        let m = answer(&a);
        a.special.social_tx.send(m).unwrap();
        drain(&mut a);
        assert_eq!(a.status.as_deref(), Some("no twitch user ghost"));
        assert_eq!(a.line.text(), "/w ghost hi there");
        a.line.accept();
        assert!(direct(&mut a, true, "ghost", "yo"));
        let m = answer(&a);
        a.special.social_tx.send(m).unwrap();
        drain(&mut a);
        assert_eq!(a.status.as_deref(), Some("no heatsync user ghost"));
        assert_eq!(a.line.text(), "/dm ghost yo");
    }

    #[test]
    fn dm_goes_to_a_known_heatsync_user_and_a_refusal_reaches_the_status_bar() {
        let f = Fake::new(vec![Ok((403, json!({"needs_consent": true})))]);
        let mut a = live(&f);
        learn(
            &mut a,
            Contact {
                name: "Cat".into(),
                login: "cat".into(),
                id: "u_12".into(),
                dm: true,
            },
        );
        assert!(direct(&mut a, true, "cat", "yo"));
        let Msg::Say(t) = answer(&a) else {
            panic!("say")
        };
        assert!(t.contains("accept the terms"));
        assert_eq!(
            f.seen.lock().unwrap()[0].3,
            Some(json!({"toUserId": "u_12", "content": "yo"}))
        );
    }

    #[test]
    fn a_post_and_a_reply_use_the_documented_body() {
        let f = Fake::new(vec![Ok((200, json!({}))), Ok((200, json!({})))]);
        let mut a = live(&f);
        assert!(post(&mut a, "gm", None));
        answer(&a);
        assert!(post(&mut a, "same", Some("abc12".into())));
        answer(&a);
        let seen = f.seen.lock().unwrap();
        assert_eq!(seen[0].3, Some(json!({"content": "gm"})));
        assert_eq!(
            seen[1].3,
            Some(json!({"content": "same", "reply_to": "abc12"}))
        );
    }

    #[test]
    fn nothing_is_sent_without_a_login() {
        let mut a = special::tests::app();
        assert!(!post(&mut a, "gm", None));
        assert!(!direct(&mut a, false, "bob", "hi"));
        assert!(a.status.as_deref().unwrap().contains("heatsync-tui login"));
    }

    #[test]
    fn follows_changed_teaches_new_dm_contacts_without_duplicating() {
        let f = Fake::new(vec![Ok((
            200,
            json!({"following": [{"id": "u9", "username": "streamer"}, {"id": "12", "username": "cat"}]}),
        ))]);
        let mut a = live(&f);
        learn(
            &mut a,
            social::Contact {
                name: "cat".into(),
                login: "cat".into(),
                id: "12".into(),
                dm: true,
            },
        );
        refresh_follows(&mut a);
        refresh_follows(&mut a); // inside the gap: one request
        let m = answer(&a);
        assert!(matches!(&m, Msg::Follows(c) if c.len() == 2));
        a.special.social_tx.send(m).unwrap();
        drain(&mut a);
        let ids: Vec<_> = a.special.contacts.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["12", "u9"]);
        assert_eq!(f.seen.lock().unwrap().len(), 1);
    }

    fn feed_row(id: &str, reply: bool) -> Event {
        proto::parse(
            &json!({"type": "new-message", "base36_id": id, "content": "x", "username": "u",
                    "is_op": !reply, "platform": "twitch"})
            .to_string(),
        )
    }

    #[test]
    fn re_answers_the_newest_top_level_post_not_a_reply() {
        let mut a = special::tests::app();
        let i = special::add(&mut a, TabKind::Feed);
        for (id, reply) in [("op1", false), ("op2", false), ("re1", true)] {
            let Event::Special(s) = feed_row(id, reply) else {
                panic!("row")
            };
            special::event(&mut a, crate::net::ChatEvent::Special(s));
        }
        assert_eq!(a.channels[i].messages.len(), 3);
        assert_eq!(last_op(&a).as_deref(), Some("op2"));
        // no feed tab → nothing to answer
        let mut b = special::tests::app();
        assert!(!re(&mut b, "x"));
        assert!(b.status.as_deref().unwrap().contains("open /feed"));
    }

    #[test]
    fn history_teaches_contacts_and_fills_the_whispers_tab_once() {
        let mut a = special::tests::app();
        let i = special::add(&mut a, TabKind::Whispers);
        let row = |at: i64, text: &str| heatsync_core::social::Row {
            at,
            line: {
                let Event::Special(s) = proto::parse(
                    &json!({"type": "whisper:new", "from_twitch_id": "42", "from_twitch_login": "bob",
                            "from_display_name": "Bob", "content": text})
                    .to_string(),
                ) else {
                    panic!("row")
                };
                s
            },
        };
        // a live whisper arrived before the history did
        let Event::Special(live_row) = proto::parse(
            r#"{"type":"whisper:new","from_twitch_id":"42","from_twitch_login":"bob","from_display_name":"Bob","content":"new"}"#,
        ) else {
            panic!("row")
        };
        special::event(&mut a, crate::net::ChatEvent::Special(live_row));
        a.special
            .social_tx
            .send(Msg::History {
                contacts: vec![],
                rows: vec![row(1, "old"), row(2, "new")],
            })
            .unwrap();
        drain(&mut a);
        let texts: Vec<_> = a.channels[i]
            .messages
            .iter()
            .map(|m| m.text.as_str())
            .collect();
        assert_eq!(texts, ["old", "new"]); // history first, the live row not doubled
        assert_eq!(find(&a, false, "bob").unwrap().id, "42"); // learned from the live frame
    }

    #[test]
    fn server_hits_join_the_log_once_and_never_duplicate_a_local_mention() {
        let mut a = special::tests::app();
        let i = special::add(&mut a, TabKind::Mentions);
        let local = Mention {
            platform: Platform::Twitch,
            channel: "xqc".into(),
            user: "bob".into(),
            color: None,
            text: "hey mellen long message here".into(),
            id: None,
            at: 100,
        };
        special::mention(&mut a, local);
        let hit = |user: &str, text: &str, id: &str| heatsync_core::social::Hit {
            id: Some(id.into()),
            platform: Platform::Twitch,
            channel: "xqc".into(),
            user: user.into(),
            text: text.into(),
            at: 0,
        };
        // newest first, as the server lists them; the first is the one we already have
        a.special
            .social_tx
            .send(Msg::Hits(vec![
                hit("amy", "mellen?", "m3"),
                hit("bob", "hey mellen long", "m1"),
            ]))
            .unwrap();
        drain(&mut a);
        assert_eq!(a.channels[i].messages.len(), 2);
        // amy's hit is older than the local line (at 0 < 100): it sits before it
        assert_eq!(a.channels[i].messages[0].user, "amy");
        // opening again brings the same hits: nothing doubles
        a.special
            .social_tx
            .send(Msg::Hits(vec![hit("amy", "mellen?", "m3")]))
            .unwrap();
        drain(&mut a);
        assert_eq!(a.channels[i].messages.len(), 2);
        assert_eq!(a.special.log.rows().count(), 2);
    }
}
