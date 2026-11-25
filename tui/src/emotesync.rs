//! the app side of [`crate::refresh`]: which rooms are open, starting their
//! fetches, and swapping a finished set into every tab that holds it.

use std::time::Instant;

use crate::app::{App, Feed};
use crate::feed::request_stacks;
use crate::http;
use crate::refresh::{self, Key};
use heatsync_core::i18n::{tr, trf};
use heatsync_core::tabsets::TabSets;
use heatsync_core::{Channel, Platform};

/// the rooms a tab draws from. a youtube tab is named by video id: its room is
/// the stream's channel id, once the site has said it. until then it has no
/// room (globals and the sender's shipped picks only) and nothing is fetched.
pub(crate) fn keys_of(ch: &Channel, st: &refresh::State) -> Vec<Key> {
    ch.subs().filter_map(|(p, n)| room_key(p, n, st)).collect()
}

fn room_key(p: Platform, n: &str, st: &refresh::State) -> Option<Key> {
    if p == Platform::Youtube {
        return st.yt.id(n).map(|id| refresh::key(p, id));
    }
    Some(refresh::key(p, n))
}

/// every youtube video open in any tab, once.
fn open_videos(app: &App) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for ch in &app.channels {
        for (p, n) in ch.subs() {
            if p == Platform::Youtube && crate::http::plain_video(n) && !out.iter().any(|v| v == n)
            {
                out.push(n.to_string());
            }
        }
    }
    out
}

/// one tab's emote set: its sources' latest sets in the tab's order, then the
/// globals. built whole from what is stored, so it never depends on which
/// fetch landed first.
pub(crate) fn tab_set(
    ch: &Channel,
    st: &refresh::State,
    globals: &heatsync_core::emote::EmoteSet,
) -> TabSets {
    let pooled = refresh::rebuild(&keys_of(ch, st), st, globals);
    // a merged tab also keeps each source room apart (a youtube source with no
    // channel id yet reads the globals alone), so a row reads its own room's emotes
    let rooms = if ch.merged() {
        ch.subs()
            .map(|(p, n)| {
                let keys: Vec<Key> = room_key(p, n, st).into_iter().collect();
                (
                    p,
                    n.to_ascii_lowercase(),
                    refresh::rebuild(&keys, st, globals),
                )
            })
            .collect()
    } else {
        Vec::new()
    };
    TabSets::new(pooled, rooms)
}

/// every room open in any tab, once.
fn open_keys(app: &App) -> Vec<Key> {
    let mut out: Vec<Key> = Vec::new();
    for ch in &app.channels {
        for k in keys_of(ch, &app.refresh) {
            if !out.contains(&k) {
                out.push(k);
            }
        }
    }
    out
}

/// once per tick: land finished fetches, start the due ones. a mock feed never
/// calls out.
pub(crate) fn pump(app: &mut App) {
    if !matches!(app.feed, Feed::Live { .. }) {
        return;
    }
    let now = Instant::now();
    let applied = app.refresh.drain(now);
    let mut rebuilt = false;
    for a in applied {
        if a.changed {
            rebuilt |= swap_in(app, &a.key);
        }
        if a.say_stale {
            app.status = Some(match a.stale.first() {
                Some((p, since)) => trf(
                    "emotes.stale_provider",
                    &[("provider", p), ("since", &since.replace('T', " "))],
                ),
                None => tr("emotes.stale").into(),
            });
        } else if a.manual {
            app.status = Some(
                tr(if a.failed {
                    "emotes.refresh_failed"
                } else if a.changed {
                    "emotes.refreshed"
                } else {
                    "emotes.current"
                })
                .into(),
            );
        }
    }
    if rebuilt {
        prefetch_focused(app);
    }
    pump_inventory(app, now);
    if !app.refresh.look(now) {
        return;
    }
    pump_youtube_ids(app, now);
    let open = open_keys(app);
    for (k, etag) in app.refresh.wanted(&open, now) {
        let held = app.refresh.set(&k).cloned();
        app.refresh.spawn(k, etag, move |k, etag| {
            http::emote_refresh(&k.1, k.0, etag, held.as_ref())
        });
    }
}

/// learn each open youtube tab's channel id (once, quietly), and when one
/// arrives rebuild the tabs that hold that video so the room draws.
fn pump_youtube_ids(app: &mut App, now: Instant) {
    let open = open_videos(app);
    for v in app.refresh.yt.drain(&open, now) {
        for i in 0..app.channels.len() {
            if app.channels[i]
                .subs()
                .any(|(p, n)| p == Platform::Youtube && n == v)
            {
                app.emotes[i] = tab_set(&app.channels[i], &app.refresh, &app.globals);
            }
        }
    }
    for v in app.refresh.yt.wanted(&open, now) {
        app.refresh.yt.spawn(v, http::youtube_channel_id);
    }
}

/// replace the set of every tab that draws from `key` with one rebuilt from its
/// sources' latest sets + the globals.
fn swap_in(app: &mut App, key: &Key) -> bool {
    let mut any = false;
    for i in 0..app.channels.len() {
        let subs = keys_of(&app.channels[i], &app.refresh);
        if subs.contains(key) {
            app.emotes[i] = tab_set(&app.channels[i], &app.refresh, &app.globals);
            any = true;
        }
    }
    any
}

/// your own emotes, read at start and on the emote cadence. a server that
/// does not have the route yet (or a login without the permission) just leaves
/// the tier empty and asks again next time: nothing is said about it.
fn pump_inventory(app: &mut App, now: Instant) {
    while let Ok(got) = app.inv_rx.try_recv() {
        app.inv_busy = false;
        if let Some(list) = got {
            app.inventory = list;
        }
        app.inv_due = app.refresh.period_from(now);
    }
    if app.inv_busy || now < app.inv_due {
        return;
    }
    let Some(c) = app.hs.clone() else { return };
    app.inv_busy = true;
    let tx = app.inv_tx.clone();
    std::thread::spawn(move || {
        let _ = tx.send(c.my_emotes().ok());
    });
}

/// the lines already on screen were scanned against the old set: look again.
fn prefetch_focused(app: &mut App) {
    let App {
        channels,
        emotes,
        store,
        fb,
        focus,
        ..
    } = app;
    if let Some(ch) = channels.get(*focus) {
        let from = ch.messages.len().saturating_sub(60);
        for m in ch.messages.iter().skip(from) {
            request_stacks(store, fb, emotes[*focus].for_row(m), m.refs(), &m.text);
        }
    }
}

/// `/emotes refresh`: re-read the focused tab's rooms now.
pub(crate) fn manual(app: &mut App) {
    let keys = app
        .channels
        .get(app.focus)
        .map(|c| keys_of(c, &app.refresh))
        .unwrap_or_default();
    if !matches!(app.feed, Feed::Live { .. }) || keys.is_empty() {
        app.status = Some(tr("emotes.nothing_to_refresh").into());
        return;
    }
    // a room is registered by the first look at the open tabs
    let open = open_keys(app);
    let now = Instant::now();
    app.refresh.register(&open, now);
    let armed = app.refresh.force(&keys, now);
    app.status = Some(
        tr(if armed == 0 {
            "emotes.refresh_failed"
        } else {
            "emotes.refreshing"
        })
        .into(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use heatsync_core::emote::{Emote, EmoteSet};
    use heatsync_core::Message;

    fn room(url: &str) -> EmoteSet {
        EmoteSet::from_list([Emote {
            name: "Pog".into(),
            url: url.into(),
            provider: "7tv".into(),
            id: url.into(),
            animated: false,
            zero_width: false,
            global: false,
            kind: Default::default(),
        }])
    }

    fn row(platform: Platform, channel: &str) -> Message {
        Message {
            platform,
            user: "u".into(),
            text: "Pog".into(),
            color: None,
            badges: Vec::new(),
            id: None,
            gone: None,
            reply_to: None,
            note: None,
            extra: heatsync_core::Extra {
                channel: channel.into(),
                ..Default::default()
            },
            emotes: None,
            tags: Vec::new(),
            heat: 0.0,
            uid: None,
        }
    }

    fn live_app_with(f: &std::sync::Arc<crate::hsauth::testkit::Fake>) -> App {
        let mut a = crate::special::tests::app();
        a.feed = Feed::Live {
            rx: std::sync::mpsc::channel().1,
            start: Instant::now(),
            connected: true,
        };
        a.hs = Some(crate::hsauth::testkit::client(f));
        let (tx, rx) = std::sync::mpsc::channel();
        a.inv_tx = tx;
        a.inv_rx = rx;
        a
    }

    fn settle(a: &mut App) {
        let t = Instant::now();
        for _ in 0..200 {
            pump_inventory(a, t);
            if !a.inv_busy {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("the inventory read never landed");
    }

    #[test]
    fn a_server_without_the_route_leaves_the_yours_tier_empty_and_silent() {
        use crate::hsauth::testkit::Fake;
        // the router's own 404, byte for byte
        let f = Fake::new(vec![Ok((
            404,
            serde_json::json!({"error": "not found", "path": "/api/cli/emotes"}),
        ))]);
        let mut a = live_app_with(&f);
        a.status = None;
        let before = Instant::now();
        pump_inventory(&mut a, before); // starts the read
        settle(&mut a);
        assert!(a.inventory.is_empty());
        assert!(a.status.is_none(), "no error line: {:?}", a.status);
        // and it asks again on the normal cadence, not now and not never
        assert!(a.inv_due > before + refresh::PERIOD / 2);
        assert!(!a.inv_busy);
    }

    #[test]
    fn your_emotes_load_and_replace_on_the_cadence() {
        use crate::hsauth::testkit::Fake;
        let row = |n: &str| {
            serde_json::json!({"emotes":[{"slot_number":1,"custom_name":n,
                "url":"https://cdn.7tv.app/emote/x/1x.webp","source":"7tv","hash":"h"}]})
        };
        let f = Fake::new(vec![
            Ok((200, row("catJAM"))),
            Ok((200, row("catJAM2"))),
            Ok((500, serde_json::json!({"error": "boom"}))),
        ]);
        let mut a = live_app_with(&f);
        pump_inventory(&mut a, Instant::now());
        settle(&mut a);
        assert_eq!(a.inventory[0].name, "catJAM");
        // not asked again before it is due
        pump_inventory(&mut a, Instant::now());
        assert!(!a.inv_busy);
        a.inv_due = Instant::now();
        pump_inventory(&mut a, Instant::now());
        settle(&mut a);
        assert_eq!(a.inventory[0].name, "catJAM2");
        // a failed read keeps what is held
        a.inv_due = Instant::now();
        pump_inventory(&mut a, Instant::now());
        settle(&mut a);
        assert_eq!(a.inventory[0].name, "catJAM2");
    }

    #[test]
    fn a_merged_tabs_rows_read_their_own_rooms_emotes() {
        let mut st = refresh::State::new();
        let t = Instant::now();
        st.seed(
            refresh::key(Platform::Twitch, "xqc"),
            &room("twitch-pog"),
            t,
        );
        st.seed(refresh::key(Platform::Kick, "xqc"), &room("kick-pog"), t);
        let mut ch = Channel::new("xqc", Platform::Twitch, 10);
        ch.extra = vec![
            (Platform::Kick, "xqc".into()),
            (Platform::Youtube, "vid".into()),
        ];
        let tab = tab_set(&ch, &st, &EmoteSet::new());
        let url = |m: &Message| tab.for_row(m).get("Pog").map(|e| e.url.clone());
        assert_eq!(
            url(&row(Platform::Twitch, "xqc")).as_deref(),
            Some("twitch-pog")
        );
        assert_eq!(
            url(&row(Platform::Kick, "xqc")).as_deref(),
            Some("kick-pog")
        );
        // no channel id yet: globals only (the set here has none)
        assert_eq!(url(&row(Platform::Youtube, "vid")), None);
        // a plain one-source tab has just its set
        let one = Channel::new("xqc", Platform::Kick, 10);
        let tab = tab_set(&one, &st, &EmoteSet::new());
        assert_eq!(
            tab.for_row(&row(Platform::Kick, "xqc"))
                .get("Pog")
                .map(|e| e.url.as_str()),
            Some("kick-pog")
        );
    }

    const UC: &str = "UCaB3_-xyzABC01234567890";

    #[test]
    fn a_youtube_room_has_no_key_until_its_channel_id_is_known() {
        let mut st = refresh::State::new();
        let ch = Channel::new("vid", Platform::Youtube, 10);
        assert!(keys_of(&ch, &st).is_empty(), "no id, no fetch");
        // the tab draws globals meanwhile
        let globals = room("global-pog");
        let tab = tab_set(&ch, &st, &globals);
        assert_eq!(
            tab.for_row(&row(Platform::Youtube, "vid"))
                .get("Pog")
                .map(|e| e.url.as_str()),
            Some("global-pog")
        );
        // the id arrives: the key keeps its case, and the room draws once fetched
        assert!(st.yt.learn("vid", UC));
        let k = keys_of(&ch, &st);
        assert_eq!(k, vec![(Platform::Youtube, UC.to_string())]);
        st.seed(k[0].clone(), &room("yt-pog"), Instant::now());
        let tab = tab_set(&ch, &st, &globals);
        assert_eq!(
            tab.for_row(&row(Platform::Youtube, "vid"))
                .get("Pog")
                .map(|e| e.url.as_str()),
            Some("yt-pog")
        );
    }

    #[test]
    fn a_merged_tabs_youtube_row_reads_its_own_room_once_the_id_lands() {
        let mut st = refresh::State::new();
        let t = Instant::now();
        st.seed(
            refresh::key(Platform::Twitch, "xqc"),
            &room("twitch-pog"),
            t,
        );
        let mut ch = Channel::new("xqc", Platform::Twitch, 10);
        ch.extra = vec![(Platform::Youtube, "vid".into())];
        let globals = room("global-pog");
        let url = |st: &refresh::State, m: &Message| {
            tab_set(&ch, st, &globals)
                .for_row(m)
                .get("Pog")
                .map(|e| e.url.clone())
        };
        let yt = row(Platform::Youtube, "vid");
        assert_eq!(url(&st, &yt).as_deref(), Some("global-pog"));
        st.yt.learn("vid", UC);
        st.seed((Platform::Youtube, UC.to_string()), &room("yt-pog"), t);
        assert_eq!(url(&st, &yt).as_deref(), Some("yt-pog"));
        assert_eq!(
            url(&st, &row(Platform::Twitch, "xqc")).as_deref(),
            Some("twitch-pog")
        );
    }
}
