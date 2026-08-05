//! fuzzy candidate scoring for completion. pure, deterministic — tiers rank
//! match quality, a sub-score orders within a tier, and ties break
//! alphabetically so cycling is stable across runs.

/// match quality, best first (derived Ord: lower variant = better).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    PrefixExact,
    PrefixCi,
    WordBoundary, // query chars land on word starts / camel humps
    Substring,
    Subsequence,
}

/// a scored hit: tier first, then the in-tier sub-score (smaller = better —
/// substring start index, or subsequence span length).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Match {
    pub tier: Tier,
    pub sub: u32,
}

/// score one candidate against the query. None = no match. empty query
/// matches everything at top tier (Tab on a bare word lists the whole pool).
pub fn score(query: &str, cand: &str) -> Option<Match> {
    if query.is_empty() {
        return Some(Match {
            tier: Tier::PrefixExact,
            sub: 0,
        });
    }
    if cand.starts_with(query) {
        return Some(Match {
            tier: Tier::PrefixExact,
            sub: 0,
        });
    }
    let ql = query.to_lowercase();
    let cl = cand.to_lowercase();
    if cl.starts_with(&ql) {
        return Some(Match {
            tier: Tier::PrefixCi,
            sub: 0,
        });
    }
    if word_boundary_hit(&ql, cand) {
        return Some(Match {
            tier: Tier::WordBoundary,
            sub: 0,
        });
    }
    if let Some(pos) = cl.find(&ql) {
        return Some(Match {
            tier: Tier::Substring,
            sub: pos as u32,
        });
    }
    subsequence_span(&ql, &cl).map(|span| Match {
        tier: Tier::Subsequence,
        sub: span,
    })
}

/// do the query chars all land on "word starts" of the candidate — position 0,
/// after a non-alphanumeric, or on a lower→UPPER camel hump?
fn word_boundary_hit(ql: &str, cand: &str) -> bool {
    let mut starts = String::new();
    let mut prev: Option<char> = None;
    for c in cand.chars() {
        let start = match prev {
            None => true,
            Some(p) => !p.is_alphanumeric() || (p.is_lowercase() && c.is_uppercase()),
        };
        if start {
            starts.extend(c.to_lowercase());
        }
        prev = Some(c);
    }
    starts.starts_with(ql)
}

/// smallest-window subsequence match: query chars appear in order. returns the
/// span (last-first) so tighter matches rank better. simple greedy first-fit,
/// span measured in chars.
fn subsequence_span(ql: &str, cl: &str) -> Option<u32> {
    let mut it = ql.chars();
    let mut want = it.next()?;
    let mut first: Option<u32> = None;
    for (i, c) in cl.chars().enumerate() {
        if c == want {
            if first.is_none() {
                first = Some(i as u32);
            }
            match it.next() {
                Some(n) => want = n,
                None => return Some(i as u32 - first.unwrap_or(0)),
            }
        }
    }
    None
}

/// rank items by fuzzy score, best first, alphabetical ci tie-break, capped.
/// returns indices into `items`.
pub fn rank<T>(items: &[T], key: impl Fn(&T) -> &str, query: &str, cap: usize) -> Vec<usize> {
    let mut hits: Vec<(Match, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, it)| score(query, key(it)).map(|m| (m, i)))
        .collect();
    hits.sort_by(|a, b| {
        a.0.cmp(&b.0).then_with(|| {
            let (ka, kb) = (key(&items[a.1]), key(&items[b.1]));
            ka.to_lowercase()
                .cmp(&kb.to_lowercase())
                .then_with(|| ka.cmp(kb))
        })
    });
    hits.truncate(cap);
    hits.into_iter().map(|(_, i)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(query: &str, cand: &str) -> Option<Tier> {
        score(query, cand).map(|m| m.tier)
    }

    #[test]
    fn tier_ladder() {
        assert_eq!(t("KEK", "KEKW"), Some(Tier::PrefixExact));
        assert_eq!(t("kek", "KEKW"), Some(Tier::PrefixCi));
        assert_eq!(t("pf", "peepoFine"), Some(Tier::WordBoundary));
        assert_eq!(t("ekw", "KEKW"), Some(Tier::Substring));
        assert_eq!(t("kw", "KEKW"), Some(Tier::Substring)); // all-caps has no humps
        assert_eq!(t("pep", "peepoFine"), Some(Tier::Subsequence));
        assert_eq!(t("xyz", "KEKW"), None);
    }

    #[test]
    fn word_boundary_snake_and_camel() {
        assert_eq!(t("ff", "flip_flop"), Some(Tier::WordBoundary));
        assert_eq!(t("gw", "GetWide"), Some(Tier::WordBoundary));
    }

    #[test]
    fn substring_position_orders() {
        let a = score("gg", "OMEGGA").unwrap();
        let b = score("gg", "GGwp").unwrap(); // ci-prefix actually
        assert!(b < a);
        let c = score("eg", "OMEGA").unwrap();
        let d = score("eg", "xxxOMEGA").unwrap();
        assert!(c < d, "earlier substring ranks better");
    }

    #[test]
    fn tight_subsequence_beats_gappy() {
        let tight = score("kkw", "KEKW").unwrap(); // K.KW span 3
        let gappy = score("kkw", "KappaXXXKW").unwrap();
        assert!(tight < gappy);
    }

    #[test]
    fn empty_query_matches_all_top() {
        assert_eq!(t("", "anything"), Some(Tier::PrefixExact));
    }

    #[test]
    fn utf8_safe() {
        assert_eq!(t("é", "élan"), Some(Tier::PrefixExact));
        assert_eq!(t("É", "élan"), Some(Tier::PrefixCi));
        assert!(score("ñ", "año").is_some());
    }

    #[test]
    fn rank_orders_and_caps() {
        let pool = ["xKEKW", "KEKW", "KEKWait", "kekuser", "zzz"];
        let r = rank(&pool, |s| s, "kek", 3);
        let names: Vec<&str> = r.iter().map(|&i| pool[i]).collect();
        assert_eq!(names, vec!["kekuser", "KEKW", "KEKWait"]);
        // kekuser is exact-prefix; KEKW/KEKWait ci-prefix alphabetical; xKEKW capped out
    }

    #[test]
    fn rank_stable_alphabetical_tiebreak() {
        let pool = ["Bbb", "bba", "bbc"];
        let r = rank(&pool, |s| s, "bb", 10);
        let names: Vec<&str> = r.iter().map(|&i| pool[i]).collect();
        // exact-prefix tier (bba, bbc — alphabetical) beats ci-prefix (Bbb)
        assert_eq!(names, vec!["bba", "bbc", "Bbb"]);
    }
}
