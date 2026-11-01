//! followed-channel stream events (went live / offline / switched game) kept
//! for the mentions tab. memory only — the server replays its last day on every
//! connect, so nothing here is written to disk.

use std::collections::{HashMap, VecDeque};

use heatsync_core::proto::ChatLine;
use heatsync_core::{NoteKind, Platform};

use crate::mentions::Mention;

/// rows kept (the server's own history cap).
pub const CAP: usize = 200;
/// the same event reported twice (live push + the history replay) lands within this.
const DUP_SECS: i64 = 60;

#[derive(Clone, Debug, PartialEq)]
pub struct FollowRow {
    pub platform: Platform,
    pub channel: String,
    pub color: Option<String>,
    pub kind: NoteKind,
    pub what: String,
    pub title: String,
    /// unix seconds it happened.
    pub at: i64,
}

impl FollowRow {
    /// a parsed follow stream line at `at` unix seconds; None for a line that isn't a note.
    pub fn of(l: &ChatLine, at: i64) -> Option<FollowRow> {
        let n = l.note.as_ref()?;
        Some(FollowRow {
            platform: l.platform,
            channel: l.channel.clone(),
            color: l.color.clone(),
            kind: n.kind,
            what: n.what.clone(),
            title: l.content.clone(),
            at,
        })
    }

    fn same(&self, o: &FollowRow) -> bool {
        self.platform == o.platform
            && self.channel.eq_ignore_ascii_case(&o.channel)
            && self.what == o.what
            && (self.at - o.at).abs() <= DUP_SECS
    }
}

/// one entry of the mentions tab's timeline.
pub enum Item<'a> {
    Mention(&'a Mention),
    Follow(&'a FollowRow),
}

#[derive(Default)]
pub struct FollowLog {
    rows: VecDeque<FollowRow>,
    /// channel login (lowercase) → `#rrggbb`, from `follow:colors`.
    colors: HashMap<String, String>,
}

impl FollowLog {
    /// keep a row in time order. false = it is a repeat (nothing changed).
    pub fn add(&mut self, row: FollowRow) -> bool {
        if self.rows.iter().any(|r| r.same(&row)) {
            return false;
        }
        let at = self.rows.partition_point(|r| r.at <= row.at);
        self.rows.insert(at, row);
        if self.rows.len() > CAP {
            self.rows.pop_front();
        }
        true
    }

    /// fold in a colour map. true = something changed.
    pub fn set_colors(&mut self, colors: Vec<(String, String)>) -> bool {
        let mut changed = false;
        for (k, v) in colors {
            changed |= self.colors.insert(k, v.clone()) != Some(v);
        }
        changed
    }

    /// the row's own colour, else the channel's from the map.
    pub fn color_of(&self, r: &FollowRow) -> Option<String> {
        r.color
            .clone()
            .or_else(|| self.colors.get(&r.channel.to_lowercase()).cloned())
    }

    /// mentions and follow rows in time order. a mention with no time (0, saved
    /// before times were kept) stays at the front; on a tie a mention goes first.
    pub fn timeline<'a>(&'a self, mentions: impl Iterator<Item = &'a Mention>) -> Vec<Item<'a>> {
        let mut out = Vec::new();
        let mut f = self.rows.iter().peekable();
        for m in mentions {
            while let Some(r) = f.next_if(|r| r.at < m.at) {
                out.push(Item::Follow(r));
            }
            out.push(Item::Mention(m));
        }
        out.extend(f.map(Item::Follow));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(chan: &str, what: &str, at: i64) -> FollowRow {
        FollowRow {
            platform: Platform::Twitch,
            channel: chan.into(),
            color: None,
            kind: NoteKind::Live,
            what: what.into(),
            title: String::new(),
            at,
        }
    }

    fn ment(at: i64) -> Mention {
        Mention {
            platform: Platform::Twitch,
            channel: "c".into(),
            user: "u".into(),
            color: None,
            text: "hey".into(),
            id: None,
            at,
        }
    }

    fn shape(l: &FollowLog, ms: &[Mention]) -> Vec<String> {
        l.timeline(ms.iter())
            .iter()
            .map(|i| match i {
                Item::Mention(m) => format!("m{}", m.at),
                Item::Follow(r) => format!("f{}", r.at),
            })
            .collect()
    }

    #[test]
    fn timeline_interleaves_by_time_and_old_mentions_lead() {
        let mut l = FollowLog::default();
        for (c, at) in [("a", 300), ("b", 100), ("c", 200)] {
            assert!(l.add(row(c, "went live", at)));
        }
        let ms = [ment(0), ment(150), ment(250), ment(400)];
        assert_eq!(
            shape(&l, &ms),
            ["m0", "f100", "m150", "f200", "m250", "f300", "m400"]
        );
    }

    #[test]
    fn a_replayed_event_is_not_added_twice() {
        let mut l = FollowLog::default();
        assert!(l.add(row("Cyr", "went live — x", 1_000)));
        // the history copy of the same go-live, a few seconds off
        assert!(!l.add(row("cyr", "went live — x", 1_004)));
        // a later go-live with the same words is a new event
        assert!(l.add(row("cyr", "went live — x", 5_000)));
        // a different channel / wording is its own row
        assert!(l.add(row("cyr", "went offline", 1_004)));
        assert!(l.add(row("other", "went live — x", 1_004)));
        assert_eq!(l.rows.len(), 4);
    }

    #[test]
    fn rows_keep_the_cap_and_colors_fall_back_to_the_map() {
        let mut l = FollowLog::default();
        for i in 0..CAP as i64 + 5 {
            l.add(row("c", &format!("went live — {i}"), i * 1_000));
        }
        assert_eq!(l.rows.len(), CAP);
        assert_eq!(l.rows.front().unwrap().at, 5_000);
        let r = row("Cyr", "went live", 1);
        assert_eq!(l.color_of(&r), None);
        assert!(l.set_colors(vec![("cyr".into(), "#ff0000".into())]));
        assert!(!l.set_colors(vec![("cyr".into(), "#ff0000".into())]));
        assert_eq!(l.color_of(&r).as_deref(), Some("#ff0000"));
        let own = FollowRow {
            color: Some("#00ff00".into()),
            ..r
        };
        assert_eq!(l.color_of(&own).as_deref(), Some("#00ff00"));
    }
}
