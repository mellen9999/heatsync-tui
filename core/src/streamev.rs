//! stream-event anti-wall state machine — port of the extension's go-live
//! pairing + guards (stream-event-text.js `createGoLivePairing`, main.js
//! `sessionWentLiveSeen`, the 60s text dedupe). pure: the clock is passed in
//! as `now_ms`, nothing here sleeps or reads a timer. feed it every parsed
//! `ChatLine`; lines without stream facts pass straight through.
//!
//! twitch announces one go-live as two events: a first-game `stream:update`
//! ("switched to X") and `stream:online`. an update with no previous game is
//! held `HOLD_MS`; a went-live for the same game swallows it.

use std::collections::{HashMap, HashSet};

use crate::proto::{ChatLine, StreamKind};
use crate::Platform;

/// how long a first "switched to" waits for the "went live" riding with it.
pub const HOLD_MS: u64 = 2_000;
/// a "switched to" for the game a channel just went live with, inside this, is the same news.
pub const ECHO_MS: u64 = 30_000;
/// identical channel+text inside this is a re-broadcast.
pub const DEDUPE_MS: u64 = 60_000;
/// a reload only brings back stored events this young (`replayKeeps`).
pub const REPLAY_WINDOW_MS: u64 = 15 * 60 * 1000;

type Key = (Platform, String);

fn key(l: &ChatLine) -> Key {
    (l.platform, l.channel.to_lowercase())
}

#[derive(Default)]
pub struct StreamEvents {
    /// first-game updates waiting for their go-live: key → (line, due_ms).
    held: HashMap<Key, (ChatLine, u64)>,
    /// channels' last go-live: key → (game, at_ms).
    online: HashMap<Key, (String, u64)>,
    /// channels that already said "went live" this session (offline clears).
    seen: HashSet<Key>,
    /// channel+text → last shown, for the 60s dedupe.
    recent: HashMap<(Key, String), u64>,
}

impl StreamEvents {
    pub fn new() -> Self {
        Self::default()
    }

    /// `follow:live:snapshot`: these were live before we connected, so their
    /// next went-live (a server re-broadcast) is not news.
    pub fn seed_live(&mut self, live: impl IntoIterator<Item = (Platform, String)>) {
        for (p, c) in live {
            self.seen.insert((p, c.to_lowercase()));
        }
    }

    /// one live line. returns what to show now (zero or one line).
    pub fn push(&mut self, line: ChatLine, now_ms: u64) -> Vec<ChatLine> {
        let Some(ev) = line.stream.clone() else {
            return vec![line];
        };
        let k = key(&line);
        match ev.kind {
            StreamKind::Update => {
                if !ev.prev_game.is_empty() {
                    return self.show(line, now_ms);
                }
                if let Some((g, at)) = self.online.get(&k) {
                    if *g == ev.game && now_ms.saturating_sub(*at) <= ECHO_MS {
                        return Vec::new();
                    }
                }
                // a newer held update for the channel replaces the older one.
                self.held.insert(k, (line, now_ms + HOLD_MS));
                Vec::new()
            }
            StreamKind::Online => {
                if self
                    .held
                    .get(&k)
                    .is_some_and(|(h, _)| h.stream.as_ref().is_some_and(|s| s.game == ev.game))
                {
                    self.held.remove(&k);
                }
                if !self.seen.insert(k.clone()) {
                    return Vec::new();
                }
                self.online.insert(k, (ev.game, now_ms));
                self.show(line, now_ms)
            }
            StreamKind::Offline => {
                self.held.remove(&k);
                self.online.remove(&k);
                self.seen.remove(&k);
                self.show(line, now_ms)
            }
        }
    }

    /// backfilled / stored lines: a went-live never replays (it was shown when
    /// it happened); anything else stays only inside the replay window.
    /// `age_ms` = how old the event is, when known.
    pub fn replay_keeps(line: &ChatLine, age_ms: Option<u64>) -> bool {
        match line.stream.as_ref().map(|s| s.kind) {
            Some(StreamKind::Online) => false,
            Some(_) => age_ms.is_none_or(|a| a <= REPLAY_WINDOW_MS),
            None => true,
        }
    }

    /// release held first-game updates whose wait is over. call every frame.
    pub fn tick(&mut self, now_ms: u64) -> Vec<ChatLine> {
        let due: Vec<Key> = self
            .held
            .iter()
            .filter(|(_, (_, at))| *at <= now_ms)
            .map(|(k, _)| k.clone())
            .collect();
        let mut out = Vec::new();
        for k in due {
            if let Some((line, _)) = self.held.remove(&k) {
                out.extend(self.show(line, now_ms));
            }
        }
        out
    }

    /// the 60s identical-text dedupe, then emit.
    fn show(&mut self, line: ChatLine, now_ms: u64) -> Vec<ChatLine> {
        let text = line
            .note
            .as_ref()
            .map(|n| n.what.clone())
            .unwrap_or_default();
        let id = (key(&line), text);
        if self
            .recent
            .get(&id)
            .is_some_and(|at| now_ms.saturating_sub(*at) < DEDUPE_MS)
        {
            return Vec::new();
        }
        if self.recent.len() >= 256 {
            self.recent
                .retain(|_, at| now_ms.saturating_sub(*at) < DEDUPE_MS);
        }
        self.recent.insert(id, now_ms);
        vec![line]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{parse, Event};

    fn line(json: &str) -> ChatLine {
        match parse(json) {
            Event::Chat(l) => l,
            e => panic!("not a line: {e:?}"),
        }
    }
    fn online(c: &str, g: &str) -> ChatLine {
        line(&format!(
            r#"{{"type":"stream:online","platform":"twitch","channel":"{c}","game":"{g}"}}"#
        ))
    }
    fn update(c: &str, g: &str, prev: &str) -> ChatLine {
        line(&format!(
            r#"{{"type":"stream:update","platform":"twitch","channel":"{c}","game":"{g}","prevGame":"{prev}"}}"#
        ))
    }
    fn offline(c: &str) -> ChatLine {
        line(&format!(
            r#"{{"type":"stream:offline","platform":"twitch","channel":"{c}"}}"#
        ))
    }
    fn whats(v: &[ChatLine]) -> Vec<String> {
        v.iter()
            .map(|l| l.note.as_ref().unwrap().what.clone())
            .collect()
    }

    #[test]
    fn plain_chat_passes_through() {
        let mut s = StreamEvents::new();
        let l = line(
            r#"{"type":"stream:redeem","platform":"twitch","channel":"c","user":"u","title":"t","cost":1}"#,
        );
        assert_eq!(s.push(l.clone(), 0), vec![l]);
    }

    #[test]
    fn real_switch_is_shown_at_once() {
        let mut s = StreamEvents::new();
        assert_eq!(whats(&s.push(update("c", "B", "A"), 0)), ["switched to B"]);
    }

    #[test]
    fn first_game_update_waits_then_releases() {
        let mut s = StreamEvents::new();
        assert!(s.push(update("c", "X", ""), 1_000).is_empty());
        assert!(s.tick(2_999).is_empty());
        assert_eq!(whats(&s.tick(3_000)), ["switched to X"]);
        assert!(s.tick(9_000).is_empty(), "released once");
    }

    #[test]
    fn went_live_for_same_game_swallows_held_update() {
        let mut s = StreamEvents::new();
        assert!(s.push(update("c", "X", ""), 0).is_empty());
        assert_eq!(whats(&s.push(online("c", "X"), 500)), ["went live — X"]);
        assert!(s.tick(5_000).is_empty());
    }

    #[test]
    fn went_live_for_other_game_keeps_held_update() {
        let mut s = StreamEvents::new();
        s.push(update("c", "X", ""), 0);
        assert_eq!(whats(&s.push(online("c", "Y"), 500)), ["went live — Y"]);
        assert_eq!(whats(&s.tick(2_000)), ["switched to X"]);
    }

    #[test]
    fn same_game_update_just_after_go_live_is_dropped() {
        let mut s = StreamEvents::new();
        s.push(online("c", "X"), 0);
        assert!(s.push(update("c", "X", ""), 30_000).is_empty());
        assert!(s.tick(40_000).is_empty());
        // past the echo window it is held like any first-game update
        assert!(s.push(update("c", "X", ""), 30_001).is_empty());
        assert_eq!(whats(&s.tick(32_001)), ["switched to X"]);
    }

    #[test]
    fn one_went_live_per_session_until_offline() {
        let mut s = StreamEvents::new();
        assert_eq!(s.push(online("c", "X"), 0).len(), 1);
        assert!(s.push(online("c", "X"), 100_000).is_empty());
        assert!(s.push(online("c", "Y"), 200_000).is_empty());
        assert_eq!(whats(&s.push(offline("c"), 300_000)), ["went offline"]);
        assert_eq!(s.push(online("c", "X"), 400_000).len(), 1);
    }

    #[test]
    fn channels_are_independent_and_case_blind() {
        let mut s = StreamEvents::new();
        assert_eq!(s.push(online("Cyr", "X"), 0).len(), 1);
        assert_eq!(s.push(online("other", "X"), 0).len(), 1);
        assert!(s.push(online("cyr", "X"), 1).is_empty());
    }

    #[test]
    fn identical_text_inside_a_minute_is_dropped() {
        let mut s = StreamEvents::new();
        assert_eq!(s.push(update("c", "B", "A"), 0).len(), 1);
        assert!(s.push(update("c", "B", "A"), 59_999).is_empty());
        assert_eq!(s.push(update("c", "B", "A"), 60_000).len(), 1);
        // a different channel with the same words still shows
        assert_eq!(s.push(update("d", "B", "A"), 1).len(), 1);
    }

    #[test]
    fn offline_drops_a_held_update_and_resets_echo() {
        let mut s = StreamEvents::new();
        s.push(online("c", "X"), 0);
        s.push(offline("c"), 1_000);
        s.push(update("c", "X", ""), 2_000);
        s.push(offline("c"), 2_500);
        assert!(s.tick(10_000).is_empty());
        // echo window cleared: next first-game update is held, not dropped
        s.push(update("c", "X", ""), 11_000);
        assert_eq!(s.tick(13_000).len(), 1);
    }

    #[test]
    fn newer_held_update_replaces_older() {
        let mut s = StreamEvents::new();
        s.push(update("c", "X", ""), 0);
        s.push(update("c", "Y", ""), 1_000);
        assert_eq!(whats(&s.tick(3_000)), ["switched to Y"]);
    }

    #[test]
    fn snapshot_seeds_already_live_so_rebroadcast_is_silent() {
        let mut s = StreamEvents::new();
        s.seed_live([(Platform::Twitch, "Cyr".to_string())]);
        assert!(s.push(online("cyr", "X"), 0).is_empty());
        // but it can go offline and come back for real
        s.push(offline("cyr"), 1);
        assert_eq!(s.push(online("cyr", "X"), 2).len(), 1);
    }

    #[test]
    fn follow_snapshot_event_feeds_seed() {
        let mut s = StreamEvents::new();
        let raw =
            r#"{"type":"follow:live:snapshot","streams":[{"username":"a","platform":"twitch"}]}"#;
        match parse(raw) {
            Event::FollowLive(v) => s.seed_live(v),
            _ => panic!(),
        }
        assert!(s.push(online("a", ""), 0).is_empty());
    }

    #[test]
    fn replay_never_brings_back_went_live() {
        let fresh = Some(1_000);
        let old = Some(REPLAY_WINDOW_MS + 1);
        let on = online("c", "X");
        let up = update("c", "B", "A");
        let off = offline("c");
        let chat = line(
            r#"{"type":"stream:redeem","platform":"twitch","channel":"c","user":"u","title":"t"}"#,
        );
        for (l, age, want) in [
            (&on, fresh, false),
            (&on, None, false),
            (&up, fresh, true),
            (&up, old, false),
            (&off, fresh, true),
            (&off, old, false),
            (&up, Some(REPLAY_WINDOW_MS), true),
            (&chat, old, true),
        ] {
            assert_eq!(
                StreamEvents::replay_keeps(l, age),
                want,
                "{:?} {age:?}",
                l.note
            );
        }
    }
}
