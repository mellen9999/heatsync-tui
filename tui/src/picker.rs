//! the live-channel picker behind `o`: who's live now, filter by typing, enter
//! to join. state, filtering, keys and row layout live here (pure, tested); the
//! fetch runs on a thread and main.rs just wires the pieces into the app.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use heatsync_core::Platform;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

use crate::{hsauth, http, palette};

/// a fetched list is reused for this long when `o` is pressed again.
pub const CACHE_FOR: Duration = Duration::from_secs(30);
/// how many of the busiest streams the top section carries.
const TOP_LIMIT: u32 = 100;
/// the server's batch live check takes at most this many names per platform.
const STATUS_MAX: usize = 20;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Section {
    Following,
    Mine,
    Top,
}

impl Section {
    fn label(self) -> &'static str {
        match self {
            Section::Following => "following",
            Section::Mine => "your channels",
            Section::Top => "top live",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    pub section: Section,
    pub platform: Platform,
    /// twitch/kick login, or the youtube video id (what chat subscribes by).
    pub name: String,
    pub display: String,
    pub viewers: Option<u64>,
    pub info: String,
}

impl Entry {
    /// the tab spec that joins it: `name`, `kick:name`, `yt:id`.
    pub fn spec(&self) -> String {
        spec_of(self.platform, &self.name)
    }
}

pub fn spec_of(p: Platform, name: &str) -> String {
    match p {
        Platform::Twitch => name.to_string(),
        Platform::Kick => format!("kick:{name}"),
        Platform::Youtube => format!("yt:{name}"),
    }
}

/// one fetch: your live channels first, then the busiest. the following section
/// is absent until the server offers follows to a cli token.
#[derive(Clone, Debug, Default)]
pub struct Fetched {
    pub entries: Vec<Entry>,
    /// set when the main list failed — shown as one line, literal entry still works.
    pub error: Option<String>,
}

fn plat(s: &str) -> Option<Platform> {
    match s {
        "twitch" => Some(Platform::Twitch),
        "kick" => Some(Platform::Kick),
        "youtube" => Some(Platform::Youtube),
        _ => None,
    }
}

fn entry_of(s: &http::LiveStream) -> Option<Entry> {
    let platform = plat(&s.platform)?;
    let login = s
        .platform_usernames
        .get(&s.platform)
        .unwrap_or(&s.username)
        .to_lowercase();
    let name = match platform {
        Platform::Youtube => s.video_id.clone().filter(|v| !v.is_empty())?,
        _ if login.is_empty() => return None,
        _ => login,
    };
    let info = match (s.category.is_empty(), s.title.is_empty()) {
        (false, false) => format!("{} · {}", s.category, s.title),
        (false, true) => s.category.clone(),
        _ => s.title.clone(),
    };
    Some(Entry {
        section: Section::Top,
        platform,
        name,
        display: if s.username.is_empty() {
            s.platform.clone()
        } else {
            s.username.clone()
        },
        viewers: Some(s.viewer_count),
        info: info.split_whitespace().collect::<Vec<_>>().join(" "),
    })
}

/// blocking — run it on a thread. `mine` is every twitch/kick source in the
/// open tabs; the ones live now become the "your channels" section. `hs` is the
/// heatsync login, when there is one, for the "following" section.
pub fn fetch(mine: &[(Platform, String)], hs: Option<&hsauth::Client>) -> Fetched {
    let top = http::live_top(TOP_LIMIT);
    let pick = |p: Platform| -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        for (q, n) in mine {
            let n = n.to_lowercase();
            if *q == p && !v.contains(&n) && v.len() < STATUS_MAX {
                v.push(n);
            }
        }
        v
    };
    let (tw, kk) = (pick(Platform::Twitch), pick(Platform::Kick));
    let live = http::live_status(&tw, &kk);
    let following = hs.and_then(|c| c.following()).map(|v| {
        v.into_iter()
            .filter_map(|s| serde_json::from_value(s).ok())
            .collect()
    });
    assemble(top, live, following)
}

/// the three answers → one ordered list: following, your channels, top live.
/// a stream shows once, in the first section that has it. no top answer is an
/// error line (the list is useless without it); the other two just go missing.
pub fn assemble(
    top: Option<Vec<http::LiveStream>>,
    live: Option<(Vec<String>, Vec<String>)>,
    following: Option<Vec<http::LiveStream>>,
) -> Fetched {
    let mut out = Fetched::default();
    let Some(top) = top else {
        out.error = Some("couldn't load the live list — type a channel and enter".into());
        return out;
    };
    let pool: Vec<Entry> = top.iter().filter_map(entry_of).collect();
    let add = |out: &mut Fetched, e: Entry| {
        if !out
            .entries
            .iter()
            .any(|m| m.platform == e.platform && m.name == e.name)
        {
            out.entries.push(e);
        }
    };
    for e in following.iter().flatten().filter_map(entry_of) {
        add(
            &mut out,
            Entry {
                section: Section::Following,
                ..e
            },
        );
    }
    if let Some((tw_live, kk_live)) = live {
        let rows = tw_live
            .iter()
            .map(|n| (Platform::Twitch, n))
            .chain(kk_live.iter().map(|n| (Platform::Kick, n)));
        for (platform, n) in rows {
            let n = n.to_lowercase();
            let found = pool.iter().find(|e| e.platform == platform && e.name == n);
            add(
                &mut out,
                match found {
                    Some(e) => Entry {
                        section: Section::Mine,
                        ..e.clone()
                    },
                    None => Entry {
                        section: Section::Mine,
                        platform,
                        display: n.clone(),
                        name: n,
                        viewers: None,
                        info: "live".into(),
                    },
                },
            );
        }
    }
    for e in pool {
        add(&mut out, e);
    }
    out
}

/// case-insensitive subsequence: every char of `q` appears in `hay`, in order.
pub fn fuzzy(q: &str, hay: &str) -> bool {
    let mut h = hay.chars().flat_map(char::to_lowercase);
    q.chars()
        .flat_map(char::to_lowercase)
        .all(|c| h.any(|x| x == c))
}

/// does the text look like something `o` can join literally: a name,
/// `kick:name`, `yt:id`, a youtube url, or a `+` merge of those.
pub fn looks_like_spec(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-:./=?&%+".contains(c))
}

pub enum Action {
    None,
    Close,
    /// join as a new tab (or focus it).
    Join(String),
    /// add as a merged sub of the current tab.
    Merge(String),
}

#[derive(Default)]
pub struct Picker {
    pub filter: String,
    pub cursor: usize,
    pub fetched: Option<Fetched>,
    /// a refresh is in flight (the list shown may be a stale copy, or empty).
    pub loading: bool,
}

impl Picker {
    pub fn new(fetched: Option<Fetched>, loading: bool) -> Picker {
        Picker {
            filter: String::new(),
            cursor: 0,
            fetched,
            loading,
        }
    }

    pub fn set(&mut self, f: Fetched) {
        self.fetched = Some(f);
        self.loading = false;
        self.clamp();
    }

    /// entries passing the filter, in section order (following, mine, top).
    pub fn visible(&self) -> Vec<&Entry> {
        let Some(f) = &self.fetched else {
            return Vec::new();
        };
        let q = self.filter.trim();
        let hit = |e: &&Entry| {
            q.is_empty()
                || fuzzy(q, &e.name)
                || fuzzy(q, &e.display)
                || e.info.to_lowercase().contains(&q.to_lowercase())
        };
        let all = &f.entries;
        [Section::Following, Section::Mine, Section::Top]
            .into_iter()
            .flat_map(|s| all.iter().filter(move |e| e.section == s))
            .filter(hit)
            .collect()
    }

    fn clamp(&mut self) {
        self.cursor = self.cursor.min(self.visible().len().saturating_sub(1));
    }

    fn selected(&self) -> Option<String> {
        self.visible().get(self.cursor).map(|e| e.spec())
    }

    /// what enter joins: the highlighted row, else the typed text if it can be
    /// a channel spec.
    fn target(&self) -> Option<String> {
        self.selected().or_else(|| {
            let t = self.filter.trim();
            looks_like_spec(t).then(|| t.to_string())
        })
    }

    fn step(&mut self, delta: isize) -> Action {
        let n = self.visible().len();
        if n > 0 {
            self.cursor = self.cursor.saturating_add_signed(delta).min(n - 1);
        }
        Action::None
    }

    pub fn key(&mut self, k: KeyEvent) -> Action {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let typing = !self.filter.is_empty();
        match k.code {
            KeyCode::Esc => Action::Close,
            KeyCode::Enter
                if k.modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                self.target().map_or(Action::None, Action::Merge)
            }
            KeyCode::Enter => self.target().map_or(Action::None, Action::Join),
            KeyCode::Down => self.step(1),
            KeyCode::Up => self.step(-1),
            KeyCode::Char('n') if ctrl => self.step(1),
            KeyCode::Char('p') if ctrl => self.step(-1),
            // j/k are LETTERS here (kaicenat, jynxzi…); vim-style moves are
            // ctrl-j/ctrl-k, alongside ctrl-n/ctrl-p, arrows, tab. `+` merges
            // only on an empty filter — mid-text it types a literal merge spec.
            KeyCode::Char('j') if ctrl => self.step(1),
            KeyCode::Char('k') if ctrl => self.step(-1),
            KeyCode::Tab => self.step(1),
            KeyCode::BackTab => self.step(-1),
            KeyCode::Char('+') if !typing => self.selected().map_or(Action::None, Action::Merge),
            KeyCode::Backspace => {
                self.filter.pop();
                self.cursor = 0;
                Action::None
            }
            KeyCode::Char('u') if ctrl => {
                self.filter.clear();
                self.cursor = 0;
                Action::None
            }
            KeyCode::Char(c) if !ctrl && !k.modifiers.contains(KeyModifiers::ALT) => {
                self.filter.push(c);
                self.cursor = 0;
                Action::None
            }
            _ => Action::None,
        }
    }
}

/// 12345 → `12.3k`, 1_250_000 → `1.3m`.
pub fn compact(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=9_999 => format!("{:.1}k", n as f64 / 1e3),
        10_000..=999_999 => format!("{}k", n / 1000),
        _ => format!("{:.1}m", n as f64 / 1e6),
    }
}

fn clip(s: &str, w: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + cw > w {
            if used >= w && w > 0 {
                out.pop();
                out.push('…');
            }
            break;
        }
        out.push(c);
        used += cw;
    }
    out
}

fn tag(p: Platform) -> &'static str {
    match p {
        Platform::Twitch => "[T]",
        Platform::Kick => "[K]",
        Platform::Youtube => "[Y]",
    }
}

fn row(e: &Entry, w: usize, selected: bool) -> Line<'static> {
    const NAME_W: usize = 22;
    let name = clip(&e.display, NAME_W);
    let pad = NAME_W.saturating_sub(unicode_width::UnicodeWidthStr::width(name.as_str()));
    let viewers = e.viewers.map_or("-".to_string(), compact);
    let head = format!(
        " {} {}{} {:>6}  ",
        tag(e.platform),
        name,
        " ".repeat(pad),
        viewers
    );
    let info_w = w.saturating_sub(unicode_width::UnicodeWidthStr::width(head.as_str()));
    let info = clip(&e.info, info_w);
    if selected {
        // reversed, no platform color: a colored fill behind text is mud.
        let line = format!("{head}{info}");
        let pad = w.saturating_sub(unicode_width::UnicodeWidthStr::width(line.as_str()));
        return Line::from(Span::styled(
            format!("{line}{}", " ".repeat(pad)),
            palette::SEL,
        ));
    }
    Line::from(vec![
        Span::raw(" "),
        Span::styled(tag(e.platform), palette::platform(e.platform)),
        Span::styled(
            format!(" {}{} {:>6}  ", name, " ".repeat(pad), viewers),
            palette::TEXT,
        ),
        Span::styled(info, palette::DIM),
    ])
}

impl Picker {
    /// the prompt line + as many list rows as fit in `h`, scrolled to keep the
    /// cursor on screen. section headers are bold; the cursor row is reversed.
    pub fn lines(&self, w: u16, h: u16) -> Vec<Line<'static>> {
        let w = w as usize;
        let mut head = vec![
            Span::styled(" pick ", palette::TAG),
            Span::styled(" ❯ ", palette::TEXT),
            Span::styled(self.filter.clone(), palette::TEXT),
            Span::styled("\u{2588}", palette::TEXT),
        ];
        if self.loading {
            head.push(Span::styled("   loading…", palette::DIM));
        }
        let mut out = vec![Line::from(head)];
        if let Some(err) = self.fetched.as_ref().and_then(|f| f.error.as_ref()) {
            out.push(Line::from(Span::styled(format!(" {err}"), palette::WARN)));
        }
        let vis = self.visible();
        let mut body: Vec<Line<'static>> = Vec::new();
        let mut cursor_at = 0;
        let mut last: Option<Section> = None;
        for (i, e) in vis.iter().enumerate() {
            if last != Some(e.section) {
                body.push(Line::from(Span::styled(
                    format!(" {}", e.section.label()),
                    Style::new().add_modifier(Modifier::BOLD),
                )));
                last = Some(e.section);
            }
            if i == self.cursor {
                cursor_at = body.len();
            }
            body.push(row(e, w, i == self.cursor));
        }
        if vis.is_empty() && self.fetched.is_some() {
            let t = self.filter.trim();
            let msg = if looks_like_spec(t) {
                format!(" no match — enter joins \"{t}\"")
            } else {
                " no match".to_string()
            };
            body.push(Line::from(Span::styled(msg, palette::DIM)));
        }
        let room = (h as usize).saturating_sub(out.len());
        let start = if cursor_at < room {
            0
        } else {
            cursor_at + 1 - room
        };
        // keep the section header glued to the first row after a scroll.
        out.extend(body.into_iter().skip(start).take(room));
        out
    }
}

/// a cached fetch and when it landed.
pub struct Cache(pub Option<(Instant, Fetched)>);

impl Cache {
    pub fn fresh(&self) -> Option<&Fetched> {
        self.0
            .as_ref()
            .filter(|(t, _)| t.elapsed() < CACHE_FOR)
            .map(|(_, f)| f)
    }
    pub fn any(&self) -> Option<&Fetched> {
        self.0.as_ref().map(|(_, f)| f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(section: Section, platform: Platform, name: &str, v: u64, info: &str) -> Entry {
        Entry {
            section,
            platform,
            name: name.into(),
            display: name.into(),
            viewers: Some(v),
            info: info.into(),
        }
    }

    fn picker() -> Picker {
        let entries = vec![
            e(
                Section::Mine,
                Platform::Twitch,
                "xqc",
                20_000,
                "just chatting",
            ),
            e(
                Section::Top,
                Platform::Kick,
                "absi",
                124_809,
                "kings league",
            ),
            e(Section::Top, Platform::Twitch, "kaicenat", 90_000, "irl"),
            e(Section::Top, Platform::Youtube, "VID123", 5_000, "lofi"),
        ];
        Picker::new(
            Some(Fetched {
                entries,
                error: None,
            }),
            false,
        )
    }

    fn k(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn type_str(p: &mut Picker, s: &str) {
        for c in s.chars() {
            p.key(k(KeyCode::Char(c)));
        }
    }

    #[test]
    fn fuzzy_is_a_case_insensitive_subsequence() {
        assert!(fuzzy("kc", "KaiCenat"));
        assert!(fuzzy("", "x"));
        assert!(!fuzzy("ck", "kaicenat"));
        assert!(!fuzzy("zz", "xqc"));
    }

    #[test]
    fn mine_comes_before_top_whatever_the_input_order() {
        let mut f = picker().fetched.unwrap();
        f.entries.reverse();
        let p = Picker::new(Some(f), false);
        let secs: Vec<_> = p.visible().iter().map(|e| e.section).collect();
        assert_eq!(secs[0], Section::Mine);
        assert!(secs[1..].iter().all(|s| *s == Section::Top));
    }

    fn ls(user: &str, platform: &str, v: u64) -> http::LiveStream {
        serde_json::from_value(serde_json::json!({
            "username": user, "platform": platform, "viewerCount": v, "category": "c", "title": "t",
        }))
        .unwrap()
    }

    #[test]
    fn assemble_orders_following_then_mine_then_top_and_shows_each_once() {
        let top = vec![
            ls("big", "twitch", 9),
            ls("xqc", "twitch", 5),
            ls("fol", "kick", 4),
        ];
        let f = assemble(
            Some(top),
            Some((vec!["xqc".into(), "quiet".into()], vec![])),
            Some(vec![ls("fol", "kick", 4)]),
        );
        let got: Vec<(Section, &str)> = f
            .entries
            .iter()
            .map(|e| (e.section, e.name.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                (Section::Following, "fol"),
                (Section::Mine, "xqc"),
                (Section::Mine, "quiet"),
                (Section::Top, "big"),
            ]
        );
        assert_eq!(f.entries[1].viewers, Some(5));
        assert_eq!(f.entries[2].viewers, None);
        assert!(f.error.is_none());
    }

    #[test]
    fn no_login_means_no_following_section_and_no_top_means_an_error_line() {
        let f = assemble(Some(vec![ls("big", "twitch", 9)]), None, None);
        assert!(f.entries.iter().all(|e| e.section == Section::Top));
        let f = assemble(None, None, Some(vec![ls("fol", "kick", 4)]));
        assert!(f.entries.is_empty() && f.error.is_some());
    }

    #[test]
    fn following_rows_lead_the_visible_list() {
        let f = assemble(
            Some(vec![ls("big", "twitch", 9)]),
            None,
            Some(vec![ls("fol", "kick", 4)]),
        );
        let p = Picker::new(Some(f), false);
        assert_eq!(p.visible()[0].name, "fol");
        let text: String = p.lines(60, 10)[1]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("following"));
    }

    #[test]
    fn filter_narrows_and_resets_the_cursor() {
        let mut p = picker();
        p.key(k(KeyCode::Down));
        type_str(&mut p, "kc");
        let names: Vec<_> = p.visible().iter().map(|e| e.name.clone()).collect();
        assert_eq!(names, vec!["kaicenat"]);
        assert_eq!(p.cursor, 0);
    }

    #[test]
    fn enter_joins_the_selected_row_with_its_platform_prefix() {
        let mut p = picker();
        p.key(k(KeyCode::Down));
        match p.key(k(KeyCode::Enter)) {
            Action::Join(s) => assert_eq!(s, "kick:absi"),
            _ => panic!("expected join"),
        }
        let mut p = picker();
        type_str(&mut p, "vid");
        match p.key(k(KeyCode::Enter)) {
            Action::Join(s) => assert_eq!(s, "yt:VID123"),
            _ => panic!("expected join"),
        }
    }

    #[test]
    fn unmatched_spec_text_joins_literally() {
        let mut p = picker();
        type_str(&mut p, "kick:someone_new");
        assert!(p.visible().is_empty());
        match p.key(k(KeyCode::Enter)) {
            Action::Join(s) => assert_eq!(s, "kick:someone_new"),
            _ => panic!("expected literal join"),
        }
        let mut p = picker();
        type_str(&mut p, "a b!");
        assert!(matches!(p.key(k(KeyCode::Enter)), Action::None));
    }

    #[test]
    fn spec_shapes() {
        for ok in [
            "xqc",
            "kick:xqc",
            "yt:abc-_1",
            "https://youtu.be/abc?t=3",
            "a+kick:a",
        ] {
            assert!(looks_like_spec(ok), "{ok}");
        }
        for bad in ["", "two words", "bad!", "ünï"] {
            assert!(!looks_like_spec(bad), "{bad}");
        }
    }

    #[test]
    fn jk_type_letters_ctrl_jk_move() {
        let mut p = picker();
        type_str(&mut p, "k");
        assert_eq!(p.filter, "k");
        p.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        p.key(ctrl('j'));
        assert_eq!(p.cursor, 1);
        p.key(ctrl('k'));
        assert_eq!(p.cursor, 0);
        p.key(k(KeyCode::Tab));
        assert_eq!(p.cursor, 1);
        p.key(k(KeyCode::BackTab));
        assert_eq!(p.cursor, 0);
    }

    #[test]
    fn arrows_and_ctrl_np_clamp_at_the_ends() {
        let mut p = picker();
        p.key(k(KeyCode::Up));
        assert_eq!(p.cursor, 0);
        for _ in 0..10 {
            p.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
        }
        assert_eq!(p.cursor, 3);
        p.key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert_eq!(p.cursor, 2);
    }

    #[test]
    fn plus_and_shift_enter_merge_esc_closes() {
        let mut p = picker();
        match p.key(k(KeyCode::Char('+'))) {
            Action::Merge(s) => assert_eq!(s, "xqc"),
            _ => panic!("expected merge"),
        }
        match p.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)) {
            Action::Merge(s) => assert_eq!(s, "xqc"),
            _ => panic!("expected merge"),
        }
        assert!(matches!(p.key(k(KeyCode::Esc)), Action::Close));
        // mid-filter '+' is just text (a literal merge spec).
        let mut p = picker();
        type_str(&mut p, "a+");
        assert_eq!(p.filter, "a+");
    }

    #[test]
    fn backspace_and_ctrl_u_edit_the_filter() {
        let mut p = picker();
        type_str(&mut p, "xq");
        p.key(k(KeyCode::Backspace));
        assert_eq!(p.filter, "x");
        p.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(p.filter.is_empty());
    }

    #[test]
    fn viewers_read_compact() {
        assert_eq!(compact(999), "999");
        assert_eq!(compact(12_345), "12k");
        assert_eq!(compact(1_234), "1.2k");
        assert_eq!(compact(1_260_000), "1.3m");
    }

    #[test]
    fn rows_fit_the_width_and_headers_lead_each_section() {
        let p = picker();
        let lines = p.lines(60, 20);
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(text[1].contains("your channels"));
        assert!(text.iter().any(|t| t.contains("top live")));
        assert!(text
            .iter()
            .all(|t| unicode_width::UnicodeWidthStr::width(t.as_str()) <= 60));
        assert!(text[2].contains("[T]") && text[2].contains("xqc") && text[2].contains("20k"));
    }

    #[test]
    fn the_list_scrolls_with_the_cursor() {
        let mut p = picker();
        for _ in 0..3 {
            p.key(k(KeyCode::Down));
        }
        let lines = p.lines(60, 4);
        assert!(lines.len() <= 4);
        let last: String = lines
            .last()
            .unwrap()
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(last.contains("VID123"));
    }

    #[test]
    fn the_cache_expires() {
        let c = Cache(Some((Instant::now(), Fetched::default())));
        assert!(c.fresh().is_some());
        let old = Instant::now() - CACHE_FOR - Duration::from_secs(1);
        let c = Cache(Some((old, Fetched::default())));
        assert!(c.fresh().is_none() && c.any().is_some());
    }
}
