//! the emotes this person actually sends, by picture (`provider:id`, never the
//! name), with a decayed count: recent use weighs more and last week's habit
//! fades. pure data + a line format; the file work lives in the client. it is
//! what the user typed, so it never leaves the machine.

use std::collections::HashMap;

/// most pictures kept; the least-used (decayed) go first.
pub const CAP: usize = 400;
/// a use is worth half after this long.
pub const HALF_LIFE_SECS: f64 = 7.0 * 86_400.0;
/// a key longer than this is not a picture key.
const MAX_KEY: usize = 120;

#[derive(Clone, Debug, Default)]
pub struct Usage {
    /// key → (score as of `at`, `at` in unix seconds)
    map: HashMap<String, (f64, u64)>,
}

fn decayed(score: f64, at: u64, now: u64) -> f64 {
    let dt = now.saturating_sub(at) as f64;
    score * 0.5f64.powf(dt / HALF_LIFE_SECS)
}

fn good_key(k: &str) -> bool {
    !k.is_empty() && k.len() <= MAX_KEY && !k.chars().any(char::is_control)
}

impl Usage {
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// the decayed use of a picture as of `now` (0 = never).
    pub fn score(&self, key: &str, now: u64) -> f64 {
        self.map
            .get(key)
            .map_or(0.0, |&(s, at)| decayed(s, at, now))
    }

    /// the person just sent this picture.
    pub fn note(&mut self, key: &str, now: u64) {
        if !good_key(key) {
            return;
        }
        let was = self.score(key, now);
        self.map.insert(key.to_string(), (was + 1.0, now));
        if self.map.len() > CAP {
            // drop the least-used, the key breaking ties so it never depends on hash order
            let mut all: Vec<(f64, &String)> = self
                .map
                .iter()
                .map(|(k, &(s, at))| (decayed(s, at, now), k))
                .collect();
            all.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(b.1)));
            let drop: Vec<String> = all
                .iter()
                .take(self.map.len() - CAP)
                .map(|(_, k)| (*k).clone())
                .collect();
            for k in drop {
                self.map.remove(&k);
            }
        }
    }

    /// `key<TAB>score<TAB>unix seconds`, one per line, in key order.
    pub fn render(&self) -> String {
        let mut rows: Vec<(&String, &(f64, u64))> = self.map.iter().collect();
        rows.sort_by(|a, b| a.0.cmp(b.0));
        let mut out = String::new();
        for (k, (s, at)) in rows {
            out.push_str(&format!("{k}\t{s:.4}\t{at}\n"));
        }
        out
    }

    /// read what `render` wrote. a corrupt, hand-edited or oversized file never
    /// fails: a bad line is skipped, a hopeless file is just empty.
    pub fn parse(text: &str) -> Usage {
        let mut u = Usage::default();
        for line in text.lines().take(CAP * 2) {
            let mut f = line.split('\t');
            let (Some(k), Some(s), Some(at), None) = (f.next(), f.next(), f.next(), f.next())
            else {
                continue;
            };
            let (Ok(s), Ok(at)) = (s.parse::<f64>(), at.parse::<u64>()) else {
                continue;
            };
            if good_key(k) && s.is_finite() && s > 0.0 && s < 1e6 {
                u.map.insert(k.to_string(), (s, at));
            }
        }
        // a file over the cap (hand-edited) keeps the cap's worth, deterministically
        if u.map.len() > CAP {
            let newest = u.map.values().map(|v| v.1).max().unwrap_or(0);
            let mut all: Vec<(f64, String)> = u
                .map
                .iter()
                .map(|(k, &(s, at))| (decayed(s, at, newest), k.clone()))
                .collect();
            all.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
            for (_, k) in all.into_iter().skip(CAP) {
                u.map.remove(&k);
            }
        }
        u
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400;

    #[test]
    fn a_use_counts_and_adds_up() {
        let mut u = Usage::default();
        assert_eq!(u.score("7tv:a", 100), 0.0);
        u.note("7tv:a", 100);
        u.note("7tv:a", 100);
        assert!((u.score("7tv:a", 100) - 2.0).abs() < 1e-9);
    }

    #[test]
    fn last_weeks_habit_fades_and_recent_use_outweighs_it() {
        let mut u = Usage::default();
        for _ in 0..4 {
            u.note("7tv:old", 0);
        }
        u.note("7tv:new", 21 * DAY);
        // two half-lives on: four old uses are worth one
        assert!((u.score("7tv:old", 14 * DAY) - 1.0).abs() < 1e-9);
        // a month on the old habit is almost nothing and the recent one leads
        let later = 42 * DAY;
        assert!(u.score("7tv:new", later) > u.score("7tv:old", later));
        assert!(u.score("7tv:old", later) < 0.1);
    }

    #[test]
    fn it_is_capped_and_the_least_used_go_first() {
        let mut u = Usage::default();
        u.note("7tv:keep", 0);
        u.note("7tv:keep", 0);
        for i in 0..CAP + 50 {
            u.note(&format!("7tv:n{i}"), 0);
        }
        assert_eq!(u.len(), CAP);
        assert!(u.score("7tv:keep", 0) > 1.9, "the used one stays");
        // and which of the ties goes is by key, not by hash order
        let mut again = Usage::default();
        again.note("7tv:keep", 0);
        again.note("7tv:keep", 0);
        for i in 0..CAP + 50 {
            again.note(&format!("7tv:n{i}"), 0);
        }
        assert_eq!(u.render(), again.render());
    }

    #[test]
    fn a_file_round_trips() {
        let mut u = Usage::default();
        u.note("7tv:a", 5);
        u.note("kick:123", 9);
        let back = Usage::parse(&u.render());
        assert_eq!(back.render(), u.render());
    }

    #[test]
    fn a_corrupt_or_hostile_file_is_empty_not_an_error() {
        assert!(Usage::parse("").is_empty());
        assert!(Usage::parse("\u{0}\u{1}\u{2}not a table at all\n\n\t\t\n").is_empty());
        assert!(Usage::parse("a\tNaN\t5\nb\tinf\t5\nc\t-3\t5\nd\t1\tnot\n").is_empty());
        assert!(Usage::parse("a\t1\t5\textra\n").is_empty());
        // a good line among bad ones survives
        let u = Usage::parse("junk\n7tv:ok\t2.5\t7\n\t1\t1\n");
        assert_eq!(u.len(), 1);
        assert!((u.score("7tv:ok", 7) - 2.5).abs() < 1e-9);
        // a huge file never grows past the cap
        let big: String = (0..CAP * 5).map(|i| format!("7tv:k{i}\t1\t0\n")).collect();
        assert!(Usage::parse(&big).len() <= CAP);
        // a key with control characters is refused on the way in too
        let mut u = Usage::default();
        u.note("a\tb", 1);
        u.note("a\nb", 1);
        u.note("", 1);
        assert!(u.is_empty());
    }
}
