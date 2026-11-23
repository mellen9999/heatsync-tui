//! tab completion for the composer — emote names and usernames, twitch-style.
//!
//! pure text machinery: the face hands in the line, the cursor, a ranker for
//! emotes and the recent chatters; this returns what span to replace with
//! what. cycling state lives here so repeated tabs walk the match list; ANY
//! other edit drops the state (the face clears it), which is what makes the
//! walk feel like one gesture.
//!
//! `@word` completes usernames only (the `@` is kept); a bare word offers, in
//! this order: the exact emote name, the room's emotes, the people, then every
//! other emote (the emote law's order inside each: see `law::offer`). an emote
//! needs two typed characters — one letter would offer half the set.

/// the fewest typed characters that offer emotes.
pub const MIN_EMOTE: usize = 2;

/// the emote names tab offers for a word, split around the people: `first` is
/// the exact name and the room's own emotes, `rest` is everything else.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Ranked {
    pub first: Vec<String>,
    pub rest: Vec<String>,
}

use std::collections::HashSet;

/// a completion in progress: the matches for the word that was under the
/// cursor, and which one currently occupies the line.
#[derive(Debug)]
pub struct Completion {
    /// char index where the completed span begins.
    start: usize,
    /// chars the span currently occupies (the original word, then whichever
    /// candidate was last inserted).
    len: usize,
    items: Vec<String>,
    /// index of the item currently in the line; None until the first advance.
    idx: Option<usize>,
}

impl Completion {
    /// find the word ending at `cursor` and gather its matches. None when the
    /// cursor doesn't end a word or nothing matches — the tab then does nothing.
    pub fn build<'a>(
        text: &str,
        cursor: usize,
        emotes: impl FnOnce(&str) -> Ranked,
        users: impl Iterator<Item = &'a str>,
    ) -> Option<Completion> {
        let chars: Vec<char> = text.chars().collect();
        let cursor = cursor.min(chars.len());
        let start = chars[..cursor]
            .iter()
            .rposition(|c| c.is_whitespace())
            .map(|i| i + 1)
            .unwrap_or(0);
        if start == cursor {
            return None; // nothing typed to complete
        }
        let word: String = chars[start..cursor].iter().collect();
        let (prefix, at) = match word.strip_prefix('@') {
            Some(rest) => (rest, true),
            None => (word.as_str(), false),
        };
        if prefix.is_empty() {
            return None;
        }

        // byte-safe: get() refuses a slice that would split a multibyte char
        // (an emote named "🅰x" must not panic the completer).
        let matches = |c: &str| {
            c.get(..prefix.len())
                .is_some_and(|h| h.eq_ignore_ascii_case(prefix))
        };
        let mut items: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut push = |s: String| {
            if seen.insert(s.clone()) {
                items.push(s);
            }
        };
        let ranked = if !at && prefix.chars().count() >= MIN_EMOTE {
            emotes(prefix)
        } else {
            Ranked::default()
        };
        for e in ranked.first {
            push(e);
        }
        let mut hits: Vec<&str> = users.filter(|u| matches(u)).collect();
        hits.sort_unstable_by_key(|a| twin_key(a, prefix));
        for u in hits {
            push(if at { format!("@{u}") } else { u.to_string() });
        }
        for e in ranked.rest {
            push(e);
        }

        (!items.is_empty()).then_some(Completion {
            start,
            len: cursor - start,
            items,
            idx: None,
        })
    }

    /// step to the next (+1) or previous (-1) match. returns the span to
    /// replace — `(lo, hi)` in char indices — and the replacement text; the
    /// internal span tracks the replacement so the next advance swaps it out.
    pub fn advance(&mut self, dir: i32) -> (usize, usize, &str) {
        let n = self.items.len();
        let next = match (self.idx, dir >= 0) {
            (None, true) => 0,
            (None, false) => n - 1,
            (Some(i), true) => (i + 1) % n,
            (Some(i), false) => (i + n - 1) % n,
        };
        self.idx = Some(next);
        let (lo, hi) = (self.start, self.start + self.len);
        self.len = self.items[next].chars().count();
        (lo, hi, &self.items[next])
    }
}

/// usernames: by lowercase name; case twins both stay, the one spelled like
/// the typed word leads, then plain byte order (HashMap order never leaks).
fn twin_key(name: &str, typed: &str) -> (String, bool, String) {
    let exact = name.get(..typed.len()).is_some_and(|h| h == typed);
    (name.to_ascii_lowercase(), !exact, name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emote::{Emote, EmoteSet};

    fn em(name: &str, provider: &str) -> Emote {
        Emote {
            name: name.into(),
            url: format!("https://cdn/{provider}/{name}.webp"),
            provider: provider.into(),
            id: format!("{provider}-{name}"),
            animated: false,
            zero_width: false,
            global: false,
            token: false,
        }
    }

    /// a room of 7tv emotes and nothing else.
    fn room(names: &[&str]) -> EmoteSet {
        EmoteSet::from_list(names.iter().map(|n| em(n, "7tv")))
    }

    fn run(set: &EmoteSet, users: &[&str], text: &str, cursor: usize) -> Option<Completion> {
        Completion::build(text, cursor, |t| set.offer(t), users.iter().copied())
    }

    fn build(text: &str, cursor: usize) -> Option<Completion> {
        let set = room(&["KEKW", "Kappa", "kekHeim", "GAMBA"]);
        run(&set, &["kekuser", "Gamba_Andy", "mellen"], text, cursor)
    }

    /// every match in the order tab walks them.
    fn walk(set: &EmoteSet, typed: &str) -> Vec<String> {
        let mut c = run(set, &[], typed, typed.len()).unwrap();
        let n = c.items.len();
        (0..n).map(|_| c.advance(1).2.to_string()).collect()
    }

    #[test]
    fn completes_emotes_case_insensitively_then_users() {
        let mut c = build("hi kek", 6).unwrap();
        // the shorter name leads inside a tier
        assert_eq!(c.advance(1), (3, 6, "KEKW"));
        assert_eq!(c.advance(1).2, "kekHeim");
        assert_eq!(c.advance(1).2, "kekuser");
        assert_eq!(c.advance(1).2, "KEKW", "wraps");
    }

    #[test]
    fn advance_replaces_the_previous_completion_not_the_word() {
        let mut c = build("kek", 3).unwrap();
        let (lo, hi, s) = c.advance(1);
        assert_eq!((lo, hi, s), (0, 3, "KEKW"));
        // the line now holds "KEKW" — the next advance must span those 4 chars
        let (lo, hi, s) = c.advance(1);
        assert_eq!((lo, hi), (0, 4));
        assert_eq!(s, "kekHeim");
    }

    #[test]
    fn backwards_starts_at_the_end_of_the_list() {
        let mut c = build("kek", 3).unwrap();
        assert_eq!(c.advance(-1).2, "kekuser");
        assert_eq!(c.advance(-1).2, "kekHeim");
    }

    #[test]
    fn at_prefix_completes_users_only_and_keeps_the_at() {
        let mut c = build("yo @ga", 6).unwrap();
        assert_eq!(c.advance(1), (3, 6, "@Gamba_Andy"));
        assert!(
            build("yo @kekw", 8).is_none(),
            "no user matches — emotes excluded"
        );
    }

    #[test]
    fn nothing_to_complete_yields_none() {
        assert!(build("", 0).is_none());
        assert!(build("word ", 5).is_none(), "cursor after a space");
        assert!(build("zzz", 3).is_none(), "no matches");
        assert!(build("@", 1).is_none(), "bare @");
    }

    #[test]
    fn word_boundaries_are_char_safe() {
        // multibyte text before the word must not break the span indices
        let mut c = build("キツネ kek", 7).unwrap();
        assert_eq!(c.advance(1), (4, 7, "KEKW"));
    }

    #[test]
    fn the_exact_name_leads_and_its_spelling_leads_its_twin() {
        let set = room(&["sadge", "Sadge", "SADGE", "SadgeBusiness"]);
        assert_eq!(
            walk(&set, "Sadge"),
            ["Sadge", "SADGE", "sadge", "SadgeBusiness"]
        );
        assert_eq!(
            walk(&set, "sadge"),
            ["sadge", "SADGE", "Sadge", "SadgeBusiness"]
        );
        // input order never matters
        let rev = room(&["SadgeBusiness", "SADGE", "Sadge", "sadge"]);
        assert_eq!(
            walk(&rev, "sadge"),
            ["sadge", "SADGE", "Sadge", "SadgeBusiness"]
        );
    }

    #[test]
    fn word_start_and_inside_matches_come_after_the_prefix_ones() {
        let set = room(&["scatter", "xcat0", "SadCat", "catJAM"]);
        // starts-with, then a word that starts there, then inside another word
        assert_eq!(walk(&set, "cat"), ["catJAM", "SadCat", "xcat0", "scatter"]);
    }

    #[test]
    fn the_room_comes_before_the_globals() {
        let mut globals = EmoteSet::from_global_list([em("Pog", "7tv"), em("Pogu", "7tv")]);
        globals.merge(EmoteSet::from_list([em("Poggers", "ffz")]));
        // typed "po": every one starts with it; the room's goes first even
        // though it is the longest
        assert_eq!(walk(&globals, "po"), ["Poggers", "Pog", "Pogu"]);
    }

    #[test]
    fn the_exact_name_then_the_room_then_people_then_the_rest() {
        // a global that starts with it, a global that is it, a room emote that
        // holds it inside a word, a room emote that starts with it
        let mut set = EmoteSet::from_list([em("xpogx", "7tv"), em("Pogchamp", "7tv")]);
        let mut globals = EmoteSet::from_global_list([em("Pogu", "7tv"), em("pog", "7tv")]);
        globals.merge(std::mem::take(&mut set));
        let mut c = run(&globals, &["pogman"], "pog", 3).unwrap();
        let order: Vec<String> = (0..5).map(|_| c.advance(1).2.to_string()).collect();
        assert_eq!(order, ["pog", "Pogchamp", "xpogx", "pogman", "Pogu"]);
    }

    #[test]
    fn at_means_people_only_and_a_name_is_never_offered_twice() {
        let set = room(&["mellen"]);
        let mut c = run(&set, &["mellen", "Mellen"], "me", 2).unwrap();
        let all: Vec<String> = (0..3).map(|_| c.advance(1).2.to_string()).collect();
        assert_eq!(
            all,
            ["mellen", "Mellen", "mellen"],
            "wraps after two distinct"
        );
        let mut c = run(&set, &["mellen"], "@me", 3).unwrap();
        assert_eq!(c.advance(1).2, "@mellen");
        assert_eq!(c.advance(1).2, "@mellen", "one item, wraps");
    }

    #[test]
    fn one_letter_offers_no_emotes_but_still_offers_people() {
        let set = room(&["KEKW", "Kappa"]);
        assert!(run(&set, &[], "k", 1).is_none());
        let mut c = run(&set, &["kevin"], "k", 1).unwrap();
        assert_eq!(c.advance(1).2, "kevin");
    }

    #[test]
    fn the_walk_does_not_depend_on_the_order_the_set_was_built_in() {
        let names = ["catJAM", "SadCat", "catKISS", "scatter", "Cat", "xcat0"];
        let mut rev = names;
        rev.reverse();
        assert_eq!(walk(&room(&names), "cat"), walk(&room(&rev), "cat"));
    }
}
