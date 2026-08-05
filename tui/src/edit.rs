//! a vi line editor for the composer. insert and normal sub-modes, motions,
//! operators, registers and history — the same muscle memory as everywhere else,
//! rather than a handful of emacs control chords.
//!
//! text is held as `Vec<char>` and the cursor is a CHAR index. chat is full of
//! emoji and CJK, and byte offsets would put motions and deletes half-way
//! through a glyph; at composer length the extra allocation is irrelevant.
//!
//! no rendering and no app state here — main.rs asks what a key did and draws
//! the result, which is what lets every motion below be tested directly.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Insert,
    Normal,
}

/// what the caller should do after a key.
#[derive(PartialEq, Debug)]
pub enum Act {
    Continue,
    /// enter — send what's in the line.
    Send,
    /// escape from normal mode — put the composer away, keeping the draft.
    Leave,
}

/// what a half-typed multi-key command is still waiting for.
#[derive(Clone, Copy, PartialEq)]
enum Await {
    /// f F t T — jump to a character
    Find { back: bool, till: bool },
    /// r — replace the character under the cursor
    Replace,
}

/// character classes, vi's rules: word characters, punctuation, blanks. `w` and
/// `b` stop at every class change; `W` and `B` only at blanks.
fn class(c: char) -> u8 {
    if c.is_whitespace() {
        0
    } else if c.is_alphanumeric() || c == '_' {
        1
    } else {
        2
    }
}

pub struct Line {
    text: Vec<char>,
    cursor: usize,
    mode: Mode,
    /// pending operator: d, c or y.
    op: Option<char>,
    count: usize,
    awaiting: Option<Await>,
    /// unnamed register, for p / P.
    reg: Vec<char>,
    /// last f/F/t/T, for ; and ,
    last_find: Option<(char, bool, bool)>,
    undo: Vec<(Vec<char>, usize)>,
    history: Vec<String>,
    /// where we are walking history; None means "on the live draft".
    hist: Option<usize>,
    /// the draft we set aside when history walking started.
    stash: Vec<char>,
}

impl Default for Line {
    fn default() -> Self {
        Line {
            text: Vec::new(),
            cursor: 0,
            mode: Mode::Insert,
            op: None,
            count: 0,
            awaiting: None,
            reg: Vec::new(),
            last_find: None,
            undo: Vec::new(),
            history: Vec::new(),
            hist: None,
            stash: Vec::new(),
        }
    }
}

impl Line {
    pub fn text(&self) -> String {
        self.text.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// cursor position as a char index, for drawing the block.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// the pending command, echoed in the footer the way vi shows one.
    pub fn pending(&self) -> String {
        let mut s = String::new();
        if self.count > 0 {
            s.push_str(&self.count.to_string());
        }
        if let Some(op) = self.op {
            s.push(op);
        }
        match self.awaiting {
            Some(Await::Find { back, till }) => s.push(match (back, till) {
                (false, false) => 'f',
                (true, false) => 'F',
                (false, true) => 't',
                (true, true) => 'T',
            }),
            Some(Await::Replace) => s.push('r'),
            None => {}
        }
        s
    }

    /// accept the line: record it in history and start a fresh one in insert
    /// mode, the way hitting enter in any chat client does.
    pub fn accept(&mut self) -> String {
        let out: String = self.text.iter().collect();
        if !out.trim().is_empty() && self.history.last().map(String::as_str) != Some(out.as_str()) {
            self.history.push(out.clone());
        }
        self.reset();
        out
    }

    /// throw the draft away and return to insert mode.
    pub fn reset(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.mode = Mode::Insert;
        self.op = None;
        self.count = 0;
        self.awaiting = None;
        self.hist = None;
        self.stash.clear();
        self.undo.clear();
    }

    /// entering the composer always starts in insert mode — you came here to
    /// type, and the draft you left behind is still there.
    pub fn focus(&mut self) {
        self.mode = Mode::Insert;
        self.op = None;
        self.count = 0;
        self.awaiting = None;
    }

    fn snapshot(&mut self) {
        self.undo.push((self.text.clone(), self.cursor));
        if self.undo.len() > 100 {
            self.undo.remove(0);
        }
    }

    /// in normal mode the cursor sits ON a character, so it stops one short of
    /// the end; in insert mode it may sit past the last one.
    fn clamp(&mut self) {
        let max = match self.mode {
            Mode::Insert => self.text.len(),
            Mode::Normal => self.text.len().saturating_sub(1),
        };
        self.cursor = self.cursor.min(max);
    }

    fn take_count(&mut self) -> usize {
        let n = self.count.max(1);
        self.count = 0;
        n
    }

    pub fn key(&mut self, k: KeyEvent) -> Act {
        match self.mode {
            Mode::Insert => self.insert_key(k),
            Mode::Normal => self.normal_key(k),
        }
    }

    fn insert_key(&mut self, k: KeyEvent) -> Act {
        match k.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                // vi steps left when leaving insert
                self.cursor = self.cursor.saturating_sub(1);
                self.clamp();
            }
            KeyCode::Enter => return Act::Send,
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.snapshot();
                    self.cursor -= 1;
                    self.text.remove(self.cursor);
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.text.len() {
                    self.snapshot();
                    self.text.remove(self.cursor);
                }
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.text.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.text.len(),
            KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                self.snapshot();
                self.text.insert(self.cursor, c);
                self.cursor += 1;
            }
            _ => {}
        }
        Act::Continue
    }

    fn normal_key(&mut self, k: KeyEvent) -> Act {
        let KeyCode::Char(c) = k.code else {
            return self.normal_special(k);
        };
        if k.modifiers.contains(KeyModifiers::CONTROL) {
            return Act::Continue;
        }

        // a pending f/F/t/T or r consumes the very next character, whatever it is
        if let Some(a) = self.awaiting.take() {
            match a {
                Await::Find { back, till } => {
                    self.last_find = Some((c, back, till));
                    let n = self.take_count();
                    if let Some(t) = self.find_target(c, back, till, n) {
                        self.apply_motion(t, !back);
                    } else {
                        self.op = None;
                    }
                }
                Await::Replace => {
                    if self.cursor < self.text.len() {
                        self.snapshot();
                        self.text[self.cursor] = c;
                    }
                }
            }
            return Act::Continue;
        }

        // counts
        if c.is_ascii_digit() && !(c == '0' && self.count == 0) {
            self.count = (self.count.saturating_mul(10))
                .saturating_add(c.to_digit(10).unwrap_or(0) as usize)
                .min(100_000);
            return Act::Continue;
        }

        // an operator repeated is the linewise form: dd, cc, yy
        if let Some(op) = self.op {
            if c == op {
                self.op = None;
                self.count = 0;
                return self.whole_line(op);
            }
        }

        match c {
            // ---- motions ------------------------------------------------
            'h' => {
                let n = self.take_count();
                self.apply_motion(self.cursor.saturating_sub(n), false)
            }
            'l' | ' ' => {
                let n = self.take_count();
                self.apply_motion((self.cursor + n).min(self.text.len()), false)
            }
            '0' => self.apply_motion(0, false),
            '^' => {
                let t = self.first_non_blank();
                self.apply_motion(t, false)
            }
            '$' => self.apply_motion(self.text.len(), false),
            'w' | 'W' => {
                let n = self.take_count();
                let t = self.word_fwd(n, c == 'W');
                self.apply_motion(t, false)
            }
            'b' | 'B' => {
                let n = self.take_count();
                let t = self.word_back(n, c == 'B');
                self.apply_motion(t, false)
            }
            'e' | 'E' => {
                let n = self.take_count();
                let t = self.word_end(n, c == 'E');
                self.apply_motion(t, true)
            }
            'f' | 'F' | 't' | 'T' => {
                self.awaiting = Some(Await::Find {
                    back: c == 'F' || c == 'T',
                    till: c == 't' || c == 'T',
                });
            }
            ';' | ',' => {
                if let Some((ch, back, till)) = self.last_find {
                    let back = if c == ',' { !back } else { back };
                    let n = self.take_count();
                    if let Some(t) = self.find_target(ch, back, till, n) {
                        self.apply_motion(t, !back);
                    }
                }
            }
            // ---- operators ----------------------------------------------
            'd' | 'c' | 'y' => self.op = Some(c),
            'D' => return self.op_to_end('d'),
            'C' => return self.op_to_end('c'),
            'Y' => return self.whole_line('y'),
            // ---- edits ----------------------------------------------------
            'x' => {
                let n = self.take_count();
                let end = (self.cursor + n).min(self.text.len());
                self.cut(self.cursor, end, false);
            }
            'X' => {
                let n = self.take_count();
                let start = self.cursor.saturating_sub(n);
                self.cut(start, self.cursor, false);
            }
            'r' => self.awaiting = Some(Await::Replace),
            'i' => self.mode = Mode::Insert,
            'a' => {
                self.cursor = (self.cursor + 1).min(self.text.len());
                self.mode = Mode::Insert;
            }
            'I' => {
                self.cursor = self.first_non_blank();
                self.mode = Mode::Insert;
            }
            'A' => {
                self.cursor = self.text.len();
                self.mode = Mode::Insert;
            }
            'p' | 'P' => {
                if !self.reg.is_empty() {
                    self.snapshot();
                    let at = if c == 'p' {
                        (self.cursor + 1).min(self.text.len())
                    } else {
                        self.cursor
                    };
                    let reg = self.reg.clone();
                    let n = reg.len();
                    self.text.splice(at..at, reg);
                    self.cursor = at + n - 1;
                    self.clamp();
                }
            }
            'u' => {
                if let Some((t, cur)) = self.undo.pop() {
                    self.text = t;
                    self.cursor = cur;
                    self.clamp();
                }
            }
            // ---- history --------------------------------------------------
            'k' => self.history_step(-1),
            'j' => self.history_step(1),
            _ => {}
        }
        Act::Continue
    }

    fn normal_special(&mut self, k: KeyEvent) -> Act {
        match k.code {
            KeyCode::Enter => Act::Send,
            KeyCode::Esc => {
                if self.op.is_some() || self.count > 0 || self.awaiting.is_some() {
                    // first escape cancels a half-typed command, as in vi
                    self.op = None;
                    self.count = 0;
                    self.awaiting = None;
                    Act::Continue
                } else {
                    Act::Leave
                }
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                Act::Continue
            }
            KeyCode::Right => {
                self.cursor += 1;
                self.clamp();
                Act::Continue
            }
            KeyCode::Up => {
                self.history_step(-1);
                Act::Continue
            }
            KeyCode::Down => {
                self.history_step(1);
                Act::Continue
            }
            KeyCode::Home => {
                self.cursor = 0;
                Act::Continue
            }
            KeyCode::End => {
                self.cursor = self.text.len();
                self.clamp();
                Act::Continue
            }
            KeyCode::Backspace => {
                self.cursor = self.cursor.saturating_sub(1);
                Act::Continue
            }
            _ => Act::Continue,
        }
    }

    /// move, or apply the pending operator across the span just travelled.
    /// `inclusive` motions (e, f, t) cover the character they land on.
    fn apply_motion(&mut self, target: usize, inclusive: bool) {
        let target = target.min(self.text.len());
        match self.op.take() {
            None => {
                self.cursor = target;
                self.clamp();
            }
            Some(op) => {
                let (lo, hi) = if target >= self.cursor {
                    (self.cursor, if inclusive { (target + 1).min(self.text.len()) } else { target })
                } else {
                    (target, self.cursor)
                };
                match op {
                    'y' => self.reg = self.text[lo..hi].to_vec(),
                    'd' => self.cut(lo, hi, false),
                    'c' => self.cut(lo, hi, true),
                    _ => {}
                }
            }
        }
    }

    fn op_to_end(&mut self, op: char) -> Act {
        self.op = Some(op);
        self.apply_motion(self.text.len(), false);
        Act::Continue
    }

    fn whole_line(&mut self, op: char) -> Act {
        match op {
            'y' => self.reg = self.text.clone(),
            'd' => self.cut(0, self.text.len(), false),
            'c' => self.cut(0, self.text.len(), true),
            _ => {}
        }
        Act::Continue
    }

    /// remove `lo..hi` into the register. `into_insert` is the `c` behaviour.
    fn cut(&mut self, lo: usize, hi: usize, into_insert: bool) {
        let hi = hi.min(self.text.len());
        if lo < hi {
            self.snapshot();
            self.reg = self.text[lo..hi].to_vec();
            self.text.drain(lo..hi);
        }
        self.cursor = lo;
        if into_insert {
            self.mode = Mode::Insert;
        }
        self.clamp();
    }

    fn first_non_blank(&self) -> usize {
        self.text.iter().position(|c| !c.is_whitespace()).unwrap_or(0)
    }

    fn find_target(&self, ch: char, back: bool, till: bool, n: usize) -> Option<usize> {
        let mut at = self.cursor;
        for _ in 0..n {
            at = if back {
                self.text[..at.min(self.text.len())].iter().rposition(|&c| c == ch)?
            } else {
                let from = at + 1;
                from + self.text.get(from..)?.iter().position(|&c| c == ch)?
            };
        }
        Some(match (back, till) {
            (false, true) => at.saturating_sub(1),
            (true, true) => at + 1,
            _ => at,
        })
    }

    fn word_fwd(&self, n: usize, big: bool) -> usize {
        let mut i = self.cursor;
        let len = self.text.len();
        for _ in 0..n {
            if i >= len {
                break;
            }
            let start = if big { 1.min(class(self.text[i])) } else { class(self.text[i]) };
            // step off the current run…
            while i < len && cls(self.text[i], big) == start && start != 0 {
                i += 1;
            }
            // …then over any blanks
            while i < len && self.text[i].is_whitespace() {
                i += 1;
            }
        }
        i
    }

    fn word_back(&self, n: usize, big: bool) -> usize {
        let mut i = self.cursor;
        for _ in 0..n {
            if i == 0 {
                break;
            }
            i -= 1;
            while i > 0 && self.text[i].is_whitespace() {
                i -= 1;
            }
            let c = cls(self.text[i], big);
            while i > 0 && cls(self.text[i - 1], big) == c && c != 0 {
                i -= 1;
            }
        }
        i
    }

    fn word_end(&self, n: usize, big: bool) -> usize {
        let mut i = self.cursor;
        let len = self.text.len();
        for _ in 0..n {
            if i + 1 >= len {
                break;
            }
            i += 1;
            while i < len && self.text[i].is_whitespace() {
                i += 1;
            }
            if i >= len {
                break;
            }
            let c = cls(self.text[i], big);
            while i + 1 < len && cls(self.text[i + 1], big) == c {
                i += 1;
            }
        }
        i.min(len.saturating_sub(1))
    }

    /// walk sent history. -1 is older (vi's `k`), 1 is newer.
    fn history_step(&mut self, dir: i32) {
        if self.history.is_empty() {
            return;
        }
        let last = self.history.len() - 1;
        let next = match (self.hist, dir) {
            (None, -1) => {
                self.stash = self.text.clone();
                Some(last)
            }
            (Some(i), -1) => Some(i.saturating_sub(1)),
            (Some(i), 1) if i >= last => None,
            (Some(i), 1) => Some(i + 1),
            _ => return,
        };
        self.text = match next {
            Some(i) => self.history[i].chars().collect(),
            None => std::mem::take(&mut self.stash),
        };
        self.hist = next;
        self.cursor = self.text.len();
        self.clamp();
    }
}

/// class, collapsing word and punctuation for the WORD motions.
fn cls(c: char, big: bool) -> u8 {
    let k = class(c);
    if big && k == 2 {
        1
    } else {
        k
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn special(k: KeyCode) -> KeyEvent {
        KeyEvent::new(k, KeyModifiers::NONE)
    }

    /// drive a sequence; `<esc>` and `<cr>` are spelled out.
    fn run(l: &mut Line, keys: &str) -> Act {
        let mut act = Act::Continue;
        let mut it = keys.chars().peekable();
        while let Some(c) = it.next() {
            if c == '<' {
                let tag: String = it.by_ref().take_while(|&c| c != '>').collect();
                act = l.key(special(match tag.as_str() {
                    "esc" => KeyCode::Esc,
                    "cr" => KeyCode::Enter,
                    "bs" => KeyCode::Backspace,
                    "up" => KeyCode::Up,
                    "down" => KeyCode::Down,
                    other => panic!("unknown key <{other}>"),
                }));
            } else {
                act = l.key(key(c));
            }
        }
        act
    }

    fn line(s: &str) -> Line {
        let mut l = Line::default();
        run(&mut l, s);
        l
    }

    #[test]
    fn typing_inserts_and_starts_in_insert_mode() {
        let l = line("hello");
        assert_eq!(l.text(), "hello");
        assert_eq!(l.mode(), Mode::Insert);
        assert_eq!(l.cursor(), 5);
    }

    #[test]
    fn escape_enters_normal_mode_and_steps_left() {
        let l = line("hello<esc>");
        assert_eq!(l.mode(), Mode::Normal);
        assert_eq!(l.cursor(), 4, "vi puts the cursor on the last char, not past it");
    }

    #[test]
    fn escape_from_normal_leaves_the_composer_keeping_the_draft() {
        let mut l = line("wip<esc>");
        assert_eq!(l.key(special(KeyCode::Esc)), Act::Leave);
        assert_eq!(l.text(), "wip", "the draft survives");
    }

    #[test]
    fn escape_first_cancels_a_half_typed_command() {
        let mut l = line("hello world<esc>0");
        run(&mut l, "2d");
        assert_eq!(l.key(special(KeyCode::Esc)), Act::Continue, "cancels the pending d");
        assert_eq!(l.text(), "hello world");
        assert_eq!(l.key(special(KeyCode::Esc)), Act::Leave, "second one leaves");
    }

    #[test]
    fn enter_sends_from_either_mode() {
        let mut l = line("hi");
        assert_eq!(l.key(special(KeyCode::Enter)), Act::Send);
        let mut l = line("hi<esc>");
        assert_eq!(l.key(special(KeyCode::Enter)), Act::Send);
    }

    // ---- motions ---------------------------------------------------------

    #[test]
    fn hl_and_line_ends() {
        let mut l = line("hello world<esc>");
        run(&mut l, "0");
        assert_eq!(l.cursor(), 0);
        run(&mut l, "$");
        assert_eq!(l.cursor(), 10);
        run(&mut l, "5h");
        assert_eq!(l.cursor(), 5);
        run(&mut l, "2l");
        assert_eq!(l.cursor(), 7);
    }

    #[test]
    fn caret_goes_to_the_first_non_blank() {
        let mut l = line("   hi<esc>");
        run(&mut l, "^");
        assert_eq!(l.cursor(), 3);
    }

    #[test]
    fn word_motions_stop_at_class_changes() {
        let mut l = line("foo.bar baz<esc>0");
        run(&mut l, "w");
        assert_eq!(l.cursor(), 3, "w stops at the punctuation");
        run(&mut l, "w");
        assert_eq!(l.cursor(), 4);
        run(&mut l, "w");
        assert_eq!(l.cursor(), 8);
    }

    #[test]
    fn big_word_motions_only_stop_at_blanks() {
        let mut l = line("foo.bar baz<esc>0");
        run(&mut l, "W");
        assert_eq!(l.cursor(), 8, "W treats foo.bar as one WORD");
        run(&mut l, "B");
        assert_eq!(l.cursor(), 0);
    }

    #[test]
    fn e_lands_on_the_last_char_of_a_word() {
        let mut l = line("hello world<esc>0");
        run(&mut l, "e");
        assert_eq!(l.cursor(), 4);
        run(&mut l, "e");
        assert_eq!(l.cursor(), 10);
    }

    #[test]
    fn find_and_till_with_repeat() {
        let mut l = line("a,b,c<esc>0");
        run(&mut l, "f,");
        assert_eq!(l.cursor(), 1);
        run(&mut l, ";");
        assert_eq!(l.cursor(), 3);
        run(&mut l, "0t,");
        assert_eq!(l.cursor(), 0, "t stops before the target");
        run(&mut l, "$F,");
        assert_eq!(l.cursor(), 3);
    }

    // ---- operators -------------------------------------------------------

    #[test]
    fn dw_deletes_a_word() {
        let mut l = line("hello world<esc>0");
        run(&mut l, "dw");
        assert_eq!(l.text(), "world");
    }

    #[test]
    fn counts_apply_to_operators() {
        let mut l = line("one two three four<esc>0");
        run(&mut l, "3dw");
        assert_eq!(l.text(), "four");
    }

    #[test]
    fn dd_and_cc_take_the_whole_line() {
        let mut l = line("throw this away<esc>");
        run(&mut l, "dd");
        assert_eq!(l.text(), "");
        let mut l = line("replace me<esc>");
        run(&mut l, "cc");
        assert_eq!(l.text(), "");
        assert_eq!(l.mode(), Mode::Insert, "c leaves you typing");
    }

    #[test]
    fn shift_d_and_c_run_to_end_of_line() {
        let mut l = line("keep this drop that<esc>0");
        run(&mut l, "wwD");
        assert_eq!(l.text(), "keep this ");
        let mut l = line("keep drop<esc>0");
        run(&mut l, "wC");
        assert_eq!(l.text(), "keep ");
        assert_eq!(l.mode(), Mode::Insert);
    }

    #[test]
    fn ci_style_change_then_type() {
        let mut l = line("hello world<esc>0");
        run(&mut l, "cwbye ");
        assert_eq!(l.text(), "bye world");
    }

    #[test]
    fn df_deletes_through_the_target() {
        let mut l = line("a,b,c<esc>0");
        run(&mut l, "df,");
        assert_eq!(l.text(), "b,c", "f is inclusive");
    }

    #[test]
    fn backwards_operator_deletes_the_span_behind() {
        let mut l = line("hello world<esc>$");
        run(&mut l, "db");
        assert_eq!(l.text(), "hello d");
    }

    // ---- edits -----------------------------------------------------------

    #[test]
    fn x_deletes_forward_and_yanks_to_the_register() {
        let mut l = line("abcdef<esc>0");
        run(&mut l, "3x");
        assert_eq!(l.text(), "def");
        run(&mut l, "$p");
        assert_eq!(l.text(), "defabc", "p pastes after the cursor");
    }

    #[test]
    fn shift_p_pastes_before() {
        let mut l = line("abcdef<esc>0");
        run(&mut l, "3x0P");
        assert_eq!(l.text(), "abcdef");
    }

    #[test]
    fn r_replaces_one_char() {
        let mut l = line("cat<esc>0");
        run(&mut l, "rb");
        assert_eq!(l.text(), "bat");
    }

    #[test]
    fn insert_entry_points_land_in_the_right_place() {
        let mut l = line("bc<esc>0");
        run(&mut l, "ia");
        assert_eq!(l.text(), "abc");
        let mut l = line("ab<esc>");
        run(&mut l, "Ac");
        assert_eq!(l.text(), "abc");
        let mut l = line("ac<esc>0");
        run(&mut l, "ab");
        assert_eq!(l.text(), "abc", "a appends after the cursor");
        let mut l = line("  bc<esc>");
        run(&mut l, "Ia");
        assert_eq!(l.text(), "  abc", "I goes to the first non-blank");
    }

    #[test]
    fn u_undoes_the_last_change() {
        let mut l = line("hello world<esc>0");
        run(&mut l, "dw");
        assert_eq!(l.text(), "world");
        run(&mut l, "u");
        assert_eq!(l.text(), "hello world");
    }

    #[test]
    fn yank_then_paste_leaves_the_text_intact() {
        let mut l = line("word<esc>0");
        run(&mut l, "yw$p");
        assert_eq!(l.text(), "wordword");
    }

    // ---- utf-8 -----------------------------------------------------------

    /// chat is full of CJK and emoji; a byte-indexed editor would slice glyphs.
    #[test]
    fn motions_and_deletes_are_char_safe() {
        let mut l = line("キツネス pog<esc>0");
        run(&mut l, "x");
        assert_eq!(l.text(), "ツネス pog");
        run(&mut l, "dw");
        assert_eq!(l.text(), "pog");
    }

    #[test]
    fn emoji_are_single_positions() {
        let mut l = line("a🔥b<esc>0");
        run(&mut l, "l");
        assert_eq!(l.cursor(), 1);
        run(&mut l, "x");
        assert_eq!(l.text(), "ab");
    }

    // ---- history ---------------------------------------------------------

    #[test]
    fn accept_records_history_and_clears_the_line() {
        let mut l = line("first");
        assert_eq!(l.accept(), "first");
        assert_eq!(l.text(), "");
        assert_eq!(l.mode(), Mode::Insert, "ready to type the next one");
    }

    #[test]
    fn k_and_j_walk_sent_history() {
        let mut l = Line::default();
        run(&mut l, "one");
        l.accept();
        run(&mut l, "two");
        l.accept();
        run(&mut l, "draft<esc>");
        run(&mut l, "k");
        assert_eq!(l.text(), "two");
        run(&mut l, "k");
        assert_eq!(l.text(), "one");
        run(&mut l, "j");
        assert_eq!(l.text(), "two");
        run(&mut l, "j");
        assert_eq!(l.text(), "draft", "coming back restores the draft");
    }

    #[test]
    fn history_ignores_blanks_and_immediate_repeats() {
        let mut l = Line::default();
        run(&mut l, "   ");
        l.accept();
        run(&mut l, "same");
        l.accept();
        run(&mut l, "same");
        l.accept();
        run(&mut l, "<esc>k");
        assert_eq!(l.text(), "same");
        run(&mut l, "k");
        assert_eq!(l.text(), "same", "only one entry, nothing older");
    }

    // ---- guards ----------------------------------------------------------

    #[test]
    fn every_motion_survives_an_empty_line() {
        let mut l = Line::default();
        l.key(special(KeyCode::Esc));
        for keys in ["h", "l", "w", "b", "e", "$", "0", "^", "x", "X", "dw", "dd", "p", "u", "D"] {
            run(&mut l, keys);
            assert_eq!(l.text(), "");
            assert_eq!(l.cursor(), 0, "{keys} moved the cursor off an empty line");
        }
    }

    #[test]
    fn counts_cannot_run_off_the_end() {
        let mut l = line("ab<esc>0");
        run(&mut l, "999l");
        assert_eq!(l.cursor(), 1, "normal mode stops on the last char");
        run(&mut l, "999x");
        assert_eq!(l.text(), "a");
    }
}
