//! tab completion for the composer — emote names and usernames, twitch-style.
//!
//! pure text machinery: the face hands in the line, the cursor, and candidate
//! iterators; this returns what span to replace with what. cycling state lives
//! here so repeated tabs walk the match list; ANY other edit drops the state
//! (the face clears it), which is what makes the walk feel like one gesture.
//!
//! `@word` completes usernames only (the `@` is kept); a bare word matches
//! emote names first (case-insensitive prefix), then usernames — same order
//! every chat client trains.

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
        emotes: impl Iterator<Item = &'a str>,
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
        let matches =
            |c: &str| c.get(..prefix.len()).is_some_and(|h| h.eq_ignore_ascii_case(prefix));
        let mut items: Vec<String> = Vec::new();
        let mut push = |s: String| {
            if !items.iter().any(|i| i.eq_ignore_ascii_case(&s)) {
                items.push(s);
            }
        };
        if !at {
            let mut hits: Vec<&str> = emotes.filter(|e| matches(e)).collect();
            hits.sort_unstable_by_key(|a| a.to_ascii_lowercase());
            for e in hits {
                push(e.to_string());
            }
        }
        let mut hits: Vec<&str> = users.filter(|u| matches(u)).collect();
        hits.sort_unstable_by_key(|a| a.to_ascii_lowercase());
        for u in hits {
            push(if at { format!("@{u}") } else { u.to_string() });
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

#[cfg(test)]
mod tests {
    use super::*;

    fn build(text: &str, cursor: usize) -> Option<Completion> {
        let emotes = ["KEKW", "Kappa", "kekHeim", "GAMBA"];
        let users = ["kekuser", "Gamba_Andy", "mellen"];
        Completion::build(
            text,
            cursor,
            emotes.iter().copied(),
            users.iter().copied(),
        )
    }

    #[test]
    fn completes_emotes_case_insensitively_then_users() {
        let mut c = build("hi kek", 6).unwrap();
        assert_eq!(c.advance(1), (3, 6, "kekHeim"));
        assert_eq!(c.advance(1).2, "KEKW");
        assert_eq!(c.advance(1).2, "kekuser");
        assert_eq!(c.advance(1).2, "kekHeim", "wraps");
    }

    #[test]
    fn advance_replaces_the_previous_completion_not_the_word() {
        let mut c = build("kek", 3).unwrap();
        let (lo, hi, s) = c.advance(1);
        assert_eq!((lo, hi, s), (0, 3, "kekHeim"));
        // the line now holds "kekHeim" — the next advance must span all 7 chars
        let (lo, hi, s) = c.advance(1);
        assert_eq!((lo, hi), (0, 7));
        assert_eq!(s, "KEKW");
    }

    #[test]
    fn backwards_starts_at_the_end_of_the_list() {
        let mut c = build("kek", 3).unwrap();
        assert_eq!(c.advance(-1).2, "kekuser");
        assert_eq!(c.advance(-1).2, "KEKW");
    }

    #[test]
    fn at_prefix_completes_users_only_and_keeps_the_at() {
        let mut c = build("yo @ga", 6).unwrap();
        assert_eq!(c.advance(1), (3, 6, "@Gamba_Andy"));
        assert!(build("yo @kekw", 8).is_none(), "no user matches — emotes excluded");
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
        assert_eq!(c.advance(1), (4, 7, "kekHeim"));
    }
}
