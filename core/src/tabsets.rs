//! one tab's emote sets: the pooled set (completion, blocks, kick sends) and,
//! for a merged tab, one set per source room. the law reads a ROW against its
//! own platform's room, never one pooled set in tab order: a kick row draws
//! kick-room emotes, a twitch row twitch-room emotes.

use std::ops::Deref;

use crate::emote::EmoteSet;
use crate::{Message, Platform};

pub struct TabSets {
    pooled: EmoteSet,
    /// (platform, lowercase channel, that room's set + the globals), in the
    /// tab's source order. empty for a single-source tab.
    rooms: Vec<(Platform, String, EmoteSet)>,
}

impl TabSets {
    pub fn new(pooled: EmoteSet, rooms: Vec<(Platform, String, EmoteSet)>) -> TabSets {
        TabSets { pooled, rooms }
    }

    /// the set a row from `platform`'s `channel` is read against. a line that
    /// does not say its channel (history) takes the tab's first source on that
    /// platform. a single-source tab has one set.
    pub fn for_line(&self, platform: Platform, channel: &str) -> &EmoteSet {
        if self.rooms.is_empty() {
            return &self.pooled;
        }
        let hit = if channel.is_empty() {
            self.rooms.iter().find(|r| r.0 == platform)
        } else {
            self.rooms
                .iter()
                .find(|r| r.0 == platform && r.1.eq_ignore_ascii_case(channel))
        };
        hit.map_or(&self.pooled, |r| &r.2)
    }

    pub fn for_row(&self, m: &Message) -> &EmoteSet {
        self.for_line(m.platform, &m.extra.channel)
    }
}

impl From<EmoteSet> for TabSets {
    fn from(pooled: EmoteSet) -> TabSets {
        TabSets::new(pooled, Vec::new())
    }
}

impl Deref for TabSets {
    type Target = EmoteSet;
    fn deref(&self) -> &EmoteSet {
        &self.pooled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emote::Emote;

    fn pog(url: &str) -> EmoteSet {
        EmoteSet::from_list([Emote {
            name: "Pog".into(),
            url: url.into(),
            provider: "7tv".into(),
            id: url.into(),
            animated: false,
            zero_width: false,
            global: false,
            kind: Default::default(),
        }])
    }

    fn merged() -> TabSets {
        TabSets::new(
            pog("pooled"),
            vec![
                (Platform::Twitch, "a".into(), pog("twitch-a")),
                (Platform::Twitch, "b".into(), pog("twitch-b")),
                (Platform::Kick, "k".into(), pog("kick-k")),
                (Platform::Youtube, "vid".into(), EmoteSet::new()),
            ],
        )
    }

    fn url(s: &EmoteSet) -> Option<&str> {
        s.get("Pog").map(|e| e.url.as_str())
    }

    #[test]
    fn a_row_reads_its_own_platforms_room_not_the_pool() {
        let t = merged();
        assert_eq!(url(t.for_line(Platform::Twitch, "a")), Some("twitch-a"));
        assert_eq!(url(t.for_line(Platform::Twitch, "B")), Some("twitch-b"));
        assert_eq!(url(t.for_line(Platform::Kick, "k")), Some("kick-k"));
        // a youtube row has no room emotes: it never borrows another platform's
        assert_eq!(url(t.for_line(Platform::Youtube, "vid")), None);
    }

    #[test]
    fn a_line_that_names_no_channel_takes_the_first_source_on_its_platform() {
        let t = merged();
        assert_eq!(url(t.for_line(Platform::Twitch, "")), Some("twitch-a"));
        assert_eq!(url(t.for_line(Platform::Kick, "")), Some("kick-k"));
    }

    #[test]
    fn a_row_from_no_source_of_the_tab_and_a_single_source_tab_use_the_pool() {
        assert_eq!(
            url(merged().for_line(Platform::Twitch, "zzz")),
            Some("pooled")
        );
        let one: TabSets = pog("only").into();
        assert_eq!(url(one.for_line(Platform::Kick, "x")), Some("only"));
    }
}
