//! name paints + heatsync profile colors, resolved off-thread and cached.
//! `want` is called once per incoming line (cheap, deduped); `pump` runs on the
//! tick, merges finished batches and starts the next one. drawing only ever
//! reads the cache — it never waits on the network.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use heatsync_core::paint::Paint;
use serde_json::Value;

use crate::http;

/// resolved entries kept; the oldest go first.
const CACHE_CAP: usize = 4096;
/// live name colors kept by username.
const NAMES_CAP: usize = 1024;
/// ids waiting for a batch — a raid's worth of new chatters is bounded.
const PENDING_CAP: usize = 512;
/// the server's per-request id limit.
const BATCH: usize = 50;
/// a failed batch waits this long before its ids go again.
const RETRY: Duration = Duration::from_secs(30);
/// minimum spacing of batches, so a busy channel is a trickle, not a stream.
const SPACING: Duration = Duration::from_millis(500);

/// what we know about one chatter. `None`/`None` is a cached "nothing set".
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct UserLook {
    pub(crate) paint: Option<Paint>,
    /// the heatsync-picked `#rrggbb` name color.
    pub(crate) color: Option<String>,
}

type Batch = (Vec<String>, Option<HashMap<String, UserLook>>);

pub(crate) struct Paints {
    known: HashMap<String, UserLook>,
    /// insertion order of `known`, for eviction.
    order: VecDeque<String>,
    /// lowercase username → live `profile:color`, for lines with no sender id
    /// (kick/youtube frames may omit it). newest wins.
    names: HashMap<String, String>,
    names_order: VecDeque<String>,
    pending: Vec<String>,
    queued: HashSet<String>,
    inflight: bool,
    next_at: Instant,
    tx: Sender<Batch>,
    rx: Receiver<Batch>,
}

impl Paints {
    pub(crate) fn new() -> Paints {
        let (tx, rx) = channel();
        Paints {
            known: HashMap::new(),
            order: VecDeque::new(),
            names: HashMap::new(),
            names_order: VecDeque::new(),
            pending: Vec::new(),
            queued: HashSet::new(),
            inflight: false,
            next_at: Instant::now(),
            tx,
            rx,
        }
    }

    #[cfg(test)]
    pub(crate) fn get(&self, id: &str) -> Option<&UserLook> {
        self.known.get(id)
    }

    /// ask for an id unless it's cached, queued, or the queue is full.
    pub(crate) fn want(&mut self, id: &str) {
        if self.known.contains_key(id)
            || self.queued.contains(id)
            || self.pending.len() >= PENDING_CAP
        {
            return;
        }
        self.queued.insert(id.to_string());
        self.pending.push(id.to_string());
    }

    /// what to draw a line's name with: the cached look for its sender id,
    /// else (only when chat sent no color) a live color matched by username.
    pub(crate) fn look_for(
        &self,
        uid: Option<&str>,
        user: &str,
        has_color: bool,
    ) -> Option<Cow<'_, UserLook>> {
        if let Some(l) = uid.and_then(|u| self.known.get(u)) {
            return Some(Cow::Borrowed(l));
        }
        if has_color || self.names.is_empty() {
            return None;
        }
        let color = self.names.get(&user.to_lowercase())?;
        Some(Cow::Owned(UserLook {
            paint: None,
            color: Some(color.clone()),
        }))
    }

    /// a live `profile:color`: the color for that id (a cached paint stays)
    /// and for each username it names.
    pub(crate) fn set_color(&mut self, id: Option<&str>, names: &[String], color: &str) {
        if let Some(id) = id {
            if let Some(l) = self.known.get_mut(id) {
                l.color = Some(color.to_string());
            } else {
                self.store(
                    id.to_string(),
                    UserLook {
                        paint: None,
                        color: Some(color.to_string()),
                    },
                );
            }
        }
        for n in names {
            if self.names.insert(n.clone(), color.to_string()).is_none() {
                self.names_order.push_back(n.clone());
            }
        }
        while self.names.len() > NAMES_CAP {
            match self.names_order.pop_front() {
                Some(old) => {
                    self.names.remove(&old);
                }
                None => break,
            }
        }
    }

    /// `cosmetic:changed`: refetch only ids we already resolved — the push
    /// goes to everyone, so unseen ids aren't worth a request.
    pub(crate) fn invalidate(&mut self, ids: &[String]) {
        for id in ids {
            if self.known.remove(id).is_some() {
                self.want(id);
            }
        }
    }

    /// merge finished batches; start the next one when due.
    pub(crate) fn pump(&mut self) {
        while let Ok((ids, got)) = self.rx.try_recv() {
            self.inflight = false;
            match got {
                Some(mut got) => {
                    for id in ids {
                        let mut look = got.remove(&id).unwrap_or_default();
                        // a live color that landed mid-fetch is newer
                        if let Some(old) = self.known.get(&id) {
                            look.color = old.color.clone().or(look.color);
                        }
                        self.queued.remove(&id);
                        self.store(id, look);
                    }
                }
                None => {
                    // network trouble: back off, ids stay queued
                    self.next_at = Instant::now() + RETRY;
                    self.pending.extend(ids);
                }
            }
        }
        if self.inflight || self.pending.is_empty() || Instant::now() < self.next_at {
            return;
        }
        // newest chatters first: they're at the bottom of the screen
        let from = self.pending.len().saturating_sub(BATCH);
        let ids: Vec<String> = self.pending.drain(from..).collect();
        self.inflight = true;
        self.next_at = Instant::now() + SPACING;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let got = fetch(&ids);
            let _ = tx.send((ids, got));
        });
    }

    fn store(&mut self, id: String, look: UserLook) {
        if self.known.insert(id.clone(), look).is_none() {
            self.order.push_back(id);
        }
        while self.known.len() > CACHE_CAP {
            match self.order.pop_front() {
                Some(old) => {
                    self.known.remove(&old);
                }
                None => break,
            }
        }
    }
}

/// both lookups for one batch; either failing fails the batch (it retries).
fn fetch(ids: &[String]) -> Option<HashMap<String, UserLook>> {
    let hs = http::paints(ids)?;
    // 7tv only knows twitch ids and `kick_<id>`
    let stv_ids: Vec<String> = ids.iter().filter(|i| stv_known(i)).cloned().collect();
    let stv = if stv_ids.is_empty() {
        Value::Null
    } else {
        http::cosmetics(&stv_ids)?
    };
    Some(resolve(ids, &hs, &stv))
}

fn stv_known(id: &str) -> bool {
    let n = id.strip_prefix("kick_").unwrap_or(id);
    !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
}

/// `/api/paints` + `/api/cosmetics/batch` bodies → one look per id. a
/// heatsync paint beats a 7tv one.
fn resolve(ids: &[String], hs: &Value, stv: &Value) -> HashMap<String, UserLook> {
    ids.iter()
        .map(|id| {
            let paint = hs
                .get("paints")
                .and_then(|p| p.get(id))
                .and_then(Paint::from_heatsync)
                .or_else(|| {
                    stv.get("cosmetics")
                        .and_then(|c| c.get(id))
                        .and_then(|c| c.get("paint"))
                        .and_then(Paint::from_7tv)
                });
            let color = hs
                .get("colors")
                .and_then(|c| c.get(id))
                .and_then(Value::as_str)
                .filter(|c| {
                    c.len() == 7
                        && c.starts_with('#')
                        && c[1..].bytes().all(|b| b.is_ascii_hexdigit())
                })
                .map(str::to_string);
            (id.clone(), UserLook { paint, color })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn heatsync_paint_beats_7tv_and_colors_ride_along() {
        let hs = json!({"paints": {"1": {"base": {"type": "solid", "stops": [{"color": "#010203", "pos": 0}]}}},
            "colors": {"1": "#aabbcc", "2": "nope"}});
        let stv = json!({"cosmetics": {
            "1": {"paint": {"function": "SOLID", "color": 0xff0000ffu32 as i32}},
            "2": {"paint": {"function": "SOLID", "color": 0x00ff00ffu32 as i32}}}});
        let got = resolve(&ids(&["1", "2", "3"]), &hs, &stv);
        assert_eq!(got["1"].paint.as_ref().unwrap().sample(0.0), [1, 2, 3]);
        assert_eq!(got["1"].color.as_deref(), Some("#aabbcc"));
        assert_eq!(got["2"].paint.as_ref().unwrap().sample(0.0), [0, 255, 0]);
        assert_eq!(got["2"].color, None, "malformed color dropped");
        assert_eq!(got["3"], UserLook::default());
    }

    #[test]
    fn want_dedupes_and_skips_known() {
        let mut p = Paints::new();
        p.want("1");
        p.want("1");
        assert_eq!(p.pending.len(), 1);
        p.store("2".into(), UserLook::default());
        p.want("2");
        assert_eq!(p.pending.len(), 1);
    }

    #[test]
    fn cache_is_capped_oldest_first() {
        let mut p = Paints::new();
        for i in 0..CACHE_CAP + 10 {
            p.store(i.to_string(), UserLook::default());
        }
        assert_eq!(p.known.len(), CACHE_CAP);
        assert!(p.get("0").is_none() && p.get(&(CACHE_CAP + 9).to_string()).is_some());
    }

    #[test]
    fn invalidate_requeues_only_resolved_ids() {
        let mut p = Paints::new();
        p.store("1".into(), UserLook::default());
        p.invalidate(&ids(&["1", "9"]));
        assert_eq!(p.pending, ids(&["1"]));
        assert!(p.get("1").is_none());
    }

    #[test]
    fn live_color_keeps_the_cached_paint() {
        let mut p = Paints::new();
        let paint = Paint::from_7tv(&json!({"function": "SOLID", "color": 0xff0000ffu32 as i32}));
        p.store(
            "1".into(),
            UserLook {
                paint: paint.clone(),
                color: None,
            },
        );
        p.set_color(Some("1"), &[], "#112233");
        assert_eq!(p.get("1").unwrap().paint, paint);
        assert_eq!(p.get("1").unwrap().color.as_deref(), Some("#112233"));
    }

    #[test]
    fn live_color_matches_by_lowercase_username_when_chat_sent_none() {
        let mut p = Paints::new();
        p.set_color(None, &["kicker".into()], "#445566");
        let c = |l: Option<Cow<UserLook>>| l.and_then(|l| l.color.clone());
        assert_eq!(
            c(p.look_for(None, "KicKer", false)).as_deref(),
            Some("#445566")
        );
        assert_eq!(c(p.look_for(None, "KicKer", true)), None, "chat color wins");
        assert_eq!(c(p.look_for(None, "other", false)), None);
        for i in 0..NAMES_CAP + 5 {
            p.set_color(None, &[format!("n{i}")], "#000000");
        }
        assert_eq!(p.names.len(), NAMES_CAP);
    }

    #[test]
    fn stv_only_knows_twitch_and_kick() {
        assert!(stv_known("123") && stv_known("kick_9"));
        assert!(!stv_known("yt_UCabc") && !stv_known("kick_") && !stv_known(""));
    }
}
