//! keeping an open room's emote set current. the set is fetched once when a tab
//! opens; this re-reads it from heatsync.org (never from a third party) on a
//! reconnect, on a jittered timer while the tab is open, and on `/emotes refresh`.
//!
//! the rules that keep it safe:
//! - a refresh REPLACES the room's set (so a renamed or removed emote stops
//!   drawing under its old name); it is built whole and swapped in one step.
//! - a failed fetch keeps the last good set, and says "emotes may be stale" once
//!   per outage, never per retry.
//! - the server's ETag rides along (`If-None-Match`), so an unchanged set costs a
//!   header, not the body.
//! - nothing is fetched or kept for a room that is not open: entries are dropped
//!   the moment their tab closes, and one fetch runs per key at a time.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use heatsync_core::emote::EmoteSet;
use heatsync_core::Platform;

/// (platform, lowercased channel): the unit that is fetched.
pub type Key = (Platform, String);

/// how often an open room is re-read.
pub const PERIOD: Duration = Duration::from_secs(300);
/// ± this much of it, so many clients never move in step.
const JITTER: Duration = Duration::from_secs(60);
/// a failed refresh is retried sooner than a good one.
const RETRY: Duration = Duration::from_secs(90);
/// after a reconnect the refresh is spread over this, so a server restart
/// doesn't draw every client back at the same second.
const RECONNECT_SPREAD: Duration = Duration::from_secs(20);
/// a room read this recently is not re-read because the socket flapped.
const FLAP_GAP: Duration = Duration::from_secs(60);
/// fetches in flight at once.
const MAX_INFLIGHT: usize = 4;

pub fn key(platform: Platform, name: &str) -> Key {
    (platform, name.to_ascii_lowercase())
}

/// what one fetch came back with.
#[derive(Debug)]
pub enum Outcome {
    /// a new body, and the validator to send next time.
    Changed(EmoteSet, Option<String>),
    /// the server says what we hold is current.
    NotModified,
    /// no usable answer.
    Failed,
}

/// what applying one outcome means for the screen.
#[derive(Debug, PartialEq)]
pub struct Applied {
    pub key: Key,
    /// the room's set changed: rebuild the tabs that hold it.
    pub changed: bool,
    /// say "emotes may be stale" (first failure of an outage only).
    pub say_stale: bool,
    /// a hand-started refresh: its result is worth a line either way.
    pub manual: bool,
    pub failed: bool,
}

struct Entry {
    set: Option<EmoteSet>,
    etag: Option<String>,
    due: Instant,
    inflight: bool,
    stale_said: bool,
    manual: bool,
    /// when this room last answered well.
    fresh: Option<Instant>,
}

pub struct State {
    entries: HashMap<Key, Entry>,
    tx: Sender<(Key, Outcome)>,
    rx: Receiver<(Key, Outcome)>,
    /// the socket dropped since the last time it was up.
    was_down: bool,
    seed: u64,
    /// the earliest the open rooms are looked at again (once a second is plenty).
    look: Instant,
}

impl State {
    pub fn new() -> State {
        let (tx, rx) = channel();
        State {
            entries: HashMap::new(),
            tx,
            rx,
            was_down: false,
            look: Instant::now(),
            seed: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(1, |d| d.as_nanos() as u64 | 1),
        }
    }

    /// true at most once a second: the caller's cue to look at the open rooms.
    pub fn look(&mut self, now: Instant) -> bool {
        if now < self.look {
            return false;
        }
        self.look = now + Duration::from_secs(1);
        true
    }

    /// a small spread in `[0, max)`, from a xorshift (no rng dependency).
    fn spread(&mut self, max: Duration) -> Duration {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        let ms = max.as_millis().max(1) as u64;
        Duration::from_millis(self.seed % ms)
    }

    /// the next due time for a good result: PERIOD ± JITTER.
    fn next_due(&mut self, now: Instant) -> Instant {
        now + PERIOD - JITTER + self.spread(JITTER * 2)
    }

    /// the set last fetched for this key (the first fetch seeds it).
    pub fn set(&self, k: &Key) -> Option<&EmoteSet> {
        self.entries.get(k).and_then(|e| e.set.as_ref())
    }

    /// the first fetch of a room's set landed (from the open path). an empty
    /// set is a failed first fetch and is not kept as "good".
    pub fn seed(&mut self, k: Key, set: &EmoteSet, now: Instant) {
        if set.is_empty() {
            return;
        }
        let due = self.next_due(now);
        let e = self.entries.entry(k).or_insert(Entry {
            set: None,
            etag: None,
            due,
            inflight: false,
            stale_said: false,
            manual: false,
            fresh: None,
        });
        e.set = Some(set.clone());
        e.etag = None;
        e.fresh = Some(now);
    }

    /// the socket state: a reconnect after a drop refreshes everything open.
    pub fn socket(&mut self, up: bool, now: Instant) {
        if !up {
            self.was_down = true;
        } else if self.was_down {
            self.was_down = false;
            self.force_all(now, true);
        }
    }

    /// make every open room due. `spread` staggers them (a reconnect); the hand
    /// refresh passes false.
    pub fn force_all(&mut self, now: Instant, spread: bool) {
        let keys: Vec<Key> = self.entries.keys().cloned().collect();
        for k in keys {
            let s = if spread {
                self.spread(RECONNECT_SPREAD)
            } else {
                Duration::ZERO
            };
            if let Some(e) = self.entries.get_mut(&k) {
                // a flapping socket must not turn into a request storm
                if spread && e.fresh.is_some_and(|t| now.duration_since(t) < FLAP_GAP) {
                    continue;
                }
                e.due = now + s;
            }
        }
    }

    /// `/emotes refresh`: these rooms, now, and tell me how it went. returns how
    /// many rooms will run — none means there is nothing to wait for.
    pub fn force(&mut self, keys: &[Key], now: Instant) -> usize {
        let mut armed = 0;
        for k in keys {
            if let Some(e) = self.entries.get_mut(k) {
                e.due = now;
                e.manual = true;
                armed += 1;
            }
        }
        armed
    }

    /// what to fetch now, given the rooms open right now. a room that is no
    /// longer open is forgotten (memory and requests both stop); a new one is
    /// registered with its first refresh a full period away. each returned key
    /// carries the validator to send, and is marked in flight.
    pub fn wanted(&mut self, open: &[Key], now: Instant) -> Vec<(Key, Option<String>)> {
        self.register(open, now);
        let mut busy = self.entries.values().filter(|e| e.inflight).count();
        let mut out = Vec::new();
        for k in open {
            if busy >= MAX_INFLIGHT {
                break;
            }
            if let Some(e) = self.entries.get_mut(k) {
                // a room whose first fetch failed has no set and no validator: it is
                // fetched like any other, and a good answer becomes its set.
                if !e.inflight && now >= e.due {
                    e.inflight = true;
                    busy += 1;
                    out.push((k.clone(), e.etag.clone()));
                }
            }
        }
        out
    }

    /// forget rooms that closed, note rooms that opened.
    pub fn register(&mut self, open: &[Key], now: Instant) {
        let open_set: HashSet<&Key> = open.iter().collect();
        self.entries.retain(|k, _| open_set.contains(k));
        for k in open {
            if !self.entries.contains_key(k) {
                let due = self.next_due(now);
                self.entries.insert(
                    k.clone(),
                    Entry {
                        set: None,
                        etag: None,
                        due,
                        inflight: false,
                        stale_said: false,
                        manual: false,
                        fresh: None,
                    },
                );
            }
        }
    }

    /// run one fetch on its own thread; the answer comes back through [`drain`].
    pub fn spawn(
        &self,
        k: Key,
        etag: Option<String>,
        fetch: impl FnOnce(&Key, Option<&str>) -> Outcome + Send + 'static,
    ) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let o = fetch(&k, etag.as_deref());
            let _ = tx.send((k, o));
        });
    }

    /// apply every finished fetch. a result for a room that closed meanwhile is
    /// dropped.
    pub fn drain(&mut self, now: Instant) -> Vec<Applied> {
        let mut done = Vec::new();
        while let Ok((k, o)) = self.rx.try_recv() {
            if let Some(a) = self.apply(k, o, now) {
                done.push(a);
            }
        }
        done
    }

    pub fn apply(&mut self, k: Key, o: Outcome, now: Instant) -> Option<Applied> {
        let next = self.next_due(now);
        let retry = now + RETRY + self.spread(RETRY / 3);
        let e = self.entries.get_mut(&k)?;
        e.inflight = false;
        let manual = std::mem::take(&mut e.manual);
        let (changed, failed, say_stale) = match o {
            Outcome::Changed(set, etag) if !set.is_empty() => {
                e.set = Some(set);
                e.fresh = Some(now);
                e.etag = etag;
                e.due = next;
                e.stale_said = false;
                (true, false, false)
            }
            Outcome::NotModified => {
                e.fresh = Some(now);
                e.due = next;
                e.stale_said = false;
                (false, false, false)
            }
            // an empty body is as unusable as no body: keep what we have.
            Outcome::Changed(..) | Outcome::Failed => {
                e.due = retry;
                let say = !e.stale_said;
                e.stale_said = true;
                (false, true, say)
            }
        };
        Some(Applied {
            key: k,
            changed,
            say_stale,
            manual,
            failed,
        })
    }
}

/// one tab's set from its sources' latest sets, then the globals. built whole,
/// so swapping it in replaces the old one in a single step.
pub fn rebuild(subs: &[Key], st: &State, globals: &EmoteSet) -> EmoteSet {
    let mut out = EmoteSet::new();
    for k in subs {
        if let Some(s) = st.set(k) {
            out.merge(s.clone());
        }
    }
    out.merge(globals.clone());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use heatsync_core::emote::Emote;

    fn emote(name: &str, id: &str) -> Emote {
        Emote {
            name: name.into(),
            url: format!("https://cdn.7tv.app/emote/{id}/2x.webp"),
            provider: "7tv".into(),
            id: id.into(),
            animated: false,
            zero_width: false,
            global: false,
            kind: Default::default(),
        }
    }

    fn set(names: &[(&str, &str)]) -> EmoteSet {
        EmoteSet::from_list(names.iter().map(|(n, i)| emote(n, i)))
    }

    fn k(n: &str) -> Key {
        key(Platform::Twitch, n)
    }

    fn seeded(n: &str, s: &EmoteSet) -> (State, Instant) {
        let mut st = State::new();
        let t0 = Instant::now();
        st.wanted(&[k(n)], t0);
        st.seed(k(n), s, t0);
        (st, t0)
    }

    #[test]
    fn a_failed_refresh_keeps_the_last_good_set_and_says_stale_once() {
        let (mut st, t0) = seeded("xqc", &set(&[("Old", "1")]));
        let a = st.apply(k("xqc"), Outcome::Failed, t0).unwrap();
        assert!(a.failed && a.say_stale && !a.changed);
        assert!(st.set(&k("xqc")).unwrap().get("Old").is_some());
        // the retry fails too: no second notice
        let b = st.apply(k("xqc"), Outcome::Failed, t0).unwrap();
        assert!(b.failed && !b.say_stale);
        assert!(st.set(&k("xqc")).unwrap().get("Old").is_some());
        // an empty answer is as bad as none
        let c = st
            .apply(k("xqc"), Outcome::Changed(EmoteSet::new(), None), t0)
            .unwrap();
        assert!(c.failed && !c.changed);
        assert!(st.set(&k("xqc")).unwrap().get("Old").is_some());
        // a good answer clears the outage; the next failure speaks again
        st.apply(k("xqc"), Outcome::NotModified, t0).unwrap();
        assert!(st.apply(k("xqc"), Outcome::Failed, t0).unwrap().say_stale);
    }

    #[test]
    fn a_renamed_emote_stops_drawing_under_its_old_name() {
        let (mut st, t0) = seeded("xqc", &set(&[("OldName", "1"), ("Keep", "2")]));
        let globals = EmoteSet::new();
        assert!(rebuild(&[k("xqc")], &st, &globals).get("OldName").is_some());
        let fresh = set(&[("NewName", "1"), ("Keep", "2")]);
        let a = st
            .apply(
                k("xqc"),
                Outcome::Changed(fresh, Some("W/\"v2\"".into())),
                t0,
            )
            .unwrap();
        assert!(a.changed);
        let after = rebuild(&[k("xqc")], &st, &globals);
        assert!(after.get("OldName").is_none());
        assert_eq!(after.get("NewName").unwrap().id, "1");
        assert!(after.get("Keep").is_some());
    }

    #[test]
    fn the_validator_is_sent_back_and_not_modified_changes_nothing() {
        let (mut st, t0) = seeded("xqc", &set(&[("A", "1")]));
        st.force(&[k("xqc")], t0);
        let w = st.wanted(&[k("xqc")], t0);
        assert_eq!(w, vec![(k("xqc"), None)]);
        st.apply(
            k("xqc"),
            Outcome::Changed(set(&[("A", "1")]), Some("W/\"x\"".into())),
            t0,
        );
        st.force(&[k("xqc")], t0);
        assert_eq!(
            st.wanted(&[k("xqc")], t0),
            vec![(k("xqc"), Some("W/\"x\"".into()))]
        );
        let a = st.apply(k("xqc"), Outcome::NotModified, t0).unwrap();
        assert!(!a.changed && !a.failed && !a.say_stale);
        assert!(st.set(&k("xqc")).unwrap().get("A").is_some());
    }

    #[test]
    fn nothing_is_fetched_or_kept_while_no_room_is_open() {
        let (mut st, t0) = seeded("xqc", &set(&[("A", "1")]));
        st.force_all(t0, false);
        // the tab closed: no request, and the entry is gone
        assert!(st.wanted(&[], t0 + PERIOD * 2).is_empty());
        assert!(st.set(&k("xqc")).is_none());
        // a late answer for a closed room is dropped
        assert!(st
            .apply(k("xqc"), Outcome::Changed(set(&[("B", "2")]), None), t0)
            .is_none());
    }

    #[test]
    fn a_room_is_not_fetched_before_its_period_and_never_twice_at_once() {
        let (mut st, t0) = seeded("xqc", &set(&[("A", "1")]));
        assert!(st
            .wanted(&[k("xqc")], t0 + Duration::from_secs(10))
            .is_empty());
        let later = t0 + PERIOD + JITTER + Duration::from_secs(1);
        assert_eq!(st.wanted(&[k("xqc")], later).len(), 1);
        // in flight: asking again fetches nothing
        assert!(st.wanted(&[k("xqc")], later).is_empty());
    }

    #[test]
    fn the_timer_is_jittered_within_bounds() {
        let mut st = State::new();
        let t0 = Instant::now();
        let mut seen = HashSet::new();
        for _ in 0..200 {
            let d = st.next_due(t0) - t0;
            assert!(d >= PERIOD - JITTER && d < PERIOD + JITTER, "{d:?}");
            seen.insert(d.as_millis());
        }
        assert!(seen.len() > 100, "not spread: {}", seen.len());
    }

    #[test]
    fn a_reconnect_refreshes_everything_open_once_spread_out() {
        let (mut st, t0) = seeded("xqc", &set(&[("A", "1")]));
        // first connect, and a connect that never followed a drop: nothing
        let t1 = t0 + FLAP_GAP * 2;
        st.socket(true, t1);
        assert!(st.wanted(&[k("xqc")], t1).is_empty());
        st.socket(false, t1);
        st.socket(true, t1);
        assert!(st.wanted(&[k("xqc")], t1 + RECONNECT_SPREAD).len() == 1);
        // a second up without a drop in between does it again? no
        st.apply(k("xqc"), Outcome::NotModified, t1);
        st.socket(true, t1);
        assert!(st.wanted(&[k("xqc")], t1 + RECONNECT_SPREAD).is_empty());
    }

    #[test]
    fn a_room_whose_first_fetch_failed_is_fetched_on_the_timer_and_on_force() {
        let mut st = State::new();
        let t0 = Instant::now();
        st.wanted(&[k("new")], t0);
        // the failed first fetch seeded nothing
        st.seed(k("new"), &EmoteSet::new(), t0);
        assert!(st.set(&k("new")).is_none());
        // the timer picks it up, with no validator to send
        let later = t0 + PERIOD + JITTER + Duration::from_secs(1);
        assert_eq!(st.wanted(&[k("new")], later), vec![(k("new"), None)]);
        // and a good answer becomes its set
        let a = st
            .apply(k("new"), Outcome::Changed(set(&[("A", "1")]), None), later)
            .unwrap();
        assert!(a.changed);
        assert!(st.set(&k("new")).unwrap().get("A").is_some());
        // a hand refresh of a room with no set runs too
        let mut st = State::new();
        st.wanted(&[k("new")], t0);
        assert_eq!(st.force(&[k("new")], t0), 1);
        assert_eq!(st.wanted(&[k("new")], t0).len(), 1);
    }

    #[test]
    fn a_hand_refresh_with_no_room_to_run_says_so_instead_of_waiting() {
        let mut st = State::new();
        assert_eq!(st.force(&[k("nowhere")], Instant::now()), 0);
    }

    #[test]
    fn a_socket_that_flaps_does_not_refetch_a_room_read_a_moment_ago() {
        let (mut st, t0) = seeded("xqc", &set(&[("A", "1")]));
        for _ in 0..3 {
            st.socket(false, t0);
            st.socket(true, t0 + Duration::from_secs(5));
        }
        assert!(st
            .wanted(&[k("xqc")], t0 + Duration::from_secs(5) + RECONNECT_SPREAD)
            .is_empty());
    }

    #[test]
    fn at_most_a_few_fetches_run_at_once() {
        let mut st = State::new();
        let t0 = Instant::now();
        let keys: Vec<Key> = (0..10).map(|i| k(&format!("c{i}"))).collect();
        st.wanted(&keys, t0);
        for key in &keys {
            st.seed(key.clone(), &set(&[("A", "1")]), t0);
        }
        st.force_all(t0, false);
        assert_eq!(st.wanted(&keys, t0).len(), MAX_INFLIGHT);
    }

    #[test]
    fn a_merged_tab_pools_its_sources_and_channel_beats_global() {
        let mut st = State::new();
        let t0 = Instant::now();
        let (a, b) = (k("a"), key(Platform::Kick, "b"));
        st.wanted(&[a.clone(), b.clone()], t0);
        st.seed(a.clone(), &set(&[("OnlyA", "1"), ("Both", "2")]), t0);
        st.seed(b.clone(), &set(&[("OnlyB", "3"), ("Both", "4")]), t0);
        let mut g = set(&[("Both", "9"), ("Glob", "8")]);
        g.mark_global();
        let out = rebuild(&[a, b], &st, &g);
        assert_eq!(out.get("Both").unwrap().id, "2");
        for n in ["OnlyA", "OnlyB", "Glob"] {
            assert!(out.get(n).is_some(), "{n}");
        }
    }

    #[test]
    fn a_spawned_fetch_lands_through_drain_with_no_network() {
        let (mut st, t0) = seeded("xqc", &set(&[("A", "1")]));
        st.force(&[k("xqc")], t0);
        let w = st.wanted(&[k("xqc")], t0);
        let (key, etag) = w.into_iter().next().unwrap();
        st.spawn(key, etag, |_, _| Outcome::Changed(set(&[("B", "2")]), None));
        let mut got = Vec::new();
        for _ in 0..200 {
            got = st.drain(t0);
            if !got.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(got.len(), 1);
        assert!(got[0].changed && got[0].manual);
        assert!(st.set(&k("xqc")).unwrap().get("B").is_some());
    }

    #[test]
    fn look_fires_at_most_once_a_second() {
        let mut st = State::new();
        let t0 = Instant::now();
        assert!(st.look(t0));
        assert!(!st.look(t0 + Duration::from_millis(500)));
        assert!(st.look(t0 + Duration::from_millis(1100)));
    }

    #[test]
    fn keys_ignore_case() {
        assert_eq!(key(Platform::Twitch, "XQC"), key(Platform::Twitch, "xqc"));
    }
}
