//! i18n — one small lookup over `locales/<lc>.txt` (`key=value` lines, embedded
//! with `include_str!` so `cargo package` ships them). the english file is the
//! source of truth: any key a locale lacks falls back to it, and a key english
//! lacks renders as "" — a raw key never reaches the screen.
//!
//! value escapes: `\n` newline, `\\` backslash. placeholders are `{name}`.
//! no i/o here: the face reads LANG & co (or the windows locale) and hands the
//! strings to [`detect`] / [`set`].

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

/// the 34 shipped locales — same set as the browser extension.
pub const LOCALES: [&str; 34] = [
    "ar", "bg", "cs", "da", "de", "el", "en", "es", "fi", "fr", "he", "hi", "hu", "id", "it", "ja",
    "ko", "ms", "nl", "no", "pl", "pt_BR", "pt_PT", "ro", "ru", "sk", "sv", "th", "tl", "tr", "uk",
    "vi", "zh_CN", "zh_TW",
];

const EN: usize = 6;

const FILES: [&str; 34] = [
    include_str!("../locales/ar.txt"),
    include_str!("../locales/bg.txt"),
    include_str!("../locales/cs.txt"),
    include_str!("../locales/da.txt"),
    include_str!("../locales/de.txt"),
    include_str!("../locales/el.txt"),
    include_str!("../locales/en.txt"),
    include_str!("../locales/es.txt"),
    include_str!("../locales/fi.txt"),
    include_str!("../locales/fr.txt"),
    include_str!("../locales/he.txt"),
    include_str!("../locales/hi.txt"),
    include_str!("../locales/hu.txt"),
    include_str!("../locales/id.txt"),
    include_str!("../locales/it.txt"),
    include_str!("../locales/ja.txt"),
    include_str!("../locales/ko.txt"),
    include_str!("../locales/ms.txt"),
    include_str!("../locales/nl.txt"),
    include_str!("../locales/no.txt"),
    include_str!("../locales/pl.txt"),
    include_str!("../locales/pt_BR.txt"),
    include_str!("../locales/pt_PT.txt"),
    include_str!("../locales/ro.txt"),
    include_str!("../locales/ru.txt"),
    include_str!("../locales/sk.txt"),
    include_str!("../locales/sv.txt"),
    include_str!("../locales/th.txt"),
    include_str!("../locales/tl.txt"),
    include_str!("../locales/tr.txt"),
    include_str!("../locales/uk.txt"),
    include_str!("../locales/vi.txt"),
    include_str!("../locales/zh_CN.txt"),
    include_str!("../locales/zh_TW.txt"),
];

type Map = HashMap<&'static str, Cow<'static, str>>;

static CATS: [OnceLock<Map>; 34] = [const { OnceLock::new() }; 34];
static CUR: AtomicUsize = AtomicUsize::new(EN);

fn parse(src: &'static str) -> Map {
    let mut m = Map::new();
    for line in src.lines() {
        if line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = if v.contains('\\') {
            Cow::Owned(v.replace("\\n", "\n").replace("\\\\", "\\"))
        } else {
            Cow::Borrowed(v)
        };
        m.insert(k.trim(), v);
    }
    m
}

fn cat(i: usize) -> &'static Map {
    CATS[i].get_or_init(|| parse(FILES[i]))
}

/// every key + raw value of a locale (tests).
pub fn entries(code: &str) -> Vec<(&'static str, &'static str)> {
    let Some(i) = LOCALES.iter().position(|l| *l == code) else {
        return Vec::new();
    };
    cat(i).iter().map(|(k, v)| (*k, v.as_ref())).collect()
}

/// the active locale code.
pub fn current() -> &'static str {
    LOCALES[CUR.load(Ordering::Relaxed)]
}

/// the locale a setting picks: an explicit code/locale string wins, "auto"/""
/// uses `detected` (the os locale). unknown → english.
pub fn pick(setting: &str, detected: &str) -> &'static str {
    let want = if setting.trim().is_empty() || setting.trim().eq_ignore_ascii_case("auto") {
        detected
    } else {
        setting
    };
    resolve(want)
}

/// make `pick(setting, detected)` the active locale.
pub fn set(setting: &str, detected: &str) {
    let lc = pick(setting, detected);
    let i = LOCALES.iter().position(|l| *l == lc).unwrap_or(EN);
    CUR.store(i, Ordering::Relaxed);
}

/// the translated string for `key` in the active locale, english if missing,
/// "" if english lacks it too.
pub fn tr(key: &str) -> &'static str {
    let cur = CUR.load(Ordering::Relaxed);
    if let Some(v) = cat(cur).get(key) {
        return v;
    }
    cat(EN).get(key).map(|v| v.as_ref()).unwrap_or("")
}

/// `tr` with `{name}` placeholders filled from `args`.
pub fn trf(key: &str, args: &[(&str, &str)]) -> String {
    fill(tr(key), args)
}

fn fill(s: &str, args: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    let mut rest = s;
    while let Some(a) = rest.find('{') {
        out.push_str(&rest[..a]);
        let after = &rest[a + 1..];
        match after.find('}') {
            Some(b) => {
                match args.iter().find(|(k, _)| *k == &after[..b]) {
                    Some((_, v)) => out.push_str(v),
                    None => out.push_str(&rest[a..a + b + 2]),
                }
                rest = &after[b + 1..];
            }
            None => {
                out.push_str(&rest[a..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// the placeholder names in `s`, in order (tests).
pub fn placeholders(s: &str) -> Vec<&str> {
    let mut v = Vec::new();
    let mut rest = s;
    while let Some(a) = rest.find('{') {
        let after = &rest[a + 1..];
        let Some(b) = after.find('}') else { break };
        v.push(&after[..b]);
        rest = &after[b + 1..];
    }
    v
}

/// os locale string (`pt_BR.UTF-8`, `zh-Hant-TW`, `nb_NO`, `fil`) → nearest
/// shipped code, "en" when nothing fits.
pub fn resolve(raw: &str) -> &'static str {
    let base = raw.split(['.', '@']).next().unwrap_or("");
    let norm = base.replace('-', "_").to_ascii_lowercase();
    let mut parts = norm.split('_');
    let lang = parts.next().unwrap_or("");
    let rest: Vec<&str> = parts.collect();
    let has = |t: &str| rest.contains(&t);
    let code = match lang {
        "" | "c" | "posix" => return "en",
        "nb" | "nn" | "no" => "no",
        "fil" | "tl" => "tl",
        "iw" => "he",
        "in" => "id",
        "pt" => {
            if has("pt") {
                "pt_PT"
            } else {
                "pt_BR"
            }
        }
        "zh" => {
            if has("hant") || has("tw") || has("hk") || has("mo") {
                "zh_TW"
            } else {
                "zh_CN"
            }
        }
        l => l,
    };
    LOCALES
        .iter()
        .find(|l| l.eq_ignore_ascii_case(code))
        .copied()
        .unwrap_or("en")
}

/// first non-empty value of LC_ALL, LC_MESSAGES, LANG (via `get`), posix
/// precedence — `C`/`POSIX` there means english. "" when none is set.
pub fn detect(get: impl Fn(&str) -> Option<String>) -> String {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|k| get(k))
        .find(|v| !v.trim().is_empty())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn en() -> Vec<(&'static str, &'static str)> {
        entries("en")
    }

    #[test]
    fn every_locale_has_every_key() {
        let keys: BTreeSet<&str> = en().into_iter().map(|(k, _)| k).collect();
        assert!(keys.len() > 200);
        for lc in LOCALES {
            let got: BTreeSet<&str> = entries(lc).into_iter().map(|(k, _)| k).collect();
            let missing: Vec<_> = keys.difference(&got).collect();
            let extra: Vec<_> = got.difference(&keys).collect();
            assert!(missing.is_empty(), "{lc} missing {missing:?}");
            assert!(extra.is_empty(), "{lc} extra {extra:?}");
        }
    }

    #[test]
    fn placeholder_sets_match_english() {
        let base: HashMap<&str, &str> = en().into_iter().collect();
        for lc in LOCALES {
            for (k, v) in entries(lc) {
                let mut a = placeholders(base[k]);
                let mut b = placeholders(v);
                a.sort_unstable();
                b.sort_unstable();
                assert_eq!(a, b, "{lc} {k}: {v}");
                assert!(!v.trim().is_empty(), "{lc} {k} empty");
            }
        }
    }

    #[test]
    fn translations_are_not_raw_keys_or_wrapped() {
        for lc in LOCALES {
            for (k, v) in entries(lc) {
                assert_ne!(v, k, "{lc} {k} renders raw");
                assert_eq!(v, v.trim_matches([' ', '\t']), "{lc} {k} has edge spaces");
            }
        }
    }

    #[test]
    fn every_key_used_in_source_exists() {
        // scan both crates' sources for tr("…") / trf("…") literals
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let known: BTreeSet<&str> = en().into_iter().map(|(k, _)| k).collect();
        let areas: BTreeSet<&str> = known.iter().filter_map(|k| k.split('.').next()).collect();
        let mut used = BTreeSet::new();
        for dir in ["core/src", "tui/src"] {
            for f in std::fs::read_dir(root.join(dir)).unwrap().flatten() {
                let p = f.path();
                if p.extension().is_none_or(|e| e != "rs") || p.ends_with("i18n.rs") {
                    continue;
                }
                let src = std::fs::read_to_string(&p).unwrap();
                // every quoted `area.name` literal whose area is a catalog area —
                // catches keys passed bare to helpers (`hint("jk", "draw.h_move")`)
                for part in src.split('"').skip(1).step_by(2) {
                    let Some((area, name)) = part.split_once('.') else {
                        continue;
                    };
                    if areas.contains(area)
                        && !name.is_empty()
                        && name
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                    {
                        used.insert(part.to_string());
                    }
                }
            }
        }
        assert!(used.len() > 100, "scan found {}", used.len());
        for k in &used {
            assert!(known.contains(k.as_str()), "key {k} not in en.txt");
        }
        // and nothing in the catalog is dead
        let unused: Vec<_> = known.iter().filter(|k| !used.contains(**k)).collect();
        assert!(unused.is_empty(), "dead keys: {unused:?}");
    }

    #[test]
    fn unknown_key_is_empty_not_the_key() {
        assert_eq!(tr("no.such.key"), "");
        assert_eq!(trf("no.such.key", &[]), "");
    }

    #[test]
    fn locale_mapping() {
        for (raw, want) in [
            ("pt_BR.UTF-8", "pt_BR"),
            ("pt", "pt_BR"),
            ("pt_PT", "pt_PT"),
            ("pt-PT", "pt_PT"),
            ("zh_TW.UTF-8", "zh_TW"),
            ("zh_Hant", "zh_TW"),
            ("zh-Hant-HK", "zh_TW"),
            ("zh_CN", "zh_CN"),
            ("zh-Hans-CN", "zh_CN"),
            ("zh", "zh_CN"),
            ("nb_NO.UTF-8", "no"),
            ("nn", "no"),
            ("fil_PH", "tl"),
            ("iw", "he"),
            ("de_DE@euro", "de"),
            ("ja-JP", "ja"),
            ("C", "en"),
            ("POSIX", "en"),
            ("", "en"),
            ("xx_YY", "en"),
        ] {
            assert_eq!(resolve(raw), want, "{raw}");
        }
    }

    #[test]
    fn detect_follows_posix_precedence() {
        let env = |pairs: &'static [(&str, &str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert_eq!(detect(env(&[("LANG", "de_DE.UTF-8")])), "de_DE.UTF-8");
        assert_eq!(
            detect(env(&[("LC_ALL", "fr_FR"), ("LANG", "de_DE")])),
            "fr_FR"
        );
        assert_eq!(
            detect(env(&[("LC_MESSAGES", "ja_JP"), ("LANG", "de_DE")])),
            "ja_JP"
        );
        assert_eq!(detect(env(&[("LC_ALL", ""), ("LANG", "es_ES")])), "es_ES");
        assert_eq!(
            resolve(&detect(env(&[("LC_ALL", "C"), ("LANG", "de")]))),
            "en"
        );
        assert_eq!(detect(env(&[])), "");
    }

    #[test]
    fn override_wins_and_auto_uses_detected() {
        assert_eq!(pick("de", "ja_JP"), "de");
        assert_eq!(pick("auto", "ja_JP.UTF-8"), "ja");
        assert_eq!(pick("AUTO", "ja"), "ja");
        assert_eq!(pick("", "zh_TW"), "zh_TW");
        assert_eq!(pick("pt_PT", "pt_BR"), "pt_PT");
        assert_eq!(pick("klingon", "ja"), "en");
        assert_eq!(pick("auto", ""), "en");
    }

    #[test]
    fn fill_substitutes_and_keeps_unknown() {
        assert_eq!(fill("a {x} b {y}", &[("x", "1")]), "a 1 b {y}");
        assert_eq!(fill("{x}{x}", &[("x", "é")]), "éé");
        assert_eq!(fill("open { brace", &[]), "open { brace");
    }
}
