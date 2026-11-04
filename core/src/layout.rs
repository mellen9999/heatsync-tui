//! the heatsync.org `/m/<layout>` share-link grammar, parse + build.
//!
//!   layout := tab ('+' tab)*      tabs, one chat column each (8 max)
//!   tab    := pc (',' pc)*        simulcast: platforms merged into one tab
//!   pc     := ('t'|'k'|'y') ':' name
//!
//! mirrors server/lib/multichat-layout.ts. parsing is total: junk is dropped,
//! never an error. an over-long layout parses to nothing, like the site.

pub const MAX_TABS: usize = 8;
pub const MAX_LEN: usize = 400;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tab {
    pub twitch: Option<String>,
    pub kick: Option<String>,
    /// bare handle (no `@`) or a `UC…` channel id.
    pub youtube: Option<String>,
}

impl Tab {
    fn is_empty(&self) -> bool {
        self.twitch.is_none() && self.kick.is_none() && self.youtube.is_none()
    }
}

fn name_ok(s: &str) -> bool {
    (1..=30).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

fn yt_handle_ok(s: &str) -> bool {
    (1..=40).contains(&s.len())
        && s.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-')
        })
}

fn uc_id_ok(s: &str) -> bool {
    s.len() == 24
        && s.starts_with("UC")
        && s.bytes()
            .skip(2)
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// pull the layout out of a full url, a `/m/...` path, or a bare layout.
fn extract(input: &str) -> &str {
    let s = input.trim();
    let s = s.split(['?', '#']).next().unwrap_or("");
    let s = match s.find("/m/") {
        Some(i) => &s[i + 3..],
        None => s,
    };
    s.trim_matches('/')
}

pub fn parse(input: &str) -> Vec<Tab> {
    let layout = extract(input);
    if layout.is_empty() || layout.len() > MAX_LEN {
        return Vec::new();
    }
    let mut tabs: Vec<Tab> = Vec::new();
    for tab_str in layout.split('+').take(MAX_TABS) {
        let mut tab = Tab::default();
        for seg in tab_str.split(',').take(3) {
            let Some((code, rest)) = seg.split_once(':') else {
                continue;
            };
            let exact = rest.trim().trim_start_matches('@');
            let low = exact.to_ascii_lowercase();
            match code.trim().to_ascii_lowercase().as_str() {
                "t" if name_ok(&low) => tab.twitch = Some(low),
                "k" if name_ok(&low) => tab.kick = Some(low),
                "y" if uc_id_ok(exact) => tab.youtube = Some(exact.to_string()),
                "y" if yt_handle_ok(&low) => tab.youtube = Some(low),
                _ => {}
            }
        }
        if !tab.is_empty() && !tabs.contains(&tab) {
            tabs.push(tab);
        }
    }
    tabs
}

fn one(t: &Tab) -> String {
    let mut segs: Vec<String> = Vec::new();
    if let Some(n) = t.twitch.as_deref().filter(|n| name_ok(n)) {
        segs.push(format!("t:{n}"));
    }
    if let Some(n) = t.kick.as_deref().filter(|n| name_ok(n)) {
        segs.push(format!("k:{n}"));
    }
    if let Some(n) = t
        .youtube
        .as_deref()
        .filter(|n| yt_handle_ok(n) || uc_id_ok(n))
    {
        segs.push(format!("y:{n}"));
    }
    segs.join(",")
}

/// serialise tabs. invalid legs are skipped, at most 8 tabs, and tail tabs are
/// dropped until the whole thing fits the 400-char cap (so it parses intact).
pub fn build(tabs: &[Tab]) -> String {
    let mut parts: Vec<String> = tabs
        .iter()
        .map(one)
        .filter(|s| !s.is_empty())
        .take(MAX_TABS)
        .collect();
    while !parts.is_empty() && parts.join("+").len() > MAX_LEN {
        parts.pop();
    }
    parts.join("+")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(tw: &str, ki: &str, yt: &str) -> Tab {
        let o = |s: &str| (!s.is_empty()).then(|| s.to_string());
        Tab {
            twitch: o(tw),
            kick: o(ki),
            youtube: o(yt),
        }
    }

    #[test]
    fn merged_tab_and_round_trip() {
        let l = "t:xqc,k:xqc,y:lofigirl+t:asmongold+k:trainwreckstv";
        let tabs = parse(l);
        assert_eq!(tabs.len(), 3);
        assert_eq!(tabs[0], t("xqc", "xqc", "lofigirl"));
        assert_eq!(build(&tabs), l);
    }

    #[test]
    fn accepts_url_path_and_bare() {
        for s in [
            "https://heatsync.org/m/t:a+k:b",
            "https://heatsync.org/m/t:a+k:b?utm=1#x",
            "/m/t:a+k:b",
            "t:a+k:b",
        ] {
            assert_eq!(parse(s), vec![t("a", "", ""), t("", "b", "")], "{s}");
        }
    }

    #[test]
    fn junk_dropped() {
        assert_eq!(
            parse("junk+t:ok,x:bad,k:BAD NAME+t:"),
            vec![t("ok", "", "")]
        );
        assert!(parse("").is_empty());
        assert!(parse("t:").is_empty());
        assert_eq!(parse("t:UPPER"), vec![t("upper", "", "")]);
    }

    #[test]
    fn youtube_handle_and_uc_id() {
        let uc = "UCabcdefghijklmnopqrstuv";
        assert_eq!(uc.len(), 24);
        assert_eq!(parse(&format!("y:{uc}")), vec![t("", "", uc)]);
        assert_eq!(parse("y:@Lo.fi"), vec![t("", "", "lo.fi")]);
        // not a UC id (wrong length) -> treated as a lowercased handle
        assert_eq!(parse("y:UCshort"), vec![t("", "", "ucshort")]);
    }

    #[test]
    fn ninth_tab_dropped_and_dupes_collapse() {
        let l = (1..=9)
            .map(|i| format!("t:c{i}"))
            .collect::<Vec<_>>()
            .join("+");
        let tabs = parse(&l);
        assert_eq!(tabs.len(), 8);
        assert_eq!(tabs[7], t("c8", "", ""));
        assert_eq!(parse("t:a+t:a").len(), 1);
        let nine: Vec<Tab> = (1..=9).map(|i| t(&format!("c{i}"), "", "")).collect();
        assert_eq!(parse(&build(&nine)).len(), 8);
    }

    #[test]
    fn length_cap() {
        assert!(parse(&format!("t:{}", "a".repeat(401))).is_empty());
        let name = "a".repeat(30);
        let tabs: Vec<Tab> = (0..8).map(|_| t(&name, &name, "")).collect();
        let tabs: Vec<Tab> = tabs
            .into_iter()
            .enumerate()
            .map(|(i, mut x)| {
                x.twitch = Some(format!("{}{i}", &name[..29]));
                x
            })
            .collect();
        let out = build(&tabs);
        assert!(out.len() <= MAX_LEN);
        let back = parse(&out);
        assert!(!back.is_empty() && back.len() < 8);
        assert_eq!(back[..], tabs[..back.len()]);
    }
}
