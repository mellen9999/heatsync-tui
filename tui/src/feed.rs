use crate::app::{App, Feed, PinMsg};
use crate::emote::fb::FbEmotes;
use crate::emote::render::EmoteStore;
use crate::http;
use crate::layout::{each_stack, mentions_any, replies_to_me};
use crate::mentions::Mention;
use crate::net::ChatEvent;
use crate::special;
use crate::talk;
use heatsync_core::emote::EmoteSet;
use heatsync_core::proto::ChatLine;
use heatsync_core::{Extra, Message, Platform};
use std::sync::mpsc::Sender;

/// fetch a channel's emote set on a detached thread and hand it back over `tx`.
/// keeps the blocking HTTP call off the UI thread so joins never freeze.
pub(crate) fn spawn_emote_fetch(
    tx: &Sender<(Platform, String, EmoteSet)>,
    platform: Platform,
    name: String,
) {
    let tx = tx.clone();
    std::thread::spawn(move || {
        let set = http::emote_set(&name, platform).unwrap_or_default();
        let _ = tx.send((platform, name, set));
    });
}

/// fetch the pooled global emote set once at startup, delivered over the same
/// channel as per-channel sets under an empty name (no real sub is ever "").
pub(crate) fn spawn_global_emotes(tx: &Sender<(Platform, String, EmoteSet)>) {
    let tx = tx.clone();
    std::thread::spawn(move || {
        let set = http::global_emotes().unwrap_or_default();
        if !set.is_empty() {
            let _ = tx.send((Platform::Twitch, String::new(), set));
        }
    });
}

/// a channel's fetched scrollback, plus when its live twitch stream began.
pub(crate) struct Hist {
    pub(crate) platform: Platform,
    pub(crate) name: String,
    pub(crate) msgs: Vec<Message>,
    /// (unix seconds, game) — twitch only, and only while the stream is live.
    pub(crate) live: Option<(u64, String)>,
}

/// fetch a channel's recent archive tail on a detached thread — scrollback on
/// open instead of an empty pane waiting for the feed to warm up. a twitch
/// channel's stream start is asked for alongside, so neither waits on the other.
pub(crate) fn spawn_history(tx: &Sender<Hist>, platform: Platform, name: String) {
    let tx = tx.clone();
    std::thread::spawn(move || {
        let ask = (platform == Platform::Twitch).then(|| {
            let n = name.clone();
            std::thread::spawn(move || http::twitch_live(&n))
        });
        let msgs = http::recent(&name, platform, 60);
        let live = ask.and_then(|h| h.join().ok()).flatten();
        if !msgs.is_empty() {
            let _ = tx.send(Hist {
                platform,
                name,
                msgs,
                live,
            });
        }
    });
}

/// fetch a channel's pin on a detached thread; a channel with none sends nothing.
pub(crate) fn spawn_pin(tx: &Sender<PinMsg>, platform: Platform, name: String) {
    let tx = tx.clone();
    std::thread::spawn(move || {
        if let Some(pin) = http::pin(platform, &name) {
            let _ = tx.send((platform, name, Some(pin)));
        }
    });
}

/// land fetched pins in their tabs.
pub(crate) fn drain_pins(app: &mut App) {
    while let Ok((platform, name, pin)) = app.pin_rx.try_recv() {
        set_pin(app, platform, &name, pin);
    }
}

fn set_pin(app: &mut App, platform: Platform, name: &str, pin: Option<heatsync_core::Pin>) {
    for c in app
        .channels
        .iter_mut()
        .filter(|c| c.matches(platform, name))
    {
        c.set_pin(pin.clone());
    }
}

/// merge fetched history behind whatever live lines already landed, and start
/// image loads for the emotes it references.
pub(crate) fn drain_history(app: &mut App) {
    let App {
        channels,
        emotes,
        store,
        fb,
        hist_rx,
        focus,
        ..
    } = app;
    while let Ok(Hist {
        platform,
        name,
        msgs,
        live,
    }) = hist_rx.try_recv()
    {
        if let Some(i) = channels.iter().position(|c| c.matches(platform, &name)) {
            // history reaches back to its oldest timed line; the stream-start
            // row goes in only when that is at or before the start.
            let oldest = msgs
                .iter()
                .map(|m| m.extra.ts)
                .filter(|t| *t > 0)
                .min()
                .unwrap_or(0);
            channels[i].backfill(&name, msgs);
            if let Some((start, game)) = live {
                crate::streamrow::start_row(
                    &mut channels[i],
                    &name,
                    oldest,
                    start,
                    &game,
                    crate::clock::now_secs(),
                );
            }
            // focused channel only — background history warms up via the tick
            // sweep when it's actually looked at.
            if i == *focus {
                let from = channels[i].messages.len().saturating_sub(60);
                for m in channels[i].messages.iter().skip(from) {
                    request_stacks(store, fb, &emotes[i], m.refs(), &m.text);
                }
            }
        }
    }
}

/// merge any emote sets that finished fetching into their channel columns, and
/// prefetch images for the backlog those sets just made resolvable — messages
/// that arrived before the set landed were scanned against an empty set.
pub(crate) fn drain_emotes(app: &mut App) {
    let App {
        channels,
        emotes,
        globals,
        store,
        fb,
        emote_rx,
        focus,
        ..
    } = app;
    let mut dirty = false;
    while let Ok((platform, name, set)) = emote_rx.try_recv() {
        // the empty name is the global set (see spawn_global_emotes): keep it
        // for tabs opened later, and pool it into every open tab now.
        if name.is_empty() {
            *globals = set;
            for es in emotes.iter_mut() {
                es.merge(globals.clone());
            }
            dirty = true;
            continue;
        }
        if let Some(i) = channels.iter().position(|c| c.matches(platform, &name)) {
            // merge — a merged tab pools its sources' sets; channel emotes
            // replace any global of the same name whichever landed first.
            emotes[i].merge(set);
            // rescan only the channel on screen — a background channel's
            // backlog is warmed by the tick sweep if and when it's focused.
            if i == *focus {
                dirty = true;
            }
        }
    }
    // one rescan of the visible backlog no matter how many sets landed.
    if dirty && !channels.is_empty() {
        let i = (*focus).min(channels.len() - 1);
        let from = channels[i].messages.len().saturating_sub(60);
        for m in channels[i].messages.iter().skip(from) {
            request_stacks(store, fb, &emotes[i], m.refs(), &m.text);
        }
    }
}

/// kick off image loads for every emote stack in `text` (idempotent — a key
/// already cached or in flight is a no-op). the prefetch model: each incoming
/// line is scanned ONCE as it arrives, plus a cheap focused-channel sweep on
/// the tick — nothing rescans every channel's backlog anymore.
pub(crate) fn request_stacks(
    store: &mut Option<EmoteStore>,
    fb: &Option<FbEmotes>,
    set: &EmoteSet,
    refs: &EmoteSet,
    text: &str,
) {
    // per-message refs can resolve emotes the channel set doesn't have.
    if set.is_empty() && refs.is_empty() {
        return;
    }
    if let Some(s) = store {
        each_stack(text, set, refs, |k| s.request(k));
    } else if let Some(f) = fb {
        each_stack(text, set, refs, |k| f.request(k));
    }
}

/// land one line in its tab (if it has one): emote prefetch, the repeat-note
/// guard, heat/unread, ping and stream-event flags.
pub(crate) fn deliver(app: &mut App, l: ChatLine, now: u64) {
    if let Some(i) = app
        .channels
        .iter()
        .position(|c| c.matches(l.platform, &l.channel))
    {
        deliver_at(app, i, l, now, true);
    }
}

/// a followed channel's stream row, for every room tab (it has no tab of its
/// own). a guest in someone else's tab: no heat, no unread, no light.
fn fan_out(app: &mut App, l: &ChatLine, now: u64) {
    for i in 0..app.channels.len() {
        if app.channels[i].kind == heatsync_core::TabKind::Channel {
            deliver_at(app, i, l.clone(), now, false);
        }
    }
}

/// land `l` in tab `i`. `own` = the line belongs to this tab (it counts as chat
/// activity); otherwise it is only shown.
fn deliver_at(app: &mut App, i: usize, l: ChatLine, now: u64, own: bool) {
    let App {
        channels,
        emotes,
        store,
        fb,
        focus,
        me,
        paints,
        seen,
        ..
    } = app;
    // one platform message is one row: the server replays a joined room's backlog
    // on every (re)subscribe, and the relay double-broadcasts some event frames
    // (a usernotice arrives twice with the same id). a seen id is dropped here,
    // before it can count as heat, unread, or a ping.
    if !channels[i].first_sight(l.platform, &l.channel, l.id.as_deref()) {
        return;
    }
    // fetch starts the instant the line lands — but only for
    // the channel on screen. prefetching every background
    // channel's emotes burned network + decode for images
    // that might never be looked at; a switched-to channel is
    // filled by the tick sweep within 200ms instead.
    if let Some(u) = &l.uid {
        paints.want(u);
    }
    if l.note.is_none() {
        seen.note(l.platform, &l.user, l.color.as_deref(), l.uid.as_deref());
    }
    let refs = (!l.emotes.is_empty())
        .then(|| std::sync::Arc::new(heatsync_core::emote::ref_set(&l.emotes)));
    if i == *focus {
        let r = refs.as_deref().unwrap_or(EmoteSet::none());
        request_stacks(store, fb, &emotes[i], r, &l.content);
    }
    let pinged = l.note.is_none()
        && !me.iter().any(|n| l.user.eq_ignore_ascii_case(n))
        && (mentions_any(&l.content, me) || replies_to_me(l.reply_to.as_deref(), me));
    let evented = l.stream.is_some();
    let chan = l.channel.clone();
    let msg = Message {
        platform: l.platform,
        user: l.user,
        text: l.content,
        color: l.color,
        badges: l.badges,
        id: l.id,
        gone: None,
        reply_to: l.reply_to,
        note: l.note,
        extra: Extra {
            ts: crate::clock::now_secs(),
            ..l.extra
        },
        emotes: refs,
        tags: Vec::new(),
        heat: 0.0,
        uid: l.uid,
    };
    if !own {
        channels[i].system(msg);
        return;
    }
    channels[i].record(msg, now);
    let mention = pinged
        .then(|| channels[i].messages.back())
        .flatten()
        .map(|m| Mention {
            platform: m.platform,
            channel: chan,
            user: m.user.clone(),
            color: m.color.clone(),
            text: m.text.clone(),
            id: m.id.clone(),
            at: crate::clock::now_secs() as i64,
        });
    if pinged {
        channels[i].ping();
    }
    if evented {
        channels[i].flag_event();
    }
    if let Some(m) = mention {
        special::mention(app, m);
    }
}

/// a live line through the stream-event guards. a followed-channel twin of an
/// open tab is dropped (the room's own event lands there); one for a channel
/// with no tab becomes a status flash. returns what to deliver now.
pub(crate) fn route(app: &mut App, l: ChatLine, now: u64) -> Vec<ChatLine> {
    let follow = l.stream.as_ref().is_some_and(|s| s.follow);
    if follow {
        // live list changed — the picker's cached rows are now stale.
        app.pick_cache.stale();
        if app
            .channels
            .iter()
            .any(|c| c.matches(l.platform, &l.channel))
        {
            return Vec::new();
        }
    }
    let out = app.streams.push(l, now);
    if follow {
        for l in &out {
            if let Some(n) = &l.note {
                app.status = Some(format!("{} {}", l.channel, n.what));
            }
            special::follow_event(app, l);
            fan_out(app, l, now);
        }
        return Vec::new();
    }
    out
}

/// drain the live feed into the channel buffers. runs on every event-loop wake,
/// so a chat line shows on the very next frame instead of the next data tick.
pub(crate) fn drain_feed(app: &mut App) -> bool {
    let Feed::Live { rx, start, .. } = &app.feed else {
        return false; // mock advances on the tick — it's a synthetic cadence
    };
    let now = start.elapsed().as_millis() as u64;
    let mut got = false;
    let mut evs = Vec::new();
    // virtual-tab events are queued and run once the channel borrows are done.
    let mut specials: Vec<ChatEvent> = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        evs.push(ev);
    }
    for ev in evs {
        got = true;
        match ev {
            ChatEvent::Line(l) => {
                for l in route(app, l, now) {
                    deliver(app, l, now);
                }
            }
            ChatEvent::FollowLive(live) => app.streams.seed_live(live),
            ChatEvent::FollowHistory(h) => special::follow_history(app, h),
            ChatEvent::FollowColors(c) => special::follow_colors(app, c),
            ChatEvent::Pin(p, c, pin) => set_pin(app, p, &c, pin),
            ChatEvent::Room(c, r) => {
                for l in app.rooms.update(&c, r) {
                    deliver(app, l, now);
                }
            }
            ChatEvent::Delete(d) => {
                for c in app.channels.iter_mut() {
                    // youtube's frame names a video; an empty channel = any tab
                    // of that platform (the id still has to match a line).
                    let here = if d.channel.is_empty() {
                        c.subs().any(|(p, _)| p == d.platform)
                    } else {
                        c.matches(d.platform, &d.channel)
                    };
                    if here {
                        c.apply(&d);
                    }
                }
            }
            ev @ (ChatEvent::Special(_)
            | ChatEvent::FeedHeat { .. }
            | ChatEvent::FeedHidden { .. }
            | ChatEvent::FeedEdit { .. }) => specials.push(ev),
            ChatEvent::Account => special::account_changed(app),
            ChatEvent::Follows => talk::refresh_follows(app),
            ChatEvent::Color { id, names, color } => {
                app.paints.set_color(id.as_deref(), &names, &color)
            }
            ChatEvent::Cosmetic(ids) => app.paints.invalidate(&ids),
            ChatEvent::Connected | ChatEvent::Disconnected => {
                if let Feed::Live { connected, .. } = &mut app.feed {
                    *connected = matches!(ev, ChatEvent::Connected);
                }
            }
            ChatEvent::Auth(ok) => special::auth(app, ok),
            ChatEvent::SendResult { ok, error } => {
                if !ok {
                    app.status = Some(format!("send failed: {}", error.unwrap_or_default()));
                }
            }
        }
    }
    // first-game updates whose wait for a go-live ran out.
    for l in app.streams.tick(now) {
        got = true;
        deliver(app, l, now);
    }
    for ev in specials {
        special::event(app, ev);
    }
    special::drain_backlog(app);
    talk::drain(app);
    got
}

/// twitch sender feedback → a system line in the tab it belongs to (so a
/// rejection stays in scrollback) and the status line (so it shows while
/// composing). connection-wide notes have no tab: status only.
pub(crate) fn drain_twitch(app: &mut App) {
    let Some(rx) = &app.twitch_notes else { return };
    while let Ok(n) = rx.try_recv() {
        if let Some(name) = &n.channel {
            if let Some(ch) = app
                .channels
                .iter_mut()
                .find(|c| c.matches(Platform::Twitch, name))
            {
                ch.system(Message {
                    platform: Platform::Twitch,
                    user: String::new(),
                    text: String::new(),
                    color: None,
                    badges: Vec::new(),
                    id: None,
                    gone: None,
                    reply_to: None,
                    note: Some(heatsync_core::Note {
                        kind: heatsync_core::NoteKind::Notice,
                        what: n.text.clone(),
                    }),
                    extra: Default::default(),
                    emotes: None,
                    tags: Vec::new(),
                    heat: 0.0,
                    uid: None,
                });
            }
        }
        app.status = Some(n.text);
    }
}

/// finished heatsync sends / mod actions → a line in the tab they belong to (so
/// a confirmation or a refusal stays in scrollback) and the status line.
/// a clean send has no words: nothing to say.
pub(crate) fn drain_hs(app: &mut App) {
    while let Ok(names) = app.names_rx.try_recv() {
        for n in names {
            if !app.me.iter().any(|m| m.eq_ignore_ascii_case(&n)) {
                app.me.push(n);
            }
        }
    }
    while let Ok(n) = app.hs_notes.try_recv() {
        if n.logged_out {
            app.hs = None; // the session file is already gone; stop using the dead login
        }
        // a send that landed from a temp tab keeps it (the ext's auto-keep).
        if n.sent {
            crate::temp::sent(app, n.platform, &n.channel);
        }
        if n.text.is_empty() {
            continue;
        }
        if let Some(ch) = app
            .channels
            .iter_mut()
            .find(|c| c.matches(n.platform, &n.channel))
        {
            ch.system(Message {
                platform: n.platform,
                user: String::new(),
                text: String::new(),
                color: None,
                badges: Vec::new(),
                id: None,
                gone: None,
                reply_to: None,
                note: Some(heatsync_core::Note {
                    kind: heatsync_core::NoteKind::Notice,
                    what: n.text.clone(),
                }),
                extra: Default::default(),
                emotes: None,
                tags: Vec::new(),
                heat: 0.0,
                uid: None,
            });
        }
        app.status = Some(n.text);
    }
}

/// merge finished emote loads into the cache (every wake — a built emote is on
/// screen the next frame, never parked until the tick).
pub(crate) fn pump_loads(app: &mut App) {
    app.paints.pump();
    if let Some(store) = &mut app.store {
        store.pump();
    } else if let Some(fb) = &mut app.fb {
        fb.pump();
    }
}

/// the slow-cadence work: heat decay, the mock driver, and a prefetch sweep of
/// the focused channel only (covers focus switches and cache-evicted emotes;
/// arrival-time scans in drain_feed cover everything else).
pub(crate) fn tick_advance(app: &mut App) {
    let App {
        channels,
        emotes,
        feed,
        store,
        fb,
        focus,
        ..
    } = app;
    match feed {
        Feed::Mock(driver) => {
            driver.tick(channels);
        }
        Feed::Live { start, .. } => {
            let now = start.elapsed().as_millis() as u64;
            for ch in channels.iter_mut() {
                ch.cool(now);
            }
        }
    }
    if let Some(ch) = channels.get(*focus) {
        let from = ch.messages.len().saturating_sub(60);
        for m in ch.messages.iter().skip(from) {
            request_stacks(store, fb, &emotes[*focus], m.refs(), &m.text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::{forward, ChatEvent};
    use heatsync_core::{proto, Unread};
    use std::sync::mpsc::{channel, Sender};
    use std::time::Instant;

    /// a live-mode app (xqc twitch, kripp kick, vid1 youtube): frames pushed into
    /// the sender reach the tabs through the real drain, as net.rs hands them over.
    fn live() -> (App, Sender<ChatEvent>) {
        let mut a = crate::special::tests::app();
        let (tx, rx) = channel();
        a.feed = Feed::Live {
            rx,
            start: Instant::now(),
            connected: true,
        };
        a.channels
            .push(heatsync_core::Channel::new("vid1", Platform::Youtube, 50));
        a.emotes.push(EmoteSet::new());
        (a, tx)
    }

    /// one raw ws frame in, drained into the tabs.
    fn frame(a: &mut App, tx: &Sender<ChatEvent>, raw: &str) {
        assert!(forward(proto::parse(raw), tx));
        drain_feed(a);
    }

    fn texts(a: &App, tab: usize) -> Vec<&str> {
        a.channels[tab]
            .messages
            .iter()
            .map(|m| m.text.as_str())
            .collect()
    }

    const KICK_TAB: usize = 1;
    const YT_TAB: usize = 2;

    fn kick_live(id: &str, user: &str, text: &str) -> String {
        format!(
            r#"{{"type":"kick-chat-message","data":{{"channel":"kripp","id":"{id}","username":"{user}","content":"{text}"}}}}"#
        )
    }

    const KICK_BACKFILL: &str = r#"{"type":"kick-chat-backfill","channel":"kripp","replay":true,"messages":[
        {"channel":"kripp","id":"k1","username":"a","content":"one"},
        {"channel":"kripp","id":"k2","username":"b","content":"two"},
        {"channel":"kripp","id":"k3","username":"a","content":"one"}]}"#;

    #[test]
    fn a_resubscribes_replayed_kick_backlog_is_not_drawn_twice() {
        let (mut a, tx) = live();
        frame(&mut a, &tx, &kick_live("k1", "a", "one"));
        // the server resends the whole backlog on every join (reconnect, a second tab)
        frame(&mut a, &tx, KICK_BACKFILL);
        assert_eq!(
            texts(&a, KICK_TAB),
            ["one", "two", "one"],
            "k3 is a new line with old words"
        );
        let before = (a.channels[KICK_TAB].seq, a.channels[KICK_TAB].heat);
        for _ in 0..3 {
            frame(&mut a, &tx, KICK_BACKFILL);
        }
        assert_eq!(texts(&a, KICK_TAB).len(), 3);
        assert_eq!(
            (a.channels[KICK_TAB].seq, a.channels[KICK_TAB].heat),
            before,
            "a replay is not news: no heat, no new line"
        );
        // and a live copy of something the replay already drew
        frame(&mut a, &tx, &kick_live("k2", "b", "two"));
        assert_eq!(texts(&a, KICK_TAB).len(), 3);
    }

    #[test]
    fn a_youtube_backlog_replayed_on_every_subscribe_is_drawn_once() {
        let (mut a, tx) = live();
        let batch = r#"{"type":"youtube:chat","videoId":"vid1","replay":true,"messages":[
            {"id":"y1","user":"u","text":"hi"},{"id":"y2","user":"v","text":"yo"}]}"#;
        frame(&mut a, &tx, batch);
        frame(&mut a, &tx, batch); // resubscribe after a reconnect
        let live = r#"{"type":"youtube:chat","videoId":"vid1","messages":[
            {"id":"y2","user":"v","text":"yo"},{"id":"y3","user":"v","text":"yo"}]}"#;
        frame(&mut a, &tx, live);
        assert_eq!(
            texts(&a, YT_TAB),
            ["hi", "yo", "yo"],
            "y3 is a new line with old words"
        );
    }

    #[test]
    fn two_people_saying_the_same_words_are_two_lines() {
        let (mut a, tx) = live();
        frame(&mut a, &tx, &kick_live("m1", "amy", "LUL"));
        frame(&mut a, &tx, &kick_live("m2", "bob", "LUL"));
        frame(&mut a, &tx, &kick_live("m3", "amy", "LUL"));
        assert_eq!(texts(&a, KICK_TAB), ["LUL", "LUL", "LUL"]);
    }

    #[test]
    fn a_replayed_ping_does_not_light_a_tab_that_was_already_read() {
        let (mut a, tx) = live();
        a.me = vec!["me".into()];
        let ping = r#"{"type":"irc:message","channel":"xqc","message":{"type":"privmsg","id":"p1","username":"amy","content":"hey me"}}"#;
        frame(&mut a, &tx, ping);
        assert_eq!(a.channels[0].unread(), Unread::Ping);
        a.channels[0].mark_seen();
        frame(&mut a, &tx, ping);
        assert_eq!(a.channels[0].unread(), Unread::None);
        assert_eq!(texts(&a, 0).len(), 1);
    }

    #[test]
    fn the_relays_second_usernotice_is_dropped_by_id_but_a_repeat_of_the_words_is_not() {
        let (mut a, tx) = live();
        let gift = |id: &str| {
            format!(
                r##"{{"type":"irc:message","channel":"xqc","message":{{"type":"usernotice","id":"{id}",
                "subType":"submysterygift","username":"g","displayName":"G","systemMessage":"G is gifting 5 Tier 1 Subs."}}}}"##
            )
        };
        frame(&mut a, &tx, &gift("n1"));
        frame(&mut a, &tx, &gift("n1")); // irc + eventsub both broadcast it
        assert_eq!(a.channels[0].messages.len(), 1);
        frame(&mut a, &tx, &gift("n2")); // a second, real gift, same words
        assert_eq!(a.channels[0].messages.len(), 2);
    }

    #[test]
    fn a_channel_open_in_two_tabs_draws_each_line_in_one_of_them_not_both() {
        let (mut a, tx) = live();
        let mut second = heatsync_core::Channel::new("vid1", Platform::Youtube, 50);
        second.extra = vec![(Platform::Kick, "kripp".into())];
        a.channels.push(second);
        a.emotes.push(EmoteSet::new());
        frame(&mut a, &tx, &kick_live("t1", "a", "once"));
        frame(&mut a, &tx, KICK_BACKFILL);
        let drawn: usize = a
            .channels
            .iter()
            .map(|c| {
                c.messages
                    .iter()
                    .filter(|m| m.id.as_deref() == Some("t1"))
                    .count()
            })
            .sum();
        assert_eq!(
            drawn, 1,
            "routing hands a line to the first tab that has the source"
        );
    }

    fn archived(id: &str, text: &str) -> Message {
        Message {
            platform: Platform::Kick,
            user: "u".into(),
            text: text.into(),
            color: None,
            badges: Vec::new(),
            id: Some(id.into()),
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
    fn history_and_live_in_either_order_draw_a_message_once() {
        let (mut a, tx) = live();
        // live first, then the history answer that overlaps it
        frame(&mut a, &tx, &kick_live("h2", "u", "two"));
        a.hist_tx
            .send(Hist {
                platform: Platform::Kick,
                name: "kripp".into(),
                msgs: vec![archived("h1", "one"), archived("h2", "two")],
                live: None,
            })
            .unwrap();
        drain_history(&mut a);
        assert_eq!(texts(&a, KICK_TAB), ["one", "two"]);
        // history first, then a late live copy of one of its lines
        frame(&mut a, &tx, &kick_live("h1", "u", "one"));
        assert_eq!(texts(&a, KICK_TAB), ["one", "two"]);
    }
}
