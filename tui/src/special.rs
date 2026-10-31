//! the virtual tabs: mentions, feed, whispers. they live in `app.channels`
//! like any tab (so focus, unread colouring and the tab bar need no second
//! code path) but join nothing — core's `Channel::special` has no sources, so
//! no line, part, emote fetch or history call ever reaches them. they pin left
//! of the channel tabs in [`TabKind`] order, and which are open persists.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

use heatsync_core::emote::{ref_set, EmoteSet};
use heatsync_core::proto::SpecialLine;
use heatsync_core::{Channel, Message, Platform, TabKind, Tag, TagKind};

use crate::app::{App, Feed};
use crate::mentions::{self, Mention};
use crate::net::{ChatEvent, Outbound};
use crate::save_state;

/// rows a virtual tab keeps (the ring cap, like a channel's).
const CAP: usize = 500;
/// feed rows fetched when the tab opens.
const FEED_BACKLOG: usize = 30;

pub(crate) struct State {
    pub(crate) log: mentions::Log,
    feed_tx: Sender<Vec<SpecialLine>>,
    feed_rx: Receiver<Vec<SpecialLine>>,
}

impl State {
    pub(crate) fn new() -> State {
        State::with_log(mentions::Log::load())
    }

    fn with_log(log: mentions::Log) -> State {
        let (feed_tx, feed_rx) = channel();
        State {
            log,
            feed_tx,
            feed_rx,
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
        for m in app.special.log.rows() {
            ch.deliver(mention_msg(m));
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
    if kind == TabKind::Feed {
        start_feed(app);
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

/// a mention landed (a live ping, or a server rule match): always remembered,
/// and shown + flagged when the tab is open.
pub(crate) fn mention(app: &mut App, m: Mention) {
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

/// text composed on a virtual tab. only whispers can take one, and sending
/// waits on the server's /api/cli routes — this is the single place it lands.
pub(crate) fn send(app: &mut App, kind: TabKind, _text: &str) {
    app.status = Some(match kind {
        TabKind::Whispers => "whispers are read-only here for now".into(),
        _ => format!("{} is read-only — open a channel to chat", kind.name()),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::InputMode;
    use crate::{hsauth, picker};
    use heatsync_core::proto::{self, Event};
    use heatsync_core::room::RoomModes;
    use heatsync_core::streamev::StreamEvents;
    use heatsync_core::{edit, mock};

    /// an app with two real tabs, a mock feed and an in-memory mention log —
    /// nothing touches the network or ~/.config.
    fn app() -> App {
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
    fn virtual_tabs_are_read_only() {
        let mut a = app();
        send(&mut a, TabKind::Mentions, "hi");
        assert_eq!(
            a.status.as_deref(),
            Some("mentions is read-only — open a channel to chat")
        );
        send(&mut a, TabKind::Whispers, "hi");
        assert_eq!(
            a.status.as_deref(),
            Some("whispers are read-only here for now")
        );
    }
}
