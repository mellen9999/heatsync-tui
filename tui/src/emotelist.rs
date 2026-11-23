//! `/emotes`: your own heatsync emotes in a scrollable list, the same shape as
//! the channel picker. j/k move, ctrl-d/ctrl-u half pages, g/G ends, `/` filters
//! by name, esc or q closes. the fetch runs on a thread and lands through
//! [`State`], drained once per tick; nothing here touches the network.

use std::sync::mpsc::{channel, Receiver, Sender};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::inventory::Slot;
use crate::palette;
use heatsync_core::i18n::{tr, trf};

pub type Fetched = Result<Vec<Slot>, String>;

pub enum Action {
    None,
    Close,
}

#[derive(Default)]
pub struct EmoteList {
    rows: Vec<Slot>,
    cursor: usize,
    filter: String,
    /// typing after `/`: keys edit the filter instead of moving.
    typing: bool,
    loading: bool,
    error: Option<String>,
}

impl EmoteList {
    fn new() -> EmoteList {
        EmoteList {
            loading: true,
            ..EmoteList::default()
        }
    }

    pub fn set(&mut self, f: Fetched) {
        self.loading = false;
        match f {
            Ok(rows) => {
                self.rows = rows;
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
        self.clamp();
    }

    /// rows passing the filter (case-insensitive substring on the name).
    pub fn visible(&self) -> Vec<&Slot> {
        let q = self.filter.trim().to_lowercase();
        self.rows
            .iter()
            .filter(|s| q.is_empty() || s.name.to_lowercase().contains(&q))
            .collect()
    }

    fn clamp(&mut self) {
        self.cursor = self.cursor.min(self.visible().len().saturating_sub(1));
    }

    fn step(&mut self, delta: isize) {
        let n = self.visible().len();
        if n > 0 {
            self.cursor = self.cursor.saturating_add_signed(delta).min(n - 1);
        }
    }

    pub fn key(&mut self, k: KeyEvent, half: usize) -> Action {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if self.typing {
            match k.code {
                KeyCode::Esc => {
                    self.typing = false;
                    self.filter.clear();
                }
                KeyCode::Enter => self.typing = false,
                KeyCode::Backspace => {
                    self.filter.pop();
                }
                KeyCode::Char('u') if ctrl => self.filter.clear(),
                KeyCode::Char(c) if !ctrl => self.filter.push(c),
                _ => {}
            }
            self.cursor = 0;
            return Action::None;
        }
        let end = self.visible().len().saturating_sub(1);
        match k.code {
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.cursor = 0;
            }
            KeyCode::Esc | KeyCode::Char('q') => return Action::Close,
            KeyCode::Char('j') | KeyCode::Down => self.step(1),
            KeyCode::Char('k') | KeyCode::Up => self.step(-1),
            KeyCode::Char('d') if ctrl => self.step(half as isize),
            KeyCode::Char('u') if ctrl => self.step(-(half as isize)),
            KeyCode::Char('g') | KeyCode::Home => self.cursor = 0,
            KeyCode::Char('G') | KeyCode::End => self.cursor = end,
            KeyCode::Char('/') => self.typing = true,
            _ => {}
        }
        Action::None
    }

    /// the header line + as many rows as fit in `h`, scrolled to keep the
    /// cursor on screen; the cursor row is reversed.
    pub fn lines(&self, w: u16, h: u16) -> Vec<Line<'static>> {
        let w = w as usize;
        let n = self.visible().len();
        let mut head = vec![Span::styled(
            format!(
                " {} ",
                trf(
                    "inventory.list_head",
                    &[("n", &self.rows.len().to_string())]
                )
            ),
            palette::TAG,
        )];
        if self.typing || !self.filter.is_empty() {
            head.push(Span::styled(" /", palette::TEXT));
            head.push(Span::styled(self.filter.clone(), palette::TEXT));
            if self.typing {
                head.push(Span::styled("\u{2588}", palette::TEXT));
            }
        }
        if self.loading {
            head.push(Span::styled(
                format!("   {}", tr("inventory.loading")),
                palette::DIM,
            ));
        }
        let mut out = vec![Line::from(head)];
        if let Some(e) = &self.error {
            out.push(Line::from(Span::styled(format!(" {e}"), palette::WARN)));
        }
        if n == 0 && !self.loading && self.error.is_none() {
            let msg = if self.rows.is_empty() {
                tr("inventory.list_empty")
            } else {
                tr("inventory.list_no_match")
            };
            out.push(Line::from(Span::styled(format!(" {msg}"), palette::DIM)));
        }
        let room = (h as usize).saturating_sub(out.len());
        let start = if self.cursor < room {
            0
        } else {
            self.cursor + 1 - room
        };
        for (i, s) in self
            .visible()
            .into_iter()
            .enumerate()
            .skip(start)
            .take(room)
        {
            out.push(row(s, w, i == self.cursor));
        }
        out
    }
}

fn row(s: &Slot, w: usize, selected: bool) -> Line<'static> {
    const NAME_W: usize = 28;
    // cut by what the terminal shows (a wide character is two cells), not by count
    let mut name = String::new();
    for c in s.name.chars() {
        if name.width() + c.width().unwrap_or(0) > NAME_W {
            break;
        }
        name.push(c);
    }
    let pad = NAME_W.saturating_sub(name.width());
    let head = format!(" {name}{}  ", " ".repeat(pad));
    if selected {
        let line = format!("{head}{}", s.source);
        let fill = w.saturating_sub(line.width());
        return Line::from(Span::styled(
            format!("{line}{}", " ".repeat(fill)),
            palette::SEL,
        ));
    }
    Line::from(vec![
        Span::styled(head, palette::TEXT),
        Span::styled(s.source.clone(), palette::DIM),
    ])
}

/// the open list (if any) and the thread's way back.
pub struct State {
    pub open: Option<EmoteList>,
    tx: Sender<(u64, Fetched)>,
    rx: Receiver<(u64, Fetched)>,
    /// which `start` the open list is waiting on: an older, slower fetch that
    /// lands after a newer one is stale and dropped.
    gen: u64,
}

impl State {
    pub fn new() -> State {
        let (tx, rx) = channel();
        State {
            open: None,
            tx,
            rx,
            gen: 0,
        }
    }

    /// show the list (loading) and fetch it off-thread. a second `/emotes`
    /// while one is open just fetches again.
    pub fn start(&mut self, fetch: impl FnOnce() -> Fetched + Send + 'static) {
        self.open = Some(EmoteList::new());
        self.gen += 1;
        let (tx, gen) = (self.tx.clone(), self.gen);
        std::thread::spawn(move || {
            let _ = tx.send((gen, fetch()));
        });
    }

    /// land finished fetches into the open list; one that arrives after the
    /// list closed is dropped.
    pub fn drain(&mut self) {
        while let Ok((gen, f)) = self.rx.try_recv() {
            if gen != self.gen {
                continue;
            }
            if let Some(l) = &mut self.open {
                l.set(f);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(i: u32, n: &str) -> Slot {
        Slot {
            slot: i,
            name: n.into(),
            source: "7tv".into(),
        }
    }

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn list(n: u32) -> EmoteList {
        let mut l = EmoteList::new();
        l.set(Ok((0..n).map(|i| slot(i, &format!("emote{i}"))).collect()));
        l
    }

    fn text(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn vim_keys_move_and_clamp() {
        let mut l = list(20);
        l.key(key(KeyCode::Char('j')), 5);
        l.key(key(KeyCode::Char('j')), 5);
        assert_eq!(l.cursor, 2);
        l.key(key(KeyCode::Char('k')), 5);
        assert_eq!(l.cursor, 1);
        l.key(ctrl('d'), 5);
        assert_eq!(l.cursor, 6);
        l.key(ctrl('u'), 5);
        l.key(ctrl('u'), 5);
        assert_eq!(l.cursor, 0);
        l.key(key(KeyCode::Char('G')), 5);
        assert_eq!(l.cursor, 19);
        l.key(key(KeyCode::Char('j')), 5);
        assert_eq!(l.cursor, 19);
        l.key(key(KeyCode::Char('g')), 5);
        assert_eq!(l.cursor, 0);
    }

    #[test]
    fn esc_and_q_close_but_esc_first_clears_a_filter() {
        let mut l = list(3);
        assert!(matches!(l.key(key(KeyCode::Char('q')), 5), Action::Close));
        assert!(matches!(l.key(key(KeyCode::Esc), 5), Action::Close));
        l.key(key(KeyCode::Char('/')), 5);
        l.key(key(KeyCode::Char('2')), 5);
        l.key(key(KeyCode::Enter), 5);
        assert_eq!(l.visible().len(), 1);
        assert!(matches!(l.key(key(KeyCode::Esc), 5), Action::None));
        assert_eq!(l.visible().len(), 3);
        assert!(matches!(l.key(key(KeyCode::Esc), 5), Action::Close));
    }

    #[test]
    fn slash_filters_by_name_and_j_k_are_letters_while_typing() {
        let mut l = EmoteList::new();
        l.set(Ok(vec![slot(1, "Kappa"), slot(2, "jk"), slot(3, "Pog")]));
        l.key(key(KeyCode::Char('/')), 5);
        l.key(key(KeyCode::Char('k')), 5);
        assert_eq!(
            l.visible()
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["Kappa", "jk"]
        );
        // `j` typed while filtering is a letter, not a move: "kj" matches nothing
        l.key(key(KeyCode::Char('j')), 5);
        assert!(l.visible().is_empty());
        l.key(key(KeyCode::Backspace), 5);
        l.key(key(KeyCode::Backspace), 5);
        l.key(key(KeyCode::Char('j')), 5);
        l.key(key(KeyCode::Char('k')), 5);
        assert_eq!(
            l.visible()
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["jk"]
        );
        l.key(key(KeyCode::Esc), 5);
        assert_eq!(l.visible().len(), 3);
    }

    #[test]
    fn the_cursor_row_stays_on_screen_and_is_reversed() {
        let mut l = list(50);
        for _ in 0..30 {
            l.key(key(KeyCode::Char('j')), 5);
        }
        let lines = l.lines(60, 10);
        assert!(lines.len() <= 10);
        assert!(text(&lines[0]).contains("your emotes (50)"));
        let last = lines.last().unwrap();
        assert!(text(last).contains("emote30"), "{}", text(last));
        assert!(last.spans[0]
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::REVERSED));
    }

    #[test]
    fn empty_loading_and_errors_are_said() {
        let mut l = EmoteList::new();
        assert!(text(&l.lines(60, 5)[0]).contains("loading"));
        l.set(Ok(vec![]));
        assert!(text(&l.lines(60, 5)[1]).contains("none yet"));
        l.set(Err("can't reach heatsync".into()));
        assert!(text(&l.lines(60, 5)[1]).contains("can't reach heatsync"));
        // a failed refresh keeps what was already shown
        let mut l = list(2);
        l.set(Err("x".into()));
        assert_eq!(l.rows.len(), 2);
    }

    #[test]
    fn a_fetch_lands_in_the_open_list_and_a_late_one_is_dropped() {
        let mut st = State::new();
        st.start(|| Ok(vec![slot(1, "a")]));
        for _ in 0..200 {
            st.drain();
            if !st.open.as_ref().unwrap().loading {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(st.open.as_ref().unwrap().rows.len(), 1);
        st.open = None;
        st.tx.send((st.gen, Ok(vec![slot(2, "b")]))).unwrap();
        st.drain();
        assert!(st.open.is_none());
    }

    #[test]
    fn an_older_fetch_landing_late_does_not_overwrite_a_newer_one() {
        let mut st = State::new();
        st.start(|| Ok(vec![slot(1, "old")]));
        let first = st.gen;
        st.start(|| Ok(vec![slot(2, "new")]));
        for _ in 0..200 {
            st.drain();
            if !st.open.as_ref().unwrap().loading {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // the first, slow answer arrives after the second
        st.tx.send((first, Ok(vec![slot(1, "old")]))).unwrap();
        st.drain();
        assert_eq!(st.open.as_ref().unwrap().rows[0].name, "new");
    }

    #[test]
    fn a_wide_name_is_cut_by_cells_so_the_row_never_overflows() {
        let wide = "日本語".repeat(20);
        let r = row(&slot(1, &wide), 60, false);
        let head: usize = r.spans[0].content.width();
        assert_eq!(head, 1 + 28 + 2, "name column is 28 cells");
    }
}
