//! room-mode changes → system notes. twitch re-sends ROOMSTATE on join and on
//! every mode change; the first state per channel is only remembered, later
//! ones say what changed (`slow mode 30s`, `emote-only on`, …).

use std::collections::HashMap;

use crate::proto::{fmt_secs, note_line, ChatLine, RoomState};
use crate::{NoteKind, Platform};

/// the full set of modes, built up from partial ROOMSTATEs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Modes {
    slow: u32,
    followers: i32,
    emote_only: bool,
    subs_only: bool,
}

impl Modes {
    /// the modes after `s`: tags it carried replace ours, absent ones stay.
    fn with(self, s: RoomState) -> Modes {
        Modes {
            slow: s.slow.unwrap_or(self.slow),
            followers: s.followers.unwrap_or(self.followers),
            emote_only: s.emote_only.unwrap_or(self.emote_only),
            subs_only: s.subs_only.unwrap_or(self.subs_only),
        }
    }
}

/// twitch's default when we have not seen a mode yet: everything off.
const OFF: Modes = Modes {
    slow: 0,
    followers: -1,
    emote_only: false,
    subs_only: false,
};

#[derive(Default)]
pub struct RoomModes {
    seen: HashMap<String, Modes>,
}

impl RoomModes {
    pub fn new() -> Self {
        Self::default()
    }

    /// fold in a channel's new state; returns a note line per change. the first
    /// state per channel is only remembered.
    pub fn update(&mut self, channel: &str, now: RoomState) -> Vec<ChatLine> {
        let key = channel.to_lowercase();
        let Some(was) = self.seen.get(&key).copied() else {
            self.seen.insert(key, OFF.with(now));
            return Vec::new();
        };
        let next = was.with(now);
        self.seen.insert(key, next);
        changes(was, next)
            .into_iter()
            .filter_map(|what| {
                note_line(
                    Platform::Twitch,
                    channel.to_string(),
                    String::new(),
                    None,
                    NoteKind::Mode,
                    what,
                    String::new(),
                )
            })
            .collect()
    }
}

fn on_off(label: &str, on: bool) -> String {
    format!("{label} {}", if on { "on" } else { "off" })
}

fn changes(was: Modes, now: Modes) -> Vec<String> {
    let mut out = Vec::new();
    if was.slow != now.slow {
        out.push(if now.slow == 0 {
            "slow mode off".to_string()
        } else {
            format!("slow mode {}", fmt_secs(now.slow as u64))
        });
    }
    if was.followers != now.followers {
        out.push(match now.followers {
            n if n < 0 => "followers-only off".to_string(),
            0 => "followers-only on".to_string(),
            n => format!("followers-only {}", fmt_secs(n as u64 * 60)),
        });
    }
    if was.emote_only != now.emote_only {
        out.push(on_off("emote-only", now.emote_only));
    }
    if was.subs_only != now.subs_only {
        out.push(on_off("sub-only", now.subs_only));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{parse, Event};

    /// a full state (the join ROOMSTATE) with every tag present.
    fn full(slow: u32, followers: i32, emote_only: bool, subs_only: bool) -> RoomState {
        RoomState {
            slow: Some(slow),
            followers: Some(followers),
            emote_only: Some(emote_only),
            subs_only: Some(subs_only),
        }
    }
    const NONE: RoomState = RoomState {
        slow: None,
        followers: None,
        emote_only: None,
        subs_only: None,
    };

    fn whats(v: &[ChatLine]) -> Vec<String> {
        v.iter()
            .map(|l| l.note.as_ref().unwrap().what.clone())
            .collect()
    }

    #[test]
    fn first_state_is_silent_then_changes_speak() {
        let mut r = RoomModes::new();
        assert!(r.update("c", full(0, -1, false, false)).is_empty());
        for (next, want) in [
            (full(30, -1, false, false), vec!["slow mode 30s"]),
            (full(120, -1, false, false), vec!["slow mode 2m"]),
            (full(0, -1, false, false), vec!["slow mode off"]),
            (full(0, 10, false, false), vec!["followers-only 10m"]),
            (full(0, 0, false, false), vec!["followers-only on"]),
            (full(0, 90, false, false), vec!["followers-only 90m"]),
            (full(0, -1, false, false), vec!["followers-only off"]),
            (full(0, -1, true, false), vec!["emote-only on"]),
            (full(0, -1, false, false), vec!["emote-only off"]),
            (full(0, -1, false, true), vec!["sub-only on"]),
            (
                full(5, 1, true, true),
                vec!["slow mode 5s", "followers-only 1m", "emote-only on"],
            ),
        ] {
            let got = r.update("c", next);
            assert_eq!(whats(&got), want, "{next:?}");
            assert!(got
                .iter()
                .all(|l| l.note.as_ref().unwrap().kind == NoteKind::Mode));
        }
    }

    #[test]
    fn a_partial_update_changes_only_what_it_names() {
        let mut r = RoomModes::new();
        r.update("c", full(0, 10, true, true));
        // only slow changed: the other three stay, no spurious "off" notes.
        let got = r.update(
            "c",
            RoomState {
                slow: Some(30),
                ..NONE
            },
        );
        assert_eq!(whats(&got), ["slow mode 30s"]);
        assert!(
            r.update("c", NONE).is_empty(),
            "an empty update says nothing"
        );
        let got = r.update(
            "c",
            RoomState {
                emote_only: Some(false),
                ..NONE
            },
        );
        assert_eq!(whats(&got), ["emote-only off"]);
        // slow is still 30, followers still 10, subs-only still on
        let got = r.update("c", full(30, 10, false, true));
        assert!(got.is_empty(), "state was carried through: {got:?}");
    }

    #[test]
    fn a_partial_first_state_fills_the_rest_with_off() {
        let mut r = RoomModes::new();
        assert!(r
            .update(
                "c",
                RoomState {
                    slow: Some(30),
                    ..NONE
                }
            )
            .is_empty());
        assert_eq!(
            whats(&r.update("c", full(30, -1, true, false))),
            ["emote-only on"]
        );
    }

    #[test]
    fn an_unchanged_state_and_other_channels_say_nothing() {
        let mut r = RoomModes::new();
        let off = full(0, -1, false, false);
        let slow9 = full(9, -1, false, false);
        r.update("a", off);
        assert!(r.update("a", off).is_empty());
        assert!(r.update("b", slow9).is_empty(), "b's first");
        assert!(r.update("B", slow9).is_empty(), "case-blind");
        assert_eq!(r.update("a", slow9).len(), 1);
    }

    #[test]
    fn the_frame_parses() {
        let raw = r#"{"type":"irc:message","channel":"Chan","message":{"type":"roomstate",
            "emoteOnly":true,"followersOnly":10,"slow":30,"subsOnly":false}}"#;
        match parse(raw) {
            Event::Room(c, s) => {
                assert_eq!(c, "Chan");
                assert_eq!(s, full(30, 10, true, false));
            }
            e => panic!("{e:?}"),
        }
        assert_eq!(
            parse(r#"{"type":"irc:message","message":{"type":"roomstate"}}"#),
            Event::Ignore
        );
        // a partial frame: absent tags stay None
        let part =
            r#"{"type":"irc:message","channel":"c","message":{"type":"roomstate","slow":30}}"#;
        assert!(matches!(
            parse(part),
            Event::Room(
                _,
                RoomState {
                    slow: Some(30),
                    followers: None,
                    emote_only: None,
                    subs_only: None
                }
            )
        ));
    }
}
