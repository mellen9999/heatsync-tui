//! room-mode changes → system notes. twitch re-sends ROOMSTATE on join and on
//! every mode change; the first state per channel is only remembered, later
//! ones say what changed (`slow mode 30s`, `emote-only on`, …).

use std::collections::HashMap;

use crate::proto::{fmt_secs, note_line, ChatLine, RoomState};
use crate::{NoteKind, Platform};

#[derive(Default)]
pub struct RoomModes {
    seen: HashMap<String, RoomState>,
}

impl RoomModes {
    pub fn new() -> Self {
        Self::default()
    }

    /// fold in a channel's new state; returns a note line per change.
    pub fn update(&mut self, channel: &str, now: RoomState) -> Vec<ChatLine> {
        let key = channel.to_lowercase();
        let Some(was) = self.seen.insert(key, now) else {
            return Vec::new();
        };
        changes(was, now)
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

fn changes(was: RoomState, now: RoomState) -> Vec<String> {
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

    const OFF: RoomState = RoomState {
        slow: 0,
        followers: -1,
        emote_only: false,
        subs_only: false,
    };

    fn whats(v: &[ChatLine]) -> Vec<String> {
        v.iter()
            .map(|l| l.note.as_ref().unwrap().what.clone())
            .collect()
    }

    #[test]
    fn first_state_is_silent_then_changes_speak() {
        let mut r = RoomModes::new();
        assert!(r.update("c", OFF).is_empty());
        for (next, want) in [
            (RoomState { slow: 30, ..OFF }, vec!["slow mode 30s"]),
            (RoomState { slow: 120, ..OFF }, vec!["slow mode 2m"]),
            (OFF, vec!["slow mode off"]),
            (
                RoomState {
                    followers: 10,
                    ..OFF
                },
                vec!["followers-only 10m"],
            ),
            (
                RoomState {
                    followers: 0,
                    ..OFF
                },
                vec!["followers-only on"],
            ),
            (
                RoomState {
                    followers: 90,
                    ..OFF
                },
                vec!["followers-only 90m"],
            ),
            (OFF, vec!["followers-only off"]),
            (
                RoomState {
                    emote_only: true,
                    ..OFF
                },
                vec!["emote-only on"],
            ),
            (OFF, vec!["emote-only off"]),
            (
                RoomState {
                    subs_only: true,
                    ..OFF
                },
                vec!["sub-only on"],
            ),
            (
                RoomState {
                    slow: 5,
                    subs_only: true,
                    emote_only: true,
                    followers: 1,
                },
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
    fn an_unchanged_state_and_other_channels_say_nothing() {
        let mut r = RoomModes::new();
        r.update("a", OFF);
        assert!(r.update("a", OFF).is_empty());
        assert!(
            r.update("b", RoomState { slow: 9, ..OFF }).is_empty(),
            "b's first"
        );
        assert!(
            r.update("B", RoomState { slow: 9, ..OFF }).is_empty(),
            "case-blind"
        );
        assert_eq!(r.update("a", RoomState { slow: 9, ..OFF }).len(), 1);
    }

    #[test]
    fn the_frame_parses() {
        let raw = r#"{"type":"irc:message","channel":"Chan","message":{"type":"roomstate",
            "emoteOnly":true,"followersOnly":10,"slow":30,"subsOnly":false}}"#;
        match parse(raw) {
            Event::Room(c, s) => {
                assert_eq!(c, "Chan");
                assert_eq!(
                    s,
                    RoomState {
                        slow: 30,
                        followers: 10,
                        emote_only: true,
                        subs_only: false
                    }
                );
            }
            e => panic!("{e:?}"),
        }
        assert_eq!(
            parse(r#"{"type":"irc:message","message":{"type":"roomstate"}}"#),
            Event::Ignore
        );
    }
}
