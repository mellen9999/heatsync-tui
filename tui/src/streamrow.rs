//! the went-live / switched / went-offline rows: `cyr ◆ went live — Doom`.
//! this file decides who a row names (colour + paint, resolved at draw time so
//! a better source restyles it the moment it lands), how it reads, and the
//! history rows that join the room tabs (the server's last 15 min, and the
//! start of a live twitch stream).

use std::collections::{HashMap, VecDeque};

use heatsync_core::proto::ChatLine;
use heatsync_core::streamev::StreamEvents;
use heatsync_core::{Channel, Extra, Message, Note, NoteKind, Platform, TabKind};
use ratatui::style::{Color, Style};

use crate::app::App;
use crate::followlog::FollowLog;
use crate::paints::{Paints, UserLook};

/// logins remembered from chat (colour + paint id); the oldest go first.
const SEEN_CAP: usize = 2048;
/// the same replayed event, seen twice, lands inside this many seconds.
const DUP_SECS: u64 = 60;
/// a live push and the stream-start row for one go-live land this close.
const START_DUP_SECS: u64 = 300;

#[derive(Default)]
struct Who {
    color: Option<String>,
    uid: Option<String>,
}

/// what chat has told us about a login on a platform this session.
#[derive(Default)]
pub(crate) struct Seen {
    map: HashMap<(Platform, String), Who>,
    order: VecDeque<(Platform, String)>,
}

impl Seen {
    /// remember a chatter's colour and paint-space id. a missing one never
    /// erases a known one.
    pub(crate) fn note(
        &mut self,
        platform: Platform,
        login: &str,
        color: Option<&str>,
        uid: Option<&str>,
    ) {
        if login.is_empty() || (color.is_none() && uid.is_none()) {
            return;
        }
        let k = (platform, login.to_lowercase());
        if !self.map.contains_key(&k) {
            self.order.push_back(k.clone());
            while self.order.len() > SEEN_CAP {
                if let Some(old) = self.order.pop_front() {
                    self.map.remove(&old);
                }
            }
        }
        let w = self.map.entry(k).or_default();
        if let Some(c) = color {
            w.color = Some(c.to_string());
        }
        if let Some(u) = uid {
            w.uid = Some(u.to_string());
        }
    }

    fn get(&self, platform: Platform, login: &str) -> Option<&Who> {
        self.map.get(&(platform, login.to_lowercase()))
    }

    pub(crate) fn color(&self, platform: Platform, login: &str) -> Option<&str> {
        self.get(platform, login)?.color.as_deref()
    }

    pub(crate) fn uid(&self, platform: Platform, login: &str) -> Option<&str> {
        self.get(platform, login)?.uid.as_deref()
    }
}

/// is this one of the three stream rows (not a mentions-tab echo, which has no
/// name, and not a raid / hype / spike)?
pub(crate) fn is_row(m: &Message) -> bool {
    !m.user.is_empty()
        && matches!(
            m.note.as_ref().map(|n| n.kind),
            Some(NoteKind::Live | NoteKind::Offline | NoteKind::Category)
        )
}

/// the row's words: the verb (with ◆ and the dash), its colour, and the game.
pub(crate) fn parts(n: &Note) -> (String, Style, &str) {
    let s = Style::new();
    match n.kind {
        NoteKind::Live => match n.what.split_once(" — ") {
            Some((v, game)) => (format!("◆ {v} —"), s.fg(Color::Yellow), game),
            None => (format!("◆ {}", n.what), s.fg(Color::Yellow), ""),
        },
        NoteKind::Category => {
            let game = n.what.strip_prefix("switched to ").unwrap_or("");
            ("◆ switched to".into(), s.fg(Color::Yellow), game)
        }
        _ => (format!("◆ {}", n.what), s.fg(Color::White), ""),
    }
}

/// the name colour, first source that answers: the frame's own, the
/// follow:colors map (twitch logins), what chat showed, the profile lookup.
/// `None` = fall to the tui's unknown-name white.
pub(crate) fn pick_color(
    frame: Option<&str>,
    map: Option<&str>,
    chat: Option<&str>,
    profile: Option<&str>,
) -> Option<String> {
    frame.or(map).or(chat).or(profile).map(str::to_string)
}

/// the look of a row's name at draw time: the paint for the streamer's id (the
/// frame's, else the one chat taught us), and the colour chain above.
pub(crate) fn look(m: &Message, paints: &Paints, follow: &FollowLog, seen: &Seen) -> UserLook {
    let uid = m.uid.as_deref().or_else(|| seen.uid(m.platform, &m.user));
    let prof = paints
        .look_for(uid, &m.user, true)
        .map(|l| l.into_owned())
        .unwrap_or_default();
    let map = (m.platform == Platform::Twitch)
        .then(|| follow.login_color(&m.user))
        .flatten();
    UserLook {
        paint: prof.paint,
        color: pick_color(
            m.color.as_deref(),
            map,
            seen.color(m.platform, &m.user),
            prof.color.as_deref(),
        ),
        badge: None,
    }
}

/// a stream line as a tab message stamped `ts` seconds.
pub(crate) fn message(l: ChatLine, ts: u64) -> Message {
    Message {
        platform: l.platform,
        user: l.user,
        text: String::new(),
        color: l.color,
        badges: Vec::new(),
        reply_to: None,
        id: l.id,
        gone: None,
        note: l.note,
        emotes: None,
        tags: Vec::new(),
        extra: Extra {
            ts,
            ..Default::default()
        },
        uid: l.uid,
        heat: 0.0,
    }
}

/// the tabs a followed channel's event belongs in: its own, else every room tab.
pub(crate) fn targets(channels: &[Channel], platform: Platform, channel: &str) -> Vec<usize> {
    let own: Vec<usize> = (0..channels.len())
        .filter(|&i| channels[i].matches(platform, channel))
        .collect();
    if !own.is_empty() {
        return own;
    }
    (0..channels.len())
        .filter(|&i| channels[i].kind == TabKind::Channel)
        .collect()
}

/// is `m` already in the tab: same stable id, or the same event a moment apart.
fn already(ch: &Channel, m: &Message) -> bool {
    let kind = m.note.as_ref().map(|n| n.kind);
    ch.messages.iter().any(|e| {
        if m.id.is_some() && e.id == m.id {
            return true;
        }
        if !is_row(e) || e.platform != m.platform || e.user != m.user {
            return false;
        }
        if kind == Some(NoteKind::Live) {
            return e.note.as_ref().map(|n| n.kind) == kind
                && e.extra.ts.abs_diff(m.extra.ts) <= START_DUP_SECS;
        }
        e.note == m.note && e.extra.ts.abs_diff(m.extra.ts) <= DUP_SECS
    })
}

/// slot a history row into a tab by its own time. quiet: no heat, no unread, no light.
pub(crate) fn place(ch: &mut Channel, m: Message) -> bool {
    !already(ch, &m) && ch.insert_by_ts(m)
}

/// `follow:history`: switched / offline rows younger than the replay window
/// join the room tabs at their own times. a went-live is never replayed (the
/// stream-start row covers it).
pub(crate) fn replay(app: &mut App, events: &[(ChatLine, u64)], now_secs: u64) {
    let now_ms = now_secs.saturating_mul(1000);
    for (l, at_ms) in events {
        let ts = (at_ms / 1000).min(now_secs);
        let m = message(l.clone(), ts);
        if l.stream.is_none()
            || !is_row(&m)
            || !StreamEvents::replay_keeps(l, Some(now_ms.saturating_sub(*at_ms)))
        {
            continue;
        }
        if let Some(u) = &l.uid {
            app.paints.want(u);
        }
        for i in targets(&app.channels, l.platform, &l.channel) {
            place(&mut app.channels[i], m.clone());
        }
    }
}

/// the row for the moment a live stream began.
pub(crate) fn went_live(login: &str, start: u64, game: &str) -> Message {
    let login = login.to_lowercase();
    let what = if game.is_empty() {
        "went live".to_string()
    } else {
        format!("went live — {game}")
    };
    Message {
        platform: Platform::Twitch,
        user: login.clone(),
        text: String::new(),
        color: None,
        badges: Vec::new(),
        reply_to: None,
        id: Some(format!("went-live:{login}:{start}")),
        gone: None,
        note: Some(Note {
            kind: NoteKind::Live,
            what,
        }),
        emotes: None,
        tags: Vec::new(),
        extra: Extra {
            ts: start,
            ..Default::default()
        },
        uid: None,
        heat: 0.0,
    }
}

/// a twitch tab's history just loaded: if the stream is live and the history
/// reaches back to its start, the went-live row goes in at that time. true =
/// inserted. `oldest` is the history's oldest known time (0 = unknown).
pub(crate) fn start_row(
    ch: &mut Channel,
    login: &str,
    oldest: u64,
    start: u64,
    game: &str,
    now: u64,
) -> bool {
    if oldest == 0 || start == 0 || start > now || oldest > start {
        return false;
    }
    place(ch, went_live(login, start, game))
}

#[cfg(test)]
mod tests {
    use super::*;
    use heatsync_core::proto::{parse, Event};
    use heatsync_core::Unread;

    fn chat_line(raw: &str) -> ChatLine {
        match parse(raw) {
            Event::Chat(l) => l,
            e => panic!("{e:?}"),
        }
    }

    fn row_of(raw: &str) -> Message {
        message(chat_line(raw), 100)
    }

    #[test]
    fn the_verb_wears_its_colour_and_the_game_is_white() {
        for (raw, verb, hue, game) in [
            (
                r#"{"type":"stream:online","platform":"twitch","channel":"C","game":"Doom"}"#,
                "◆ went live —",
                Color::Yellow,
                "Doom",
            ),
            (
                r#"{"type":"stream:online","platform":"twitch","channel":"c"}"#,
                "◆ went live",
                Color::Yellow,
                "",
            ),
            (
                r#"{"type":"stream:update","platform":"twitch","channel":"c","game":"B","prevGame":"A"}"#,
                "◆ switched to",
                Color::Yellow,
                "B",
            ),
            (
                r#"{"type":"stream:offline","platform":"twitch","channel":"c"}"#,
                "◆ went offline",
                Color::White,
                "",
            ),
        ] {
            let m = row_of(raw);
            assert!(is_row(&m));
            let (v, s, g) = parts(m.note.as_ref().unwrap());
            assert_eq!((v.as_str(), s.fg, g), (verb, Some(hue), game), "{raw}");
        }
    }

    #[test]
    fn raids_and_nameless_echoes_are_not_rows() {
        let raid = row_of(
            r#"{"type":"stream:raid","platform":"twitch","channel":"c","target":"t","viewers":3}"#,
        );
        assert!(!is_row(&raid));
        let mut echo = row_of(r#"{"type":"stream:offline","platform":"twitch","channel":"c"}"#);
        echo.user.clear();
        assert!(!is_row(&echo));
    }

    #[test]
    fn name_colour_goes_frame_then_map_then_chat_then_profile() {
        let all = (
            Some("#010101"),
            Some("#020202"),
            Some("#030303"),
            Some("#040404"),
        );
        let got = |f, m, c, p| pick_color(f, m, c, p);
        assert_eq!(got(all.0, all.1, all.2, all.3).as_deref(), all.0);
        assert_eq!(got(None, all.1, all.2, all.3).as_deref(), all.1);
        assert_eq!(got(None, None, all.2, all.3).as_deref(), all.2);
        assert_eq!(got(None, None, None, all.3).as_deref(), all.3);
        assert_eq!(got(None, None, None, None), None);
    }

    #[test]
    fn look_walks_the_chain_and_restyles_when_a_source_lands() {
        let paints = Paints::new();
        let mut follow = FollowLog::default();
        let mut seen = Seen::default();
        let m = row_of(r#"{"type":"stream:offline","platform":"twitch","channel":"cyr"}"#);
        assert_eq!(look(&m, &paints, &follow, &seen).color, None);
        seen.note(Platform::Twitch, "Cyr", Some("#030303"), None);
        assert_eq!(
            look(&m, &paints, &follow, &seen).color.as_deref(),
            Some("#030303")
        );
        follow.set_colors(vec![("cyr".into(), "#020202".into())]);
        assert_eq!(
            look(&m, &paints, &follow, &seen).color.as_deref(),
            Some("#020202")
        );
        let framed = row_of(
            r##"{"type":"stream:offline","platform":"twitch","channel":"cyr","color":"#010101"}"##,
        );
        assert_eq!(
            look(&framed, &paints, &follow, &seen).color.as_deref(),
            Some("#010101")
        );
        // the follow map is twitch logins: a kick row never borrows it
        let kick = row_of(r#"{"type":"stream:offline","platform":"kick","channel":"cyr"}"#);
        assert_eq!(look(&kick, &paints, &follow, &seen).color, None);
    }

    #[test]
    fn a_paint_is_found_by_the_frames_id_else_the_one_chat_taught() {
        let mut paints = Paints::new();
        let paint = |hex: &str| UserLook {
            paint: heatsync_core::paint::Paint::from_heatsync(&serde_json::json!({"base": {
                "type": "linear", "angle": 90,
                "stops": [{"color": hex, "pos": 0}, {"color": hex, "pos": 100}]}})),
            ..Default::default()
        };
        paints.insert_for_test("777", paint("#ff0000"));
        paints.insert_for_test("kick_9", paint("#00ff00"));
        let (follow, mut seen) = (FollowLog::default(), Seen::default());
        let framed = row_of(
            r#"{"type":"stream:offline","platform":"twitch","channel":"cyr","userId":"777"}"#,
        );
        assert_eq!(framed.uid.as_deref(), Some("777"));
        assert!(look(&framed, &paints, &follow, &seen).paint.is_some());
        // no id on the frame: nothing, until chat shows the login's id
        let bare = row_of(r#"{"type":"stream:offline","platform":"kick","channel":"cyr"}"#);
        assert!(look(&bare, &paints, &follow, &seen).paint.is_none());
        seen.note(Platform::Kick, "cyr", None, Some("kick_9"));
        assert!(look(&bare, &paints, &follow, &seen).paint.is_some());
        // the same login on twitch is a different streamer
        let tw = row_of(r#"{"type":"stream:offline","platform":"twitch","channel":"cyr"}"#);
        assert!(look(&tw, &paints, &follow, &seen).paint.is_none());
    }

    fn tabs() -> Vec<Channel> {
        vec![
            Channel::special(TabKind::Mentions, 10),
            Channel::new("xqc", Platform::Twitch, 50),
            Channel::new("kripp", Platform::Kick, 50),
        ]
    }

    #[test]
    fn a_tabbed_channel_keeps_its_own_row_and_a_stranger_goes_everywhere() {
        let ch = tabs();
        assert_eq!(targets(&ch, Platform::Twitch, "XQC"), [1]);
        assert_eq!(targets(&ch, Platform::Twitch, "cyr"), [1, 2]);
        assert_eq!(
            targets(&ch, Platform::Kick, "xqc"),
            [1, 2],
            "other platform"
        );
    }

    fn hist(raw: &[(&str, u64)]) -> Vec<(ChatLine, u64)> {
        raw.iter().map(|(r, at)| (chat_line(r), *at)).collect()
    }

    const NOW: u64 = 10_000;

    #[test]
    fn history_replays_switched_and_offline_under_15_min_never_online() {
        let mut app = crate::special::tests::app();
        crate::special::add(&mut app, TabKind::Mentions);
        let ev = |ty: &str, extra: &str| {
            format!(
                r##"{{"type":"follow:stream:{ty}","platform":"twitch","channel":"cyr","color":"#ff00aa","userId":"55"{extra}}}"##
            )
        };
        let h = hist(&[
            (&ev("online", r#","game":"Doom""#), (NOW - 60) * 1000),
            (
                &ev("update", r#","game":"B","prevGame":"A""#),
                (NOW - 600) * 1000,
            ),
            (&ev("offline", ""), (NOW - 100) * 1000),
            (
                &ev("update", r#","game":"Old","prevGame":"A""#),
                (NOW - 901) * 1000,
            ),
        ]);
        replay(&mut app, &h, NOW);
        for ch in &app.channels[1..] {
            let got: Vec<_> = ch
                .messages
                .iter()
                .map(|m| {
                    (
                        m.extra.ts,
                        m.note.as_ref().unwrap().what.clone(),
                        m.uid.clone(),
                    )
                })
                .collect();
            assert_eq!(
                got,
                [
                    (
                        NOW - 600,
                        "switched to B".to_string(),
                        Some("55".to_string())
                    ),
                    (
                        NOW - 100,
                        "went offline".to_string(),
                        Some("55".to_string())
                    ),
                ]
            );
            assert_eq!(ch.messages[0].color.as_deref(), Some("#ff00aa"));
            assert_eq!(ch.unread(), Unread::None);
            assert_eq!(ch.heat, 0.0);
        }
        // a reconnect resends it all: nothing doubles
        replay(&mut app, &h, NOW);
        assert_eq!(app.channels[1].messages.len(), 2);
        // and the mentions tab got nothing from here
        assert!(app.channels[0].messages.is_empty());
    }

    #[test]
    fn a_followed_channel_with_a_tab_replays_only_there() {
        let mut app = crate::special::tests::app();
        let h = hist(&[(
            r#"{"type":"follow:stream:offline","platform":"twitch","channel":"xqc"}"#,
            (NOW - 5) * 1000,
        )]);
        replay(&mut app, &h, NOW);
        let lens: Vec<_> = app.channels.iter().map(|c| c.messages.len()).collect();
        assert_eq!(lens, [1, 0]);
    }

    #[test]
    fn a_followed_row_lands_in_every_room_tab_quietly_and_keeps_its_flash() {
        let mut app = crate::special::tests::app();
        crate::special::add(&mut app, TabKind::Mentions);
        let l = chat_line(
            r##"{"type":"follow:stream:online","platform":"twitch","channel":"cyr","game":"Doom","color":"#ff00aa","userId":"55"}"##,
        );
        let out = crate::feed::route(&mut app, l, 0);
        assert!(out.is_empty());
        assert_eq!(app.status.as_deref(), Some("cyr went live — Doom"));
        for ch in app.channels.iter().filter(|c| c.kind == TabKind::Channel) {
            assert_eq!(ch.messages.len(), 1, "{}", ch.name);
            let m = &ch.messages[0];
            assert_eq!((m.user.as_str(), m.uid.as_deref()), ("cyr", Some("55")));
            assert_eq!((ch.unread(), ch.heat), (Unread::None, 0.0));
        }
        assert_eq!(app.channels[0].messages.len(), 1, "mentions row kept");
        // a channel with a tab: its follow twin is dropped, the room's own event lands
        let own = chat_line(
            r#"{"type":"stream:offline","platform":"twitch","channel":"xqc","userId":"9"}"#,
        );
        let out = crate::feed::route(&mut app, own, 1);
        assert_eq!(out.len(), 1);
        let twin =
            chat_line(r#"{"type":"follow:stream:offline","platform":"twitch","channel":"xqc"}"#);
        assert!(crate::feed::route(&mut app, twin, 2).is_empty());
        let xqc = app.channels.iter().position(|c| c.name == "xqc").unwrap();
        assert_eq!(app.channels[xqc].messages.len(), 1);
    }

    #[test]
    fn chat_teaches_seen_the_colour_and_the_id() {
        let mut app = crate::special::tests::app();
        let chat = chat_line(
            r##"{"type":"irc:message","channel":"xqc","message":{"type":"privmsg","username":"Cyr","content":"hi","color":"#123456","userId":"42"}}"##,
        );
        crate::feed::deliver(&mut app, chat, 0);
        assert_eq!(app.seen.color(Platform::Twitch, "cyr"), Some("#123456"));
        assert_eq!(app.seen.uid(Platform::Twitch, "cyr"), Some("42"));
        // a stream row never teaches it anything
        let row = chat_line(
            r##"{"type":"stream:offline","platform":"twitch","channel":"xqc","color":"#ffffff"}"##,
        );
        crate::feed::deliver(&mut app, row, 1);
        assert_eq!(app.seen.color(Platform::Twitch, "xqc"), None);
    }

    #[test]
    fn the_start_row_needs_history_that_reaches_it_and_lands_once() {
        let mut ch = Channel::new("cyr", Platform::Twitch, 50);
        let mut chat = |t: u64| {
            let mut m = went_live("x", 0, "");
            m.note = None;
            m.id = None;
            m.user = "u".into();
            m.text = "hi".into();
            m.extra.ts = t;
            ch.system(m);
        };
        chat(5_000);
        chat(6_000);
        // history starts after the stream did: not reached
        assert!(!start_row(&mut ch, "Cyr", 5_000, 4_000, "Doom", NOW));
        // unknown history time / a start in the future: no
        assert!(!start_row(&mut ch, "Cyr", 0, 4_000, "Doom", NOW));
        assert!(!start_row(&mut ch, "Cyr", 5_000, NOW + 1, "Doom", NOW));
        assert_eq!(ch.messages.len(), 2);
        assert!(start_row(&mut ch, "Cyr", 5_000, 5_500, "Doom", NOW));
        assert!(
            !start_row(&mut ch, "Cyr", 5_000, 5_500, "Doom", NOW),
            "once"
        );
        let order: Vec<_> = ch.messages.iter().map(|m| m.extra.ts).collect();
        assert_eq!(order, [5_000, 5_500, 6_000]);
        let m = &ch.messages[1];
        assert_eq!(m.id.as_deref(), Some("went-live:cyr:5500"));
        assert_eq!(m.note.as_ref().unwrap().what, "went live — Doom");
        assert_eq!((ch.unread(), ch.heat), (Unread::None, 0.0));
    }

    #[test]
    fn a_live_went_live_row_already_there_blocks_the_start_row() {
        let mut ch = Channel::new("cyr", Platform::Twitch, 50);
        ch.system(went_live("cyr", 5_100, "Doom"));
        let mut other_id = went_live("cyr", 5_100, "Doom");
        other_id.id = None;
        ch.system(other_id);
        assert!(!start_row(&mut ch, "cyr", 4_000, 5_000, "Doom", NOW));
    }

    #[test]
    fn seen_remembers_and_is_bounded() {
        let mut s = Seen::default();
        s.note(Platform::Twitch, "Cyr", Some("#111111"), Some("5"));
        s.note(Platform::Twitch, "cyr", None, None);
        s.note(Platform::Twitch, "cyr", Some("#222222"), None);
        assert_eq!(s.color(Platform::Twitch, "CYR"), Some("#222222"));
        assert_eq!(s.uid(Platform::Twitch, "cyr"), Some("5"));
        assert_eq!(s.color(Platform::Kick, "cyr"), None);
        for i in 0..SEEN_CAP + 10 {
            s.note(Platform::Kick, &format!("u{i}"), Some("#333333"), None);
        }
        assert!(s.map.len() <= SEEN_CAP);
    }
}
