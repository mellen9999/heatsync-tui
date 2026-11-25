//! youtube tabs are named by video id, but the room's emote route is keyed by
//! the stream's channel id. this learns that id once per video from the site's
//! resolve route and holds it. nothing here ever guesses: no answer means no
//! id, and the tab reads globals + the sender's shipped picks until one lands.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

/// a video with no answer yet (not live, an older server, offline) is asked
/// again this often — quiet, never a status line.
pub const RETRY: Duration = Duration::from_secs(300);
/// lookups in flight at once (the route is rate limited).
const MAX_INFLIGHT: usize = 2;

/// a youtube channel id: `UC` + 22 of `[A-Za-z0-9_-]`. case matters.
pub fn valid(id: &str) -> bool {
    id.len() == 24
        && id.starts_with("UC")
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

pub struct YtIds {
    ids: HashMap<String, String>,
    busy: HashSet<String>,
    due: HashMap<String, Instant>,
    tx: Sender<(String, Option<String>)>,
    rx: Receiver<(String, Option<String>)>,
}

impl YtIds {
    pub fn new() -> YtIds {
        let (tx, rx) = channel();
        YtIds {
            ids: HashMap::new(),
            busy: HashSet::new(),
            due: HashMap::new(),
            tx,
            rx,
        }
    }

    /// the stream's channel id for this video, when known.
    pub fn id(&self, video: &str) -> Option<&str> {
        self.ids.get(video).map(String::as_str)
    }

    /// teach an answer directly (tests, and anything that already knows).
    pub fn learn(&mut self, video: &str, id: &str) -> bool {
        valid(id)
            && self
                .ids
                .insert(video.to_string(), id.to_string())
                .as_deref()
                != Some(id)
    }

    /// the open videos to look up now; forgets videos that closed. each one
    /// returned is marked in flight.
    pub fn wanted(&mut self, open: &[String], now: Instant) -> Vec<String> {
        let keep: HashSet<&String> = open.iter().collect();
        self.ids.retain(|v, _| keep.contains(v));
        self.due.retain(|v, _| keep.contains(v));
        let mut out = Vec::new();
        for v in open {
            if self.busy.len() >= MAX_INFLIGHT {
                break;
            }
            if self.ids.contains_key(v) || self.busy.contains(v) || out.contains(v) {
                continue;
            }
            if self.due.get(v).is_some_and(|t| now < *t) {
                continue;
            }
            self.busy.insert(v.clone());
            out.push(v.clone());
        }
        out
    }

    pub fn spawn(&self, video: String, ask: impl FnOnce(&str) -> Option<String> + Send + 'static) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let got = ask(&video);
            let _ = tx.send((video, got));
        });
    }

    /// apply finished lookups; the videos that gained an id.
    pub fn drain(&mut self, open: &[String], now: Instant) -> Vec<String> {
        let mut learned = Vec::new();
        while let Ok((video, got)) = self.rx.try_recv() {
            self.busy.remove(&video);
            if !open.contains(&video) {
                continue;
            }
            match got {
                Some(id) if self.learn(&video, &id) => learned.push(video),
                Some(_) => {}
                None => {
                    self.due.insert(video, now + RETRY);
                }
            }
        }
        learned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UC: &str = "UCaB3_-xyzABC01234567890";

    #[test]
    fn only_a_real_channel_id_is_kept() {
        assert!(valid(UC));
        for bad in [
            "",
            "dQw4w9WgXcQ",
            "@handle",
            "uCaB3_-xyzABC01234567890",
            "UCshort",
            "UCaB3_-xyzABC012345678901",
            "UCaB3_-xyzABC0123456789/",
        ] {
            assert!(!valid(bad), "{bad}");
        }
        let mut y = YtIds::new();
        assert!(!y.learn("vid", "dQw4w9WgXcQ"));
        assert_eq!(y.id("vid"), None);
    }

    #[test]
    fn the_id_keeps_its_case() {
        let mut y = YtIds::new();
        assert!(y.learn("vid", UC));
        assert_eq!(y.id("vid"), Some(UC));
        assert!(!y.learn("vid", UC), "the same answer is not news");
    }

    #[test]
    fn asks_once_then_not_before_the_retry_and_never_for_a_closed_video() {
        let mut y = YtIds::new();
        let t = Instant::now();
        let open = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(y.wanted(&open, t), vec!["a", "b"], "capped in flight");
        assert!(
            y.wanted(&open, t).is_empty(),
            "busy ones are not asked twice"
        );
        y.spawn("a".into(), |_| None);
        y.spawn("b".into(), |_| Some(UC.into()));
        let mut got = Vec::new();
        for _ in 0..200 {
            got.extend(y.drain(&open, t));
            if y.busy.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(got, vec!["b"]);
        assert_eq!(y.id("b"), Some(UC));
        // a had no answer: quiet until the retry; c goes now
        assert_eq!(y.wanted(&open, t), vec!["c"]);
        assert!(y.wanted(&open, t + RETRY / 2).is_empty());
        assert!(y
            .wanted(&["a".to_string()], t + RETRY)
            .contains(&"a".to_string()));
        // a tab that closed forgets its id
        y.wanted(&["a".to_string()], t + RETRY);
        assert_eq!(y.id("b"), None);
    }
}
