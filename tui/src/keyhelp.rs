//! the `?` list: every key the chat view answers, with a plain line each.
//!
//! the footer has room for the essentials only, so before this a key like `T`
//! (move the tab bar) lived in the readme and nowhere a person would look. this
//! table is the one place a key's meaning is written: the `?` screen draws it,
//! and the tests below fail when `normal_key` handles a key the table doesn't
//! name, or when the readme's keys line leaves one out.

use crate::palette;
use heatsync_core::i18n::tr;
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// (key as typed, locale key for what it does). one row per action; aliases
/// are named in the row's words, never as rows of their own.
pub(crate) const NORMAL: &[(&str, &str)] = &[
    ("j k", "keys.chan"),
    ("alt+1-9", "keys.jump"),
    ("i", "keys.say"),
    ("o", "keys.pick"),
    ("s", "keys.keep"),
    ("x", "keys.close"),
    ("m", "keys.manage"),
    ("v", "keys.cursor"),
    ("enter", "keys.enter"),
    ("T", "keys.tabbar"),
    ("t", "keys.stamps"),
    ("B", "keys.big"),
    ("P", "keys.pin"),
    ("space", "keys.pause"),
    ("?", "keys.keys"),
    ("q", "keys.quit"),
];

/// widest key column, so the words line up.
fn key_w() -> usize {
    NORMAL.iter().map(|(k, _)| k.width()).max().unwrap_or(0)
}

/// `text` broken at spaces into pieces at most `w` columns wide (a word wider
/// than `w` is cut). CJK and thai have no spaces between words, so a piece can
/// also break between any two characters once it is full.
fn wrap(text: &str, w: usize) -> Vec<String> {
    let w = w.max(1);
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in text.split(' ') {
        let sep = usize::from(!cur.is_empty());
        if cur.width() + sep + word.width() <= w {
            if sep == 1 {
                cur.push(' ');
            }
            cur.push_str(word);
            continue;
        }
        if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        for ch in word.chars() {
            if cur.width() + ch.width().unwrap_or(0) > w {
                out.push(std::mem::take(&mut cur));
            }
            cur.push(ch);
        }
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

/// every line of the screen at `width` columns: the title, one or more lines
/// per key (the words wrap under themselves), then the note, one line per
/// `·`-separated part.
fn all_lines(width: u16) -> Vec<Line<'static>> {
    let kw = key_w();
    let indent = 2 + kw + 2;
    let words_w = (width as usize).saturating_sub(indent).max(8);
    let mut all = vec![
        Line::from(Span::styled(
            format!(" {} ", tr("keys.title")),
            palette::TAG,
        )),
        Line::raw(""),
    ];
    for (k, d) in NORMAL {
        for (i, piece) in wrap(tr(d), words_w).into_iter().enumerate() {
            let key = if i == 0 {
                format!("  {k}{}  ", " ".repeat(kw - k.width()))
            } else {
                " ".repeat(indent)
            };
            all.push(Line::from(vec![
                Span::styled(key, palette::HEAD),
                Span::raw(piece),
            ]));
        }
    }
    all.push(Line::raw(""));
    let note_w = (width as usize).saturating_sub(2).max(8);
    for part in tr("keys.note").split(" · ") {
        for piece in wrap(part, note_w) {
            all.push(Line::from(Span::styled(format!("  {piece}"), palette::DIM)));
        }
    }
    all
}

/// the screen's lines at `width` columns, from `scroll` down, at most `height` of them.
pub(crate) fn lines(width: u16, scroll: usize, height: u16) -> Vec<Line<'static>> {
    all_lines(width)
        .into_iter()
        .skip(scroll)
        .take(height as usize)
        .collect()
}

/// how far the list can scroll in a pane `width` x `height`.
pub(crate) fn max_scroll(width: u16, height: u16) -> usize {
    all_lines(width).len().saturating_sub(height as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// the chars `normal_key` matches with `KeyCode::Char('…')`, read from its source.
    fn handled_chars() -> BTreeSet<char> {
        let src = include_str!("./input.rs");
        let start = src.find("fn normal_key(").expect("normal_key in input.rs");
        // the function ends at the first closing brace in column 0
        let end = start + src[start..].find("\n}\n").expect("normal_key ends");
        let body = &src[start..end];
        let mut out = BTreeSet::new();
        let mut rest = body;
        while let Some(i) = rest.find("KeyCode::Char('") {
            let tail = &rest[i + "KeyCode::Char('".len()..];
            if let Some(c) = tail.chars().next() {
                out.insert(c);
            }
            rest = tail;
        }
        out
    }

    /// keys a row names in its words instead of as a row: h l (= j k), a (= i).
    const ALIASES: &[char] = &['h', 'l', 'a'];

    fn listed() -> BTreeSet<String> {
        NORMAL
            .iter()
            .flat_map(|(k, _)| k.split(' ').map(str::to_string))
            .collect()
    }

    #[test]
    fn every_key_the_chat_view_answers_is_in_the_list() {
        let listed = listed();
        let missing: Vec<char> = handled_chars()
            .into_iter()
            .filter(|c| !ALIASES.contains(c))
            .filter(|c| {
                let name = if *c == ' ' {
                    "space".to_string()
                } else {
                    c.to_string()
                };
                !listed.contains(&name)
            })
            .collect();
        assert!(
            missing.is_empty(),
            "normal_key handles {missing:?} but the ? list does not name them"
        );
        // the scan must see the handler, or this test proves nothing
        assert!(handled_chars().len() >= 12);
    }

    #[test]
    fn the_list_names_no_key_the_chat_view_ignores() {
        let handled = handled_chars();
        for (k, _) in NORMAL {
            for part in k.split(' ') {
                let ok = match part {
                    "space" => handled.contains(&' '),
                    "enter" => include_str!("./input.rs")
                        .contains("KeyCode::Enter => {\n            special::enter"),
                    "alt+1-9" => true,
                    p if p.chars().count() == 1 => handled.contains(&p.chars().next().unwrap()),
                    other => panic!("unknown key spelling {other}"),
                };
                assert!(
                    ok,
                    "the ? list names `{part}` but normal_key does not answer it"
                );
            }
        }
    }

    #[test]
    fn the_readme_keys_line_names_every_key() {
        let readme = include_str!("../../README.md");
        for (k, _) in NORMAL {
            let spelled = match *k {
                "j k" => "`j`/`k`",
                "alt+1-9" => "`alt+1`..`alt+9`",
                other => other,
            };
            let needle = if spelled.starts_with('`') {
                spelled.to_string()
            } else {
                format!("`{spelled}`")
            };
            assert!(readme.contains(&needle), "README.md never names {needle}");
        }
    }

    #[test]
    fn every_row_draws_and_the_list_scrolls_to_its_end() {
        let all = lines(200, 0, u16::MAX);
        for (k, _) in NORMAL {
            assert!(
                all.iter()
                    .any(|l| l.spans.first().is_some_and(|s| s.content.trim() == *k)),
                "row {k} not drawn"
            );
        }
        // a short pane reaches the note at its last scroll
        let h = 6;
        let last = lines(200, max_scroll(200, h), h);
        assert_eq!(last.len(), h as usize);
        let tail = tr("keys.note").rsplit(" · ").next().unwrap();
        assert!(last.last().unwrap().spans[0].content.contains(tail));
        assert_eq!(max_scroll(200, 200), 0);
    }

    #[test]
    fn a_phone_width_terminal_wraps_instead_of_cutting() {
        for w in [34u16, 40, 60] {
            let all = lines(w, 0, u16::MAX);
            for l in &all {
                let cols: usize = l.spans.iter().map(|s| s.content.width()).sum();
                assert!(cols <= w as usize, "{cols} > {w}: {l:?}");
            }
            // every word of every row is still on screen somewhere
            let text: String = all
                .iter()
                .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
                .collect::<Vec<_>>()
                .join(" ");
            for (_, d) in NORMAL {
                for word in tr(d).split(' ') {
                    assert!(text.contains(word), "{w} cols lost {word:?}");
                }
            }
        }
    }

    #[test]
    fn wrap_breaks_at_spaces_and_cuts_only_a_word_too_wide() {
        assert_eq!(wrap("move the tab bar", 8), ["move the", "tab bar"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("", 4), [""]);
        // double-width characters count as two columns
        assert_eq!(wrap("表示切替", 4), ["表示", "切替"]);
    }
}
