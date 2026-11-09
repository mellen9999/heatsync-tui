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

/// pick a locale by code, os-locale string or "auto"/"" (= `detect`'s answer).
/// unknown → english.
pub fn set(code: &str, detected: &str) {
    let want = if code.trim().is_empty() || code.trim().eq_ignore_ascii_case("auto") {
        detected
    } else {
        code
    };
    let lc = resolve(want);
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
