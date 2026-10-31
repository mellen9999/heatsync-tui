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

/// fetch a channel's recent archive tail on a detached thread — scrollback on
/// open instead of an empty pane waiting for the feed to warm up.
pub(crate) fn spawn_history(
    tx: &Sender<(Platform, String, Vec<Message>)>,
    platform: Platform,
    name: String,
) {
    let tx = tx.clone();
    std::thread::spawn(move || {
        let msgs = http::recent(&name, platform, 60);
        if !msgs.is_empty() {
            let _ = tx.send((platform, name, msgs));
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
    while let Ok((platform, name, msgs)) = hist_rx.try_recv() {
        if let Some(i) = channels.iter().position(|c| c.matches(platform, &name)) {
            channels[i].backfill(msgs);
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
fn deliver(app: &mut App, l: ChatLine, now: u64) {
    let App {
        channels,
        emotes,
        store,
        fb,
        focus,
        me,
        paints,
        ..
    } = app;
    let Some(i) = channels
        .iter()
        .position(|c| c.matches(l.platform, &l.channel))
    else {
        return;
    };
    // fetch starts the instant the line lands — but only for
    // the channel on screen. prefetching every background
    // channel's emotes burned network + decode for images
    // that might never be looked at; a switched-to channel is
    // filled by the tick sweep within 200ms instead.
    // the relay double-broadcasts some event frames (usernotice
    // arrives twice with the same id) — drop a note identical
    // to one already in the recent tail.
    // 16 covers a spam-speed channel filling the gap between
    // the two broadcasts (~200ms apart).
    if l.note.is_some()
        && channels[i].messages.iter().rev().take(16).any(|e| {
            e.platform == l.platform
                && e.user == l.user
                && e.text == l.content
                && e.note.as_ref() == l.note.as_ref()
        })
    {
        return;
    }
    if let Some(u) = &l.uid {
        paints.want(u);
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
    channels[i].record(
        Message {
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
        },
        now,
    );
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
fn route(app: &mut App, l: ChatLine, now: u64) -> Vec<ChatLine> {
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
            | ChatEvent::FeedHidden { .. }) => specials.push(ev),
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
