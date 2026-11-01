//! the virtual tabs: mentions, feed, whispers. they live in `app.channels`
//! like any tab (so focus, unread colouring and the tab bar need no second
//! code path) but join nothing — core's `Channel::special` has no sources, so
//! no line, part, emote fetch or history call ever reaches them. they pin left
//! of the channel tabs in [`TabKind`] order, and which are open persists.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use heatsync_core::emote::{ref_set, EmoteSet};
use heatsync_core::proto::{ChatLine, SpecialLine};
use heatsync_core::{Channel, Message, Note, Platform, TabKind, Tag, TagKind};

use crate::app::{App, Feed};
use crate::followlog::{FollowLog, FollowRow, Item};
use crate::mentions::{self, Mention};
use crate::net::{ChatEvent, Outbound};
use crate::save_state;
use crate::social::Msg;
use crate::talk;
use heatsync_core::social::{Contact, Row};

/// rows a virtual tab keeps (the ring cap, like a channel's).
const CAP: usize = 500;
/// feed rows fetched when the tab opens.
const FEED_BACKLOG: usize = 30;

pub(crate) struct State {
    pub(crate) log: mentions::Log,
    /// followed-channel stream events shown among the mentions (never persisted).
    follow: FollowLog,
    /// recent mentions + arrival time: a live ping and the server's rule match
    /// for one message must read as one row.
    recent: Vec<(Instant, Mention)>,
    feed_tx: Sender<Vec<SpecialLine>>,
    feed_rx: Receiver<Vec<SpecialLine>>,
    /// the relay socket is signed in with the heatsync login (not HEATSYNC_TOKEN).
    pub(crate) ws_cli: bool,
    /// the "log in again" hint has been shown — it speaks once per run.
    pub(crate) hinted: bool,
    /// finished account calls (see `talk`) come back here.
    pub(crate) social_tx: Sender<Msg>,
    pub(crate) social_rx: Receiver<Msg>,
    /// people `/w` and `/dm` can address: your threads, follows, live senders.
    pub(crate) contacts: Vec<Contact>,
}

impl State {
    pub(crate) fn new(ws_cli: bool) -> State {
        State {
            ws_cli,
            ..State::with_log(mentions::Log::load())
        }
    }

    fn with_log(log: mentions::Log) -> State {
        let (feed_tx, feed_rx) = channel();
        let (social_tx, social_rx) = channel();
        State {
            log,
            follow: FollowLog::default(),
            recent: Vec::new(),
            feed_tx,
            feed_rx,
            ws_cli: false,
            hinted: false,
            social_tx,
            social_rx,
            contacts: Vec::new(),
        }
    }
}

/// index of the open tab of `kind`.
pub(crate) fn find(app: &App, kind: TabKind) -> Option<usize> {
    app.channels.iter().position(|c| c.kind == kind)
}

/// the tab's line, as the mention log remembers it.
fn mention_msg(m: &Mention) -> Message {
    let mut tags = vec![Tag::platform()];
    if !m.channel.is_empty() {
        tags.push(Tag::new(&format!("[{}]", m.channel), TagKind::Origin));
    }
    row(
        m.platform, &m.user, &m.color, &m.text, tags, 0.0, None, None,
    )
}

/// a followed-channel event as a dim `[chan]` row (the channel's colour when known).
fn follow_msg(f: &FollowLog, r: &FollowRow) -> Message {
    let mut m = row(
        r.platform,
        "",
        &f.color_of(r),
        &r.title,
        vec![
            Tag::platform(),
            Tag::new(&format!("[{}]", r.channel), TagKind::Origin),
        ],
        0.0,
        None,
        None,
    );
    m.note = Some(Note {
        kind: r.kind,
        what: r.what.clone(),
    });
    m.extra.ts = r.at.max(0) as u64;
    m
}

/// the mentions tab's rows: the log and the follow events in time order.
fn mention_rows(s: &State) -> Vec<Message> {
    s.follow
        .timeline(s.log.rows())
        .into_iter()
        .map(|i| match i {
            Item::Mention(m) => mention_msg(m),
            Item::Follow(r) => follow_msg(&s.follow, r),
        })
        .collect()
}

/// refill the open mentions tab from scratch — history, not news.
fn rebuild(app: &mut App) {
    if let Some(i) = find(app, TabKind::Mentions) {
        let mut ch = Channel::special(TabKind::Mentions, CAP);
        for m in mention_rows(&app.special) {
            ch.deliver(m);
        }
        ch.mark_seen();
        app.channels[i] = ch;
    }
}

/// `follow:history`: the server's replay. folded in by time, deduped against
/// what is already here, never a ping.
pub(crate) fn follow_history(app: &mut App, events: Vec<(ChatLine, u64)>) {
    let mut fresh = false;
    for (l, ms) in events {
        if let Some(r) = FollowRow::of(&l, (ms / 1000) as i64) {
            fresh |= app.special.follow.add(r);
        }
    }
    if fresh {
        rebuild(app);
    }
}

/// `follow:colors`: repaint the rows when a channel's colour is new.
pub(crate) fn follow_colors(app: &mut App, colors: Vec<(String, String)>) {
    if app.special.follow.set_colors(colors) {
        rebuild(app);
    }
}

/// a live followed-channel event that passed the stream guards: remembered, and
/// when the tab is open it lands at the bottom; a go-live marks the tab yellow.
pub(crate) fn follow_event(app: &mut App, l: &ChatLine) {
    let Some(r) = FollowRow::of(l, crate::clock::now_secs() as i64) else {
        return;
    };
    let live = r.kind == heatsync_core::NoteKind::Live;
    if !app.special.follow.add(r.clone()) {
        return;
    }
    if let Some(i) = find(app, TabKind::Mentions) {
        let m = follow_msg(&app.special.follow, &r);
        let ch = &mut app.channels[i];
        ch.system(m);
        if app.focus != i {
            if live {
                ch.flag_event();
            }
        } else {
            ch.mark_seen();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn row(
    platform: Platform,
    user: &str,
    color: &Option<String>,
    text: &str,
    tags: Vec<Tag>,
    heat: f64,
    id: Option<String>,
    emotes: Option<Arc<EmoteSet>>,
) -> Message {
    Message {
        platform,
        user: user.to_string(),
        text: text.to_string(),
        color: color.clone(),
        badges: Vec::new(),
        reply_to: None,
        id,
        gone: None,
        note: None,
        emotes,
        tags,
        extra: Default::default(),
        uid: None,
        heat,
    }
}

fn special_msg(s: SpecialLine) -> Message {
    let l = s.line;
    let refs = (!l.emotes.is_empty()).then(|| Arc::new(ref_set(&l.emotes)));
    row(
        l.platform, &l.user, &l.color, &l.content, s.tags, s.heat, l.id, refs,
    )
}

/// make sure the tab exists (pinned in order) and return its index. fills it
/// from what we already know — the mention log — and starts the feed.
pub(crate) fn add(app: &mut App, kind: TabKind) -> usize {
    if let Some(i) = find(app, kind) {
        return i;
    }
    let at = app
        .channels
        .iter()
        .take_while(|c| c.kind.is_special() && c.kind < kind)
        .count();
    let mut ch = Channel::special(kind, CAP);
    if kind == TabKind::Mentions {
        for m in mention_rows(&app.special) {
            ch.deliver(m);
        }
        ch.mark_seen(); // history isn't news
    }
    app.channels.insert(at, ch);
    app.emotes.insert(at, app.globals.clone());
    if app.focus >= at && app.channels.len() > 1 {
        app.focus += 1; // keep the same tab focused
    }
    if app.manage_cursor >= at {
        app.manage_cursor += 1;
    }
    match kind {
        TabKind::Feed => start_feed(app),
        TabKind::Whispers => talk::load_history(app),
        TabKind::Mentions => talk::load_hits(app),
        TabKind::Channel => {}
    }
    at
}

/// open (or focus) a virtual tab.
pub(crate) fn open(app: &mut App, kind: TabKind) {
    let i = add(app, kind);
    app.focus = i;
    save_state(app);
}

/// a virtual tab is being closed: stop whatever feeds it.
pub(crate) fn on_close(app: &App, idx: usize) {
    if app.channels.get(idx).map(|c| c.kind) == Some(TabKind::Feed) {
        if let Some(out) = &app.out {
            let _ = out.send(Outbound::Feed(false));
        }
    }
}

/// join the site feed and load its latest rows. mock mode has no network.
fn start_feed(app: &App) {
    if !matches!(app.feed, Feed::Live { .. }) {
        return;
    }
    if let Some(out) = &app.out {
        let _ = out.send(Outbound::Feed(true));
    }
    let tx = app.special.feed_tx.clone();
    std::thread::spawn(move || {
        let rows = crate::http::feed_recent(FEED_BACKLOG);
        if !rows.is_empty() {
            let _ = tx.send(rows);
        }
    });
}

/// startup: the tabs the config says are open, plus their feed / history.
pub(crate) fn restore(app: &mut App, kinds: &[TabKind]) {
    for &k in kinds {
        add(app, k);
    }
}

/// the initial feed load landed: chronological rows go in behind whatever
/// live rows already arrived.
pub(crate) fn drain_backlog(app: &mut App) {
    while let Ok(rows) = app.special.feed_rx.try_recv() {
        let Some(i) = find(app, TabKind::Feed) else {
            continue;
        };
        let ch = &mut app.channels[i];
        let mut fresh: Vec<Message> = rows
            .into_iter()
            .map(special_msg)
            .filter(|m| !ch.messages.iter().any(|e| e.id.is_some() && e.id == m.id))
            .collect();
        // oldest first, then in front of the live tail (history is not unread)
        let live: Vec<Message> = ch.messages.drain(..).collect();
        for m in fresh.drain(..).chain(live) {
            ch.system(m);
        }
    }
}

/// how long two reports of one message count as the same mention.
const DUP_WINDOW: Duration = Duration::from_secs(10);

/// same message? an id match when both sides have one, else same channel +
/// user with one text the other's prefix / substring (snippets are cut short).
fn same_mention(a: &Mention, b: &Mention) -> bool {
    if let (Some(x), Some(y)) = (&a.id, &b.id) {
        return x == y;
    }
    let (t, u) = (a.text.trim(), b.text.trim());
    a.channel.eq_ignore_ascii_case(&b.channel)
        && a.user.eq_ignore_ascii_case(&b.user)
        && !t.is_empty()
        && !u.is_empty()
        && (t.contains(u) || u.contains(t))
}

/// a mention landed (a live ping, or a server rule match): always remembered,
/// and shown + flagged when the tab is open. a repeat report is dropped.
pub(crate) fn mention(app: &mut App, m: Mention) {
    let now = Instant::now();
    app.special
        .recent
        .retain(|(t, _)| now.duration_since(*t) < DUP_WINDOW);
    if app.special.recent.iter().any(|(_, r)| same_mention(r, &m)) {
        return;
    }
    app.special.recent.push((now, m.clone()));
    if let Some(i) = find(app, TabKind::Mentions) {
        let ch = &mut app.channels[i];
        ch.deliver(mention_msg(&m));
        if app.focus != i {
            ch.ping();
        } else {
            ch.mark_seen();
        }
    }
    app.special.log.push(m);
}

/// the socket's answer to `authenticate`. a login without the `account`
/// permission is refused and stays anonymous: say so once, in plain words.
pub(crate) fn auth(app: &mut App, ok: bool) {
    if ok {
        app.status = Some("authenticated".into());
    } else if !app.special.ws_cli {
        app.status = Some("auth failed — check HEATSYNC_TOKEN".into());
    } else if !app.special.hinted {
        app.special.hinted = true;
        app.status =
            Some("run `heatsync-tui login` again to get whispers, dms and mentions".into());
    }
}

/// route the events virtual tabs care about.
pub(crate) fn event(app: &mut App, ev: ChatEvent) {
    match ev {
        ChatEvent::Special(s) => match s.tab {
            TabKind::Mentions => mention(
                app,
                Mention {
                    platform: s.line.platform,
                    channel: s.line.channel,
                    user: s.line.user,
                    color: s.line.color,
                    text: s.line.content,
                    id: s.line.id,
                    at: crate::clock::now_secs() as i64,
                },
            ),
            TabKind::Feed | TabKind::Whispers => {
                let tab = s.tab;
                let Some(i) = find(app, tab) else { return };
                if tab == TabKind::Feed
                    && s.line.id.is_some()
                    && app.channels[i].messages.iter().any(|m| m.id == s.line.id)
                {
                    return; // already in the backlog
                }
                if let Some(p) = s.peer.clone() {
                    talk::learn(app, p);
                }
                let msg = special_msg(s);
                let ch = &mut app.channels[i];
                ch.deliver(msg);
                if tab == TabKind::Whispers && app.focus != i {
                    ch.ping();
                }
                if app.focus == i {
                    ch.mark_seen();
                }
            }
            TabKind::Channel => {}
        },
        ChatEvent::FeedHeat { id, heat } => {
            if let Some(i) = find(app, TabKind::Feed) {
                if let Some(m) = app.channels[i]
                    .messages
                    .iter_mut()
                    .find(|m| m.id.as_deref() == Some(id.as_str()))
                {
                    m.heat = heat;
                }
            }
        }
        ChatEvent::FeedHidden { id } => {
            if let Some(i) = find(app, TabKind::Feed) {
                for m in app.channels[i]
                    .messages
                    .iter_mut()
                    .filter(|m| m.id.as_deref() == Some(id.as_str()))
                {
                    m.gone = Some("hidden".into());
                }
            }
        }
        _ => {}
    }
}

/// Enter on the mentions tab: jump to the tab the newest mention came from.
/// (no message cursor yet, so "newest" is the whole selection.)
pub(crate) fn enter(app: &mut App) {
    let Some(ch) = app.channels.get(app.focus) else {
        return;
    };
    if ch.kind != TabKind::Mentions {
        return;
    }
    let Some(m) = ch.messages.back() else {
        app.status = Some("no mentions yet".into());
        return;
    };
    let chan = m
        .tags
        .iter()
        .find(|t| t.kind == TagKind::Origin)
        .map(|t| t.text.trim_matches(|c| c == '[' || c == ']').to_string())
        .unwrap_or_default();
    match app
        .channels
        .iter()
        .position(|c| c.matches(m.platform, &chan))
    {
        Some(i) => app.focus = i,
        None => app.status = Some(format!("not open: {chan}")),
    }
}

/// text composed on a virtual tab. the feed takes posts (a reply when `r`
/// picked a row); whispers want `/w` or `/dm` (they need a name); mentions
/// are read-only. true = the line went and the composer clears.
pub(crate) fn send(app: &mut App, kind: TabKind, text: &str, reply: Option<String>) -> bool {
    match kind {
        TabKind::Feed => return talk::post(app, text, reply),
        TabKind::Whispers => {
            app.status =
                Some("say who to: /w <user> <msg> or /dm <user> <msg> — or r on a row".into())
        }
        _ => {
            app.status = Some(format!(
                "{} is read-only — open a channel to chat",
                kind.name()
            ))
        }
    }
    false
}

/// the whispers / dms history landed: oldest first, in front of whatever live
/// rows already arrived, skipping any the live feed already showed.
pub(crate) fn backfill_whispers(app: &mut App, rows: Vec<Row>) {
    let Some(i) = find(app, TabKind::Whispers) else {
        return;
    };
    let ch = &mut app.channels[i];
    let same = |a: &Message, b: &Message| {
        a.user == b.user && a.text == b.text && a.tags.first() == b.tags.first()
    };
    let fresh: Vec<Message> = rows
        .into_iter()
        .map(|r| special_msg(r.line))
        .filter(|m| !ch.messages.iter().any(|e| same(e, m)))
        .collect();
    let live: Vec<Message> = ch.messages.drain(..).collect();
    for m in fresh.into_iter().chain(live) {
        ch.system(m);
    }
}

/// the server's rule hits landed: any the local log doesn't already hold are
/// merged in by time (history, not news — nothing flashes) and the tab is
/// rebuilt from the log so they sit where they happened.
pub(crate) fn backfill_mentions(app: &mut App, hits: Vec<Mention>) {
    let mut fresh: Vec<Mention> = Vec::new();
    for m in hits {
        let held = app
            .special
            .log
            .rows()
            .chain(fresh.iter())
            .any(|r| same_mention(r, &m));
        if !held {
            fresh.push(m);
        }
    }
    if fresh.is_empty() {
        return;
    }
    app.special.log.merge(fresh);
    rebuild(app);
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::app::InputMode;
    use crate::{hsauth, picker};
    use heatsync_core::proto::{self, Event};
    use heatsync_core::room::RoomModes;
    use heatsync_core::streamev::StreamEvents;
    use heatsync_core::{edit, mock};

    /// an app with two real tabs, a mock feed and an in-memory mention log —
    /// nothing touches the network or ~/.config.
    pub(crate) fn app() -> App {
        let (hs_tx, hs_notes) = channel::<hsauth::Note>();
        let (emote_tx, emote_rx) = channel();
        let (hist_tx, hist_rx) = channel();
        let (pick_tx, pick_rx) = channel();
        App {
            channels: vec![
                Channel::new("xqc", Platform::Twitch, 50),
                Channel::new("kripp", Platform::Kick, 50),
            ],
            emotes: vec![EmoteSet::new(), EmoteSet::new()],
            globals: EmoteSet::new(),
            focus: 0,
            paused: false,
            feed: Feed::Mock(mock::Driver::new()),
            store: None,
            fb: None,
            mode: InputMode::Normal,
            input: String::new(),
            line: edit::Line::default(),
            status: None,
            out: None,
            twitch_tx: None,
            twitch_notes: None,
            kick_tx: None,
            hs: None,
            hs_tx,
            hs_notes,
            tab_pos: crate::config::TabPos::Top,
            manage_cursor: 0,
            completion: None,
            me: Vec::new(),
            names_rx: channel().1,
            stamps: false,
            streams: StreamEvents::new(),
            rooms: RoomModes::new(),
            paints: crate::paints::Paints::new(),
            pin_tx: channel().0,
            pin_rx: channel().1,
            cursor: 0,
            reply: None,
            view_h: std::cell::Cell::new(0),
            chat_body: Default::default(),
            emote_tx,
            emote_rx,
            hist_tx,
            hist_rx,
            picker: None,
            pick_cache: picker::Cache(None),
            pick_tx,
            pick_rx,
            pick_inflight: false,
            special: State::with_log(mentions::Log::open(None)),
        }
    }

    fn kinds(app: &App) -> Vec<TabKind> {
        app.channels.iter().map(|c| c.kind).collect()
    }

    fn ev(raw: &str) -> ChatEvent {
        match proto::parse(raw) {
            Event::Special(s) => ChatEvent::Special(s),
            Event::FeedHeat { id, heat } => ChatEvent::FeedHeat { id, heat },
            Event::FeedHidden { id } => ChatEvent::FeedHidden { id },
            e => panic!("{e:?}"),
        }
    }

    fn mention_of(chan: &str, user: &str) -> Mention {
        Mention {
            platform: Platform::Twitch,
            channel: chan.into(),
            user: user.into(),
            color: None,
            text: "hey me".into(),
            id: None,
            at: 0,
        }
    }

    #[test]
    fn mention_rows_carry_platform_and_origin_tags() {
        let m = mention_msg(&Mention {
            platform: Platform::Kick,
            channel: "xqc".into(),
            user: "bob".into(),
            color: None,
            text: "hey".into(),
            id: None,
            at: 0,
        });
        assert_eq!(m.tags[0].kind, TagKind::Platform);
        assert_eq!(m.tags[1], Tag::new("[xqc]", TagKind::Origin));
        assert_eq!(m.platform, Platform::Kick);
    }

    #[test]
    fn tabs_pin_left_in_kind_order_and_focus_follows_the_same_tab() {
        let mut a = app();
        a.focus = 1; // kripp
        open(&mut a, TabKind::Whispers);
        assert_eq!(a.focus, 0, "opening focuses the new tab");
        a.focus = 1; // xqc, right after whispers
        add(&mut a, TabKind::Mentions);
        assert_eq!(
            kinds(&a),
            vec![
                TabKind::Mentions,
                TabKind::Whispers,
                TabKind::Channel,
                TabKind::Channel
            ]
        );
        assert_eq!(a.channels[a.focus].name, "xqc", "focus kept its tab");
        add(&mut a, TabKind::Feed);
        assert_eq!(
            kinds(&a)[..3],
            [TabKind::Mentions, TabKind::Feed, TabKind::Whispers]
        );
        assert_eq!(a.channels[a.focus].name, "xqc");
        assert_eq!(a.emotes.len(), a.channels.len(), "emote sets stay aligned");
    }

    #[test]
    fn opening_twice_focuses_the_existing_tab() {
        let mut a = app();
        open(&mut a, TabKind::Feed);
        a.focus = 2;
        open(&mut a, TabKind::Feed);
        assert_eq!(a.channels.len(), 3);
        assert_eq!(a.focus, 0);
    }

    #[test]
    fn mentions_are_logged_even_with_the_tab_closed_and_flag_it_when_open() {
        let mut a = app();
        mention(&mut a, mention_of("xqc", "bob"));
        assert_eq!(a.special.log.rows().count(), 1);
        // opening replays the log without marking it unread
        open(&mut a, TabKind::Mentions);
        assert_eq!(a.channels[0].messages.len(), 1);
        assert_eq!(a.channels[0].unread(), heatsync_core::Unread::None);
        // a new one while another tab is focused pings the tab
        a.focus = 1;
        mention(&mut a, mention_of("kripp", "al"));
        assert_eq!(a.channels[0].messages.len(), 2);
        assert_eq!(a.channels[0].unread(), heatsync_core::Unread::Ping);
        assert_eq!(a.special.log.rows().count(), 2);
    }

    #[test]
    fn a_ping_and_its_rule_match_are_one_row() {
        let mut a = app();
        open(&mut a, TabKind::Mentions);
        // live ping (has the id), then the server's match with a cut snippet
        let mut live = mention_of("xqc", "Bob");
        live.text = "hey mellen look at this".into();
        live.id = Some("m1".into());
        mention(&mut a, live);
        let mut rule = mention_of("xqc", "bob");
        rule.text = "hey mellen look".into();
        mention(&mut a, rule.clone());
        assert_eq!(a.channels[0].messages.len(), 1, "snippet dedupe");
        assert_eq!(a.special.log.rows().count(), 1);
        // same id wins even when the text differs
        let mut other = mention_of("kripp", "al");
        other.id = Some("m1".into());
        mention(&mut a, other);
        assert_eq!(a.channels[0].messages.len(), 1);
        // a different message from the same user is a new row
        let mut next = mention_of("xqc", "bob");
        next.text = "totally different".into();
        mention(&mut a, next);
        assert_eq!(a.channels[0].messages.len(), 2);
        // outside the window the same text counts again
        a.special.recent.clear();
        mention(&mut a, rule);
        assert_eq!(a.channels[0].messages.len(), 3);
    }

    fn follow_ev(chan: &str, what: &str, at_ms: u64) -> (ChatLine, u64) {
        let raw = format!(
            r##"{{"type":"follow:history","events":[{{"type":"follow:stream:online","platform":"twitch","channel":"{chan}","game":"{what}","color":"#ff0000","time":{at_ms}}}]}}"##
        );
        match proto::parse(&raw) {
            Event::FollowHistory(mut h) => h.remove(0),
            e => panic!("{e:?}"),
        }
    }

    fn history(chan: &str, game: &str, at_ms: u64) -> Event {
        let (l, at) = follow_ev(chan, game, at_ms);
        Event::FollowHistory(vec![(l, at)])
    }

    #[test]
    fn follow_history_merges_by_time_dedupes_and_never_persists_or_pings() {
        let mut a = app();
        let mut early = mention_of("xqc", "al");
        early.at = 1_000;
        let mut late = mention_of("xqc", "bo");
        late.at = 3_000;
        late.text = "yo me".into();
        a.special.log.push(early);
        a.special.log.push(late);
        let before = a.special.log.rows().count();
        add(&mut a, TabKind::Mentions);
        let i = find(&a, TabKind::Mentions).unwrap();
        a.focus = a.channels.len() - 1; // not on the mentions tab
        for _ in 0..2 {
            // a reconnect resends the same history
            let Event::FollowHistory(h) = history("cyr", "Doom", 2_000_000) else {
                unreachable!()
            };
            follow_history(&mut a, h);
        }
        let ch = &a.channels[i];
        assert_eq!(
            ch.messages
                .iter()
                .map(|m| m.note.is_some())
                .collect::<Vec<_>>(),
            [false, true, false]
        );
        let f = &ch.messages[1];
        assert_eq!(f.tags[1], Tag::new("[cyr]", TagKind::Origin));
        assert_eq!(f.note.as_ref().unwrap().what, "went live — Doom");
        assert_eq!(f.color.as_deref(), Some("#ff0000"));
        assert_eq!(ch.unread(), heatsync_core::Unread::None);
        assert_eq!(a.special.log.rows().count(), before, "not in the log");
    }

    #[test]
    fn live_follow_go_live_lands_in_the_tab_and_marks_it_yellow() {
        let mut a = app();
        add(&mut a, TabKind::Mentions);
        let i = find(&a, TabKind::Mentions).unwrap();
        a.focus = i + 1;
        let (l, _) = follow_ev("cyr", "Doom", 1);
        follow_event(&mut a, &l);
        follow_event(&mut a, &l); // a repeat adds nothing
        assert_eq!(a.channels[i].messages.len(), 1);
        assert_eq!(a.channels[i].unread(), heatsync_core::Unread::Event);
        assert_eq!(a.special.log.rows().count(), 0);
    }

    #[test]
    fn enter_jumps_to_the_newest_mentions_source_tab() {
        let mut a = app();
        open(&mut a, TabKind::Mentions);
        enter(&mut a);
        assert_eq!(a.status.as_deref(), Some("no mentions yet"));
        mention(&mut a, mention_of("kripp", "bob")); // twitch/kripp isn't open
        a.focus = 0;
        enter(&mut a);
        assert_eq!(a.focus, 0);
        assert_eq!(a.status.as_deref(), Some("not open: kripp"));
        mention(&mut a, mention_of("xqc", "al"));
        enter(&mut a);
        assert_eq!(a.channels[a.focus].name, "xqc");
        // Enter elsewhere does nothing
        let at = a.focus;
        enter(&mut a);
        assert_eq!(a.focus, at);
    }

    #[test]
    fn server_rule_matches_become_mentions() {
        let mut a = app();
        open(&mut a, TabKind::Mentions);
        event(
            &mut a,
            ev(
                r##"{"type":"mention:rule-match","ruleId":1,"pattern":"p","channel":"xqc",
            "platform":"twitch","username":"bob","snippet":"yo","url":"u"}"##,
            ),
        );
        assert_eq!(a.channels[0].messages.len(), 1);
        assert_eq!(a.special.log.rows().count(), 1);
    }

    #[test]
    fn feed_rows_update_in_place_and_dedupe() {
        let mut a = app();
        open(&mut a, TabKind::Feed);
        let row = r##"{"type":"new-message","base36_id":"0a","content":"hi","username":"u","heat":3,"is_op":true}"##;
        event(&mut a, ev(row));
        event(&mut a, ev(row)); // same id twice → one row
        assert_eq!(a.channels[0].messages.len(), 1);
        assert_eq!(a.channels[0].messages[0].heat, 3.0);
        event(
            &mut a,
            ev(r#"{"type":"heat:updated","messageId":"0a","newHeat":88}"#),
        );
        assert_eq!(a.channels[0].messages[0].heat, 88.0);
        event(&mut a, ev(r#"{"type":"message:hidden","id":"0a"}"#));
        assert_eq!(a.channels[0].messages[0].gone.as_deref(), Some("hidden"));
        // heat / hidden for an unknown row are no-ops
        event(
            &mut a,
            ev(r#"{"type":"heat:updated","messageId":"zz","newHeat":1}"#),
        );
    }

    #[test]
    fn rows_for_a_closed_tab_are_dropped() {
        let mut a = app();
        event(
            &mut a,
            ev(r#"{"type":"new-message","base36_id":"0a","content":"hi","username":"u"}"#),
        );
        event(
            &mut a,
            ev(r#"{"type":"whisper:new","from_display_name":"B","content":"x"}"#),
        );
        assert_eq!(a.channels.len(), 2);
    }

    #[test]
    fn whispers_ping_the_tab_when_unfocused() {
        let mut a = app();
        open(&mut a, TabKind::Whispers);
        a.focus = 1;
        event(
            &mut a,
            ev(r#"{"type":"dm:new","from_display_name":"B","content":"x"}"#),
        );
        assert_eq!(a.channels[0].messages[0].tags[0].kind, TagKind::Dm);
        assert_eq!(a.channels[0].unread(), heatsync_core::Unread::Ping);
    }

    #[test]
    fn backlog_lands_behind_live_rows_without_unread() {
        let mut a = app();
        open(&mut a, TabKind::Feed);
        event(
            &mut a,
            ev(r#"{"type":"new-message","base36_id":"03","content":"live","username":"u"}"#),
        );
        let row = |id: &str| {
            proto::feed_row(
                &serde_json::from_str(&format!(
                    r#"{{"base36_id":"{id}","content":"old {id}","username":"u"}}"#
                ))
                .unwrap(),
            )
            .unwrap()
        };
        // oldest first, overlapping the live row's id
        a.special
            .feed_tx
            .send(vec![row("01"), row("02"), row("03")])
            .unwrap();
        drain_backlog(&mut a);
        let ids: Vec<_> = a.channels[0]
            .messages
            .iter()
            .map(|m| m.id.clone().unwrap())
            .collect();
        assert_eq!(ids, vec!["01", "02", "03"]);
        assert_eq!(a.channels[0].messages[2].text, "live");
    }

    #[test]
    fn virtual_tabs_say_what_they_take() {
        let mut a = app();
        assert!(!send(&mut a, TabKind::Mentions, "hi", None));
        assert_eq!(
            a.status.as_deref(),
            Some("mentions is read-only — open a channel to chat")
        );
        // whispers need a name: the hint says how
        assert!(!send(&mut a, TabKind::Whispers, "hi", None));
        assert!(a.status.as_deref().unwrap().contains("/w <user>"));
        // the feed takes posts — without a login it says how to get one
        assert!(!send(&mut a, TabKind::Feed, "hi", None));
        assert!(a.status.as_deref().unwrap().contains("heatsync-tui login"));
    }

    #[test]
    fn a_refused_login_hints_once_and_a_bad_env_token_keeps_its_old_words() {
        let mut a = app();
        a.special.ws_cli = true;
        auth(&mut a, false);
        assert!(a.status.as_deref().unwrap().contains("heatsync-tui login"));
        a.status = None;
        auth(&mut a, false); // every reconnect fails again: stay quiet
        assert_eq!(a.status, None);
        a.special.ws_cli = false;
        auth(&mut a, false);
        assert!(a.status.as_deref().unwrap().contains("HEATSYNC_TOKEN"));
    }
}
