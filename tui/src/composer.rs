//! composer — the message line editor. vim-grade: insert/normal/visual modes,
//! counts, operators (d/c/y), a register, undo/redo, dot-repeat, find repeats.
//! everything is pure over (String, byte-cursor) so it unit-tests without a
//! terminal; main.rs maps crossterm keys to [`VKey`] and outcomes to app state.

/// composer-local mode. `Visual.start` is the byte anchor of the selection.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum LineMode {
    Insert,
    Normal,
    Visual { start: usize },
}

/// keys the vi layer cares about (main.rs translates crossterm events).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum VKey {
    Char(char),
    Esc,
    Enter,
    CtrlR,
    Left,
    Right,
    Home,
    End,
}

/// what the app should do after a normal/visual keypress.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Outcome {
    Stay,
    ExitComposer,
    Send,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Op {
    Delete,
    Change,
    Yank,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Find {
    pub back: bool,
    pub till: bool,
}

/// multi-key command state: operator waiting for a motion, f/t/r waiting for
/// their char, g waiting for e.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Wait {
    None,
    Op(Op, u32), // operator + its own count
    FindChar { find: Find, op: Option<(Op, u32)> },
    Replace,
    G(Option<(Op, u32)>), // `g` prefix (ge motion)
}

pub struct Composer {
    pub text: String,
    pub cur: usize, // byte index, always on a char boundary
    pub mode: LineMode,
    pub comp: Option<Session>,
    count: u32, // accumulating count prefix (0 = none)
    wait: Wait,
    last_find: Option<(char, Find)>,
    register: String,
    undo: Vec<(String, usize)>,
    redo: Vec<(String, usize)>,
    // dot-repeat: keys of the last change are recorded as a tape and replayed.
    tape: Vec<VKey>,
    last_change: Option<Vec<VKey>>,
    capturing_insert: bool, // change entered Insert — keep taping until Esc
    replaying: bool,
}

impl Default for Composer {
    fn default() -> Composer {
        Composer {
            text: String::new(),
            cur: 0,
            mode: LineMode::Insert,
            comp: None,
            count: 0,
            wait: Wait::None,
            last_find: None,
            register: String::new(),
            undo: Vec::new(),
            redo: Vec::new(),
            tape: Vec::new(),
            last_change: None,
            capturing_insert: false,
            replaying: false,
        }
    }
}

const UNDO_CAP: usize = 100;

// ---- char classes & motions (vim `word` semantics) -------------------------

fn class(c: char) -> u8 {
    if c.is_whitespace() {
        0
    } else if c.is_alphanumeric() || c == '_' {
        1
    } else {
        2
    }
}

fn prev_boundary(text: &str, i: usize) -> usize {
    text[..i]
        .char_indices()
        .next_back()
        .map(|(j, _)| j)
        .unwrap_or(0)
}

fn next_boundary(text: &str, i: usize) -> usize {
    text[i..]
        .chars()
        .next()
        .map(|c| i + c.len_utf8())
        .unwrap_or(i)
}

fn char_at(text: &str, i: usize) -> Option<char> {
    text[i..].chars().next()
}

/// start of the next word (vim `w`). lands at len when no next word.
pub fn word_fwd(text: &str, cur: usize) -> usize {
    let mut i = cur;
    let Some(c0) = char_at(text, i) else {
        return text.len();
    };
    let k = class(c0);
    if k != 0 {
        while let Some(c) = char_at(text, i) {
            if class(c) != k {
                break;
            }
            i = next_boundary(text, i);
        }
    }
    while let Some(c) = char_at(text, i) {
        if class(c) != 0 {
            break;
        }
        i = next_boundary(text, i);
    }
    i
}

/// start of the previous word (vim `b`).
pub fn word_back(text: &str, cur: usize) -> usize {
    let mut i = cur;
    while i > 0 && char_at(text, prev_boundary(text, i)).is_some_and(|c| class(c) == 0) {
        i = prev_boundary(text, i);
    }
    if i == 0 {
        return 0;
    }
    let k = class(char_at(text, prev_boundary(text, i)).unwrap());
    while i > 0 && char_at(text, prev_boundary(text, i)).is_some_and(|c| class(c) == k) {
        i = prev_boundary(text, i);
    }
    i
}

/// end of the current/next word (vim `e`) — byte index OF the last char.
pub fn word_end(text: &str, cur: usize) -> usize {
    let mut i = next_boundary(text, cur);
    while let Some(c) = char_at(text, i) {
        if class(c) != 0 {
            break;
        }
        i = next_boundary(text, i);
    }
    let Some(c0) = char_at(text, i) else {
        return prev_boundary(text, text.len());
    };
    let k = class(c0);
    let mut last = i;
    while let Some(c) = char_at(text, i) {
        if class(c) != k {
            break;
        }
        last = i;
        i = next_boundary(text, i);
    }
    last
}

/// end of the previous word (vim `ge`).
pub fn word_end_back(text: &str, cur: usize) -> usize {
    let mut i = cur;
    // step back at least once, then skip whitespace
    if i > 0 {
        i = prev_boundary(text, i);
    }
    while i > 0 && char_at(text, i).is_some_and(|c| class(c) == 0) {
        i = prev_boundary(text, i);
    }
    i
}

/// first non-blank (vim `^`).
fn first_nonblank(text: &str) -> usize {
    text.char_indices()
        .find(|(_, c)| !c.is_whitespace())
        .map(|(i, _)| i)
        .unwrap_or(0)
}

/// nth occurrence of `c` from `cur` (vim f/F/t/T). returns the CURSOR target.
pub fn find_char(text: &str, cur: usize, c: char, find: Find, n: u32) -> Option<usize> {
    let n = n.max(1) as usize;
    if !find.back {
        let start = next_boundary(text, cur);
        let mut hits = text[start..]
            .char_indices()
            .filter(|(_, ch)| *ch == c)
            .map(|(i, _)| start + i);
        let pos = hits.nth(n - 1)?;
        Some(if find.till {
            prev_boundary(text, pos)
        } else {
            pos
        })
    } else {
        let mut hits: Vec<usize> = text[..cur]
            .char_indices()
            .filter(|(_, ch)| *ch == c)
            .map(|(i, _)| i)
            .collect();
        hits.reverse();
        let pos = *hits.get(n - 1)?;
        Some(if find.till {
            next_boundary(text, pos)
        } else {
            pos
        })
    }
}

impl Composer {
    // ---- basic editing (insert mode uses these; unchanged surface) ---------

    pub fn clear(&mut self) {
        self.text.clear();
        self.cur = 0;
        self.comp = None;
        self.mode = LineMode::Insert;
        self.wait = Wait::None;
        self.count = 0;
        self.undo.clear();
        self.redo.clear();
        self.capturing_insert = false;
    }

    /// take the trimmed line and reset (Enter/send).
    pub fn take(&mut self) -> String {
        let t = self.text.trim().to_string();
        self.clear();
        t
    }

    pub fn insert(&mut self, c: char) {
        self.text.insert(self.cur, c);
        self.cur += c.len_utf8();
        if self.capturing_insert {
            self.tape.push(VKey::Char(c));
        }
        self.after_edit();
    }

    pub fn backspace(&mut self) {
        if let Some(c) = self.text[..self.cur].chars().next_back() {
            self.cur -= c.len_utf8();
            self.text.remove(self.cur);
        }
        if self.capturing_insert {
            self.tape.clear(); // backspace poisons a clean dot tape — drop it
            self.capturing_insert = false;
        }
        self.after_edit();
    }

    pub fn delete(&mut self) {
        if self.cur < self.text.len() {
            self.text.remove(self.cur);
        }
        self.after_edit();
    }

    pub fn left(&mut self) {
        self.cur = prev_boundary(&self.text, self.cur);
        self.close_session();
    }

    pub fn right(&mut self) {
        self.cur = next_boundary(&self.text, self.cur);
        self.close_session();
    }

    pub fn home(&mut self) {
        self.cur = 0;
        self.close_session();
    }

    pub fn end(&mut self) {
        self.cur = self.text.len();
        self.close_session();
    }

    /// ctrl-w — delete the word (and trailing spaces) before the cursor.
    pub fn kill_word(&mut self) {
        let head = &self.text[..self.cur];
        let trimmed = head.trim_end();
        let cut = trimmed
            .rfind(char::is_whitespace)
            .map(|i| i + 1)
            .unwrap_or(0);
        self.text.replace_range(cut..self.cur, "");
        self.cur = cut;
        self.after_edit();
    }

    /// ctrl-u — delete everything before the cursor.
    pub fn kill_to_start(&mut self) {
        self.text.replace_range(..self.cur, "");
        self.cur = 0;
        self.after_edit();
    }

    /// the word the cursor sits at the end of: (byte start, the word so far).
    pub fn word_at_cursor(&self) -> (usize, &str) {
        let head = &self.text[..self.cur];
        let start = head.rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
        (start, &head[start..])
    }

    // ---- mode transitions --------------------------------------------------

    /// enter insert mode from the app (i/a keys) or from a change op. one
    /// insert session = one undo unit.
    pub fn enter_insert(&mut self) {
        self.push_undo();
        self.mode = LineMode::Insert;
    }

    /// Esc in insert mode: normal mode, cursor clamps onto the last char.
    pub fn enter_normal(&mut self) {
        self.mode = LineMode::Normal;
        self.wait = Wait::None;
        self.count = 0;
        self.close_session();
        if self.cur >= self.text.len() && !self.text.is_empty() {
            self.cur = prev_boundary(&self.text, self.text.len());
        }
        if self.capturing_insert {
            self.capturing_insert = false;
            if !self.replaying && !self.tape.is_empty() {
                self.last_change = Some(std::mem::take(&mut self.tape));
            }
        }
    }

    pub fn selection(&self) -> Option<(usize, usize)> {
        match self.mode {
            LineMode::Visual { start } => {
                let (a, b) = if start <= self.cur {
                    (start, self.cur)
                } else {
                    (self.cur, start)
                };
                Some((a, next_boundary(&self.text, b)))
            }
            _ => None,
        }
    }

    // ---- undo / register ---------------------------------------------------

    fn push_undo(&mut self) {
        self.undo.push((self.text.clone(), self.cur));
        if self.undo.len() > UNDO_CAP {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    fn undo_pop(&mut self) {
        if let Some((t, c)) = self.undo.pop() {
            self.redo.push((self.text.clone(), self.cur));
            self.text = t;
            self.cur = c.min(self.text.len());
        }
    }

    fn redo_pop(&mut self) {
        if let Some((t, c)) = self.redo.pop() {
            self.undo.push((self.text.clone(), self.cur));
            self.text = t;
            self.cur = c.min(self.text.len());
        }
    }

    fn yank_span(&mut self, a: usize, b: usize) {
        self.register = self.text[a..b].to_string();
    }

    fn delete_span(&mut self, a: usize, b: usize) {
        self.yank_span(a, b);
        self.text.replace_range(a..b, "");
        self.cur = a.min(self.text.len());
    }

    /// clamp normal-mode cursor onto a char (never == len unless empty).
    fn clamp(&mut self) {
        if self.cur >= self.text.len() && !self.text.is_empty() {
            self.cur = prev_boundary(&self.text, self.text.len());
        }
    }

    // ---- dot tape ----------------------------------------------------------

    fn tape_key(&mut self, k: VKey) {
        if !self.replaying {
            self.tape.push(k);
        }
    }

    /// a change command completed (and did NOT enter insert): commit the tape.
    fn commit_change(&mut self) {
        if !self.replaying {
            self.last_change = Some(std::mem::take(&mut self.tape));
        }
    }

    /// a pure motion / aborted command: the tape isn't a change — drop it.
    fn drop_tape(&mut self) {
        if !self.replaying {
            self.tape.clear();
        }
    }

    fn repeat_last_change(&mut self) {
        let Some(keys) = self.last_change.clone() else {
            return;
        };
        self.replaying = true;
        for k in keys {
            match self.mode {
                LineMode::Insert => match k {
                    VKey::Char(c) => self.insert(c),
                    VKey::Esc => self.enter_normal(),
                    _ => {}
                },
                _ => {
                    let _ = self.vi_key(k);
                }
            }
        }
        if self.mode == LineMode::Insert {
            self.enter_normal();
        }
        self.replaying = false;
    }

    // ---- normal/visual dispatch -------------------------------------------

    /// handle a key in Normal or Visual mode.
    pub fn vi_key(&mut self, k: VKey) -> Outcome {
        self.tape_key(k);
        match self.wait {
            Wait::FindChar { find, op } => return self.finish_find(k, find, op),
            Wait::Replace => return self.finish_replace(k),
            Wait::G(op) => return self.finish_g(k, op),
            _ => {}
        }
        match k {
            VKey::Esc => {
                match self.mode {
                    LineMode::Visual { .. } => {
                        self.mode = LineMode::Normal;
                        self.reset_cmd();
                        Outcome::Stay
                    }
                    _ => {
                        if self.wait != Wait::None || self.count > 0 {
                            self.reset_cmd(); // abort pending command, stay
                            Outcome::Stay
                        } else {
                            Outcome::ExitComposer
                        }
                    }
                }
            }
            VKey::Enter => Outcome::Send,
            VKey::CtrlR => {
                self.redo_pop();
                self.clamp();
                self.reset_cmd();
                Outcome::Stay
            }
            VKey::Left => self.motion_key('h'),
            VKey::Right => self.motion_key('l'),
            VKey::Home => self.motion_key('0'),
            VKey::End => self.motion_key('$'),
            VKey::Char(c) => self.vi_char(c),
        }
    }

    fn reset_cmd(&mut self) {
        self.wait = Wait::None;
        self.count = 0;
        self.drop_tape();
    }

    fn total_count(&self, opcount: u32) -> u32 {
        let a = self.count.max(1);
        let b = opcount.max(1);
        a.saturating_mul(b)
    }

    fn vi_char(&mut self, c: char) -> Outcome {
        // count prefix (0 is a motion when no count pending)
        if c.is_ascii_digit()
            && !(c == '0' && self.count == 0 && !matches!(self.wait, Wait::Op(_, n) if n > 0))
        {
            let d = c.to_digit(10).unwrap();
            match &mut self.wait {
                Wait::Op(_, n) => *n = n.saturating_mul(10).saturating_add(d),
                _ => self.count = self.count.saturating_mul(10).saturating_add(d),
            }
            return Outcome::Stay;
        }

        if let Wait::Op(op, opcount) = self.wait {
            return self.op_motion(op, opcount, c);
        }

        let visual = matches!(self.mode, LineMode::Visual { .. });
        match c {
            // motions
            'h' | 'l' | '0' | '^' | '$' | 'w' | 'b' | 'e' | ';' | ',' => self.motion_key(c),
            'f' | 'F' | 't' | 'T' => {
                self.wait = Wait::FindChar {
                    find: Find {
                        back: c == 'F' || c == 'T',
                        till: c == 't' || c == 'T',
                    },
                    op: None,
                };
                Outcome::Stay
            }
            'g' => {
                self.wait = Wait::G(None);
                Outcome::Stay
            }
            // operators
            'd' | 'c' | 'y' if !visual => {
                let op = match c {
                    'd' => Op::Delete,
                    'c' => Op::Change,
                    _ => Op::Yank,
                };
                self.wait = Wait::Op(op, 0);
                Outcome::Stay
            }
            'd' | 'x' if visual => self.visual_op(Op::Delete),
            'c' | 's' if visual => self.visual_op(Op::Change),
            'y' if visual => self.visual_op(Op::Yank),
            // visual entry / tweaks
            'v' => {
                self.mode = match self.mode {
                    LineMode::Visual { .. } => LineMode::Normal,
                    _ => LineMode::Visual { start: self.cur },
                };
                self.reset_cmd();
                Outcome::Stay
            }
            'V' => {
                if !self.text.is_empty() {
                    self.mode = LineMode::Visual { start: 0 };
                    self.cur = prev_boundary(&self.text, self.text.len());
                }
                self.reset_cmd();
                Outcome::Stay
            }
            'o' if visual => {
                if let LineMode::Visual { start } = self.mode {
                    self.mode = LineMode::Visual { start: self.cur };
                    self.cur = start;
                }
                Outcome::Stay
            }
            // edits
            'x' => {
                self.push_undo();
                let n = self.count.max(1);
                let mut b = self.cur;
                for _ in 0..n {
                    b = next_boundary(&self.text, b);
                }
                self.delete_span(self.cur, b);
                self.clamp();
                self.commit_change();
                self.count = 0;
                Outcome::Stay
            }
            'X' => {
                self.push_undo();
                let n = self.count.max(1);
                let mut a = self.cur;
                for _ in 0..n {
                    a = prev_boundary(&self.text, a);
                }
                self.delete_span(a, self.cur);
                self.commit_change();
                self.count = 0;
                Outcome::Stay
            }
            'D' => {
                self.push_undo();
                self.delete_span(self.cur, self.text.len());
                self.clamp();
                self.commit_change();
                self.count = 0;
                Outcome::Stay
            }
            'C' => {
                self.push_undo();
                self.delete_span(self.cur, self.text.len());
                self.begin_change_insert();
                Outcome::Stay
            }
            'Y' => {
                self.yank_span(self.cur, self.text.len());
                self.reset_cmd();
                Outcome::Stay
            }
            's' => {
                self.push_undo();
                let b = next_boundary(&self.text, self.cur);
                self.delete_span(self.cur, b);
                self.begin_change_insert();
                Outcome::Stay
            }
            'r' => {
                self.wait = Wait::Replace;
                Outcome::Stay
            }
            '~' => {
                self.push_undo();
                let n = self.count.max(1);
                for _ in 0..n {
                    let Some(ch) = char_at(&self.text, self.cur) else {
                        break;
                    };
                    let flipped: String = if ch.is_uppercase() {
                        ch.to_lowercase().collect()
                    } else {
                        ch.to_uppercase().collect()
                    };
                    let b = next_boundary(&self.text, self.cur);
                    self.text.replace_range(self.cur..b, &flipped);
                    self.cur += flipped.len();
                }
                self.clamp();
                self.commit_change();
                self.count = 0;
                Outcome::Stay
            }
            'p' | 'P' => {
                if self.register.is_empty() {
                    self.reset_cmd();
                    return Outcome::Stay;
                }
                self.push_undo();
                if visual {
                    // replace the selection with the register
                    if let Some((a, b)) = self.selection() {
                        let reg = self.register.clone();
                        self.text.replace_range(a..b, &reg);
                        self.cur = a + reg.len();
                        self.mode = LineMode::Normal;
                        self.clamp();
                    }
                } else {
                    let n = self.count.max(1);
                    let at = if c == 'p' && !self.text.is_empty() {
                        next_boundary(&self.text, self.cur)
                    } else {
                        self.cur
                    };
                    let chunk = self.register.repeat(n as usize);
                    self.text.insert_str(at, &chunk);
                    self.cur = at + chunk.len();
                    self.cur = prev_boundary(&self.text, self.cur); // land on last pasted char
                }
                self.commit_change();
                self.count = 0;
                Outcome::Stay
            }
            'i' | 'a' | 'I' | 'A' => {
                match c {
                    'a' => {
                        if !self.text.is_empty() {
                            self.cur = next_boundary(&self.text, self.cur);
                        }
                    }
                    'I' => self.cur = first_nonblank(&self.text),
                    'A' => self.cur = self.text.len(),
                    _ => {}
                }
                self.enter_insert();
                self.capturing_insert = !self.replaying;
                self.count = 0;
                Outcome::Stay
            }
            'u' => {
                self.undo_pop();
                self.clamp();
                self.reset_cmd();
                Outcome::Stay
            }
            '.' => {
                if !self.replaying {
                    self.drop_tape();
                    self.repeat_last_change();
                }
                Outcome::Stay
            }
            _ => {
                self.reset_cmd();
                Outcome::Stay
            }
        }
    }

    /// motions shared by bare normal mode and visual extension.
    fn motion_key(&mut self, c: char) -> Outcome {
        let n = self.count.max(1);
        for _ in 0..n {
            match c {
                'h' => self.cur = prev_boundary(&self.text, self.cur),
                'l' => {
                    let nb = next_boundary(&self.text, self.cur);
                    if nb < self.text.len() || matches!(self.mode, LineMode::Visual { .. }) {
                        self.cur = nb.min(self.text.len());
                        self.clamp();
                    }
                }
                '0' => self.cur = 0,
                '^' => self.cur = first_nonblank(&self.text),
                '$' => {
                    self.cur = if self.text.is_empty() {
                        0
                    } else {
                        prev_boundary(&self.text, self.text.len())
                    }
                }
                'w' => {
                    self.cur = word_fwd(&self.text, self.cur);
                    self.clamp();
                }
                'b' => self.cur = word_back(&self.text, self.cur),
                'e' => self.cur = word_end(&self.text, self.cur),
                ';' | ',' => {
                    if let Some((ch, mut find)) = self.last_find {
                        if c == ',' {
                            find.back = !find.back;
                        }
                        if let Some(p) = find_char(&self.text, self.cur, ch, find, 1) {
                            self.cur = p;
                        }
                    }
                }
                _ => {}
            }
        }
        self.count = 0;
        self.drop_tape();
        Outcome::Stay
    }

    /// operator + motion → span. vim quirks live here.
    fn op_motion(&mut self, op: Op, opcount: u32, m: char) -> Outcome {
        let n = self.total_count(opcount);
        // dd / cc / yy — whole line
        let line_double = matches!(
            (op, m),
            (Op::Delete, 'd') | (Op::Change, 'c') | (Op::Yank, 'y')
        );
        if line_double {
            self.wait = Wait::None;
            self.count = 0;
            match op {
                Op::Yank => {
                    self.yank_span(0, self.text.len());
                    self.drop_tape();
                }
                Op::Delete => {
                    self.push_undo();
                    self.delete_span(0, self.text.len());
                    self.commit_change();
                }
                Op::Change => {
                    self.push_undo();
                    self.delete_span(0, self.text.len());
                    self.begin_change_insert();
                }
            }
            return Outcome::Stay;
        }
        if matches!(m, 'f' | 'F' | 't' | 'T') {
            self.wait = Wait::FindChar {
                find: Find {
                    back: m == 'F' || m == 'T',
                    till: m == 't' || m == 'T',
                },
                op: Some((op, opcount)),
            };
            return Outcome::Stay;
        }
        if m == 'g' {
            self.wait = Wait::G(Some((op, opcount)));
            return Outcome::Stay;
        }
        let cur = self.cur;
        let span: Option<(usize, usize)> = match m {
            'w' => {
                // dw eats to next word start; cw behaves as ce (vim quirk)
                if op == Op::Change {
                    let mut e = cur;
                    for _ in 0..n {
                        e = word_end(&self.text, e);
                    }
                    Some((cur, next_boundary(&self.text, e)))
                } else {
                    let mut b = cur;
                    for _ in 0..n {
                        b = word_fwd(&self.text, b);
                    }
                    Some((cur, b))
                }
            }
            'e' => {
                let mut e = cur;
                for _ in 0..n {
                    e = word_end(&self.text, e);
                }
                Some((cur, next_boundary(&self.text, e)))
            }
            'b' => {
                let mut a = cur;
                for _ in 0..n {
                    a = word_back(&self.text, a);
                }
                Some((a, cur))
            }
            'h' => {
                let mut a = cur;
                for _ in 0..n {
                    a = prev_boundary(&self.text, a);
                }
                Some((a, cur))
            }
            'l' => {
                let mut b = cur;
                for _ in 0..n {
                    b = next_boundary(&self.text, b);
                }
                Some((cur, b))
            }
            '0' => Some((0, cur)),
            '^' => Some((
                first_nonblank(&self.text).min(cur),
                first_nonblank(&self.text).max(cur),
            )),
            '$' => Some((cur, self.text.len())),
            _ => None,
        };
        self.wait = Wait::None;
        self.count = 0;
        let Some((a, b)) = span else {
            self.drop_tape();
            return Outcome::Stay;
        };
        let (a, b) = (a.min(b), b.max(a));
        self.apply_op(op, a, b)
    }

    fn apply_op(&mut self, op: Op, a: usize, b: usize) -> Outcome {
        match op {
            Op::Yank => {
                self.yank_span(a, b);
                self.cur = a;
                self.drop_tape();
            }
            Op::Delete => {
                self.push_undo();
                self.delete_span(a, b);
                self.clamp();
                self.commit_change();
            }
            Op::Change => {
                self.push_undo();
                self.delete_span(a, b);
                self.begin_change_insert();
            }
        }
        Outcome::Stay
    }

    fn visual_op(&mut self, op: Op) -> Outcome {
        let Some((a, b)) = self.selection() else {
            return Outcome::Stay;
        };
        self.mode = LineMode::Normal;
        self.wait = Wait::None;
        self.count = 0;
        match op {
            Op::Yank => {
                self.yank_span(a, b);
                self.cur = a;
                self.drop_tape();
            }
            Op::Delete => {
                self.push_undo();
                self.delete_span(a, b);
                self.clamp();
                self.drop_tape(); // visual changes don't dot-repeat (vim-ish)
            }
            Op::Change => {
                self.push_undo();
                self.delete_span(a, b);
                self.enter_insert_no_undo();
                self.drop_tape();
            }
        }
        Outcome::Stay
    }

    fn finish_find(&mut self, k: VKey, find: Find, op: Option<(Op, u32)>) -> Outcome {
        self.wait = Wait::None;
        let VKey::Char(c) = k else {
            self.reset_cmd();
            return Outcome::Stay;
        };
        let n = match op {
            Some((_, oc)) => self.total_count(oc),
            None => self.count.max(1),
        };
        self.count = 0;
        self.last_find = Some((c, find));
        let Some(pos) = find_char(&self.text, self.cur, c, find, n) else {
            self.drop_tape();
            return Outcome::Stay;
        };
        match op {
            None => {
                self.cur = pos;
                self.drop_tape();
                Outcome::Stay
            }
            Some((op, _)) => {
                let (a, b) = if pos >= self.cur {
                    // forward spans are inclusive of the target char
                    (self.cur, next_boundary(&self.text, pos))
                } else {
                    (pos, self.cur)
                };
                self.apply_op(op, a, b)
            }
        }
    }

    fn finish_replace(&mut self, k: VKey) -> Outcome {
        self.wait = Wait::None;
        let VKey::Char(c) = k else {
            self.reset_cmd();
            return Outcome::Stay;
        };
        if self.text.is_empty() {
            self.reset_cmd();
            return Outcome::Stay;
        }
        self.push_undo();
        let n = self.count.max(1);
        self.count = 0;
        // vim r with a count replaces n chars (only if n chars remain)
        let mut i = self.cur;
        let mut targets = Vec::new();
        for _ in 0..n {
            if i >= self.text.len() {
                break;
            }
            targets.push(i);
            i = next_boundary(&self.text, i);
        }
        for &at in targets.iter().rev() {
            let b = next_boundary(&self.text, at);
            self.text.replace_range(at..b, &c.to_string());
        }
        if let Some(&last) = targets.last() {
            self.cur = last.min(self.text.len());
            self.clamp();
        }
        self.commit_change();
        Outcome::Stay
    }

    fn finish_g(&mut self, k: VKey, op: Option<(Op, u32)>) -> Outcome {
        self.wait = Wait::None;
        let VKey::Char('e') = k else {
            self.reset_cmd();
            return Outcome::Stay;
        };
        let n = match op {
            Some((_, oc)) => self.total_count(oc),
            None => self.count.max(1),
        };
        self.count = 0;
        let mut pos = self.cur;
        for _ in 0..n {
            pos = word_end_back(&self.text, pos);
        }
        match op {
            None => {
                self.cur = pos;
                self.drop_tape();
                Outcome::Stay
            }
            Some((op, _)) => self.apply_op(op, pos, self.cur),
        }
    }

    fn begin_change_insert(&mut self) {
        self.enter_insert_no_undo();
        self.capturing_insert = !self.replaying;
        self.count = 0;
    }

    /// change-ops already pushed their undo snapshot — don't double-push.
    fn enter_insert_no_undo(&mut self) {
        self.mode = LineMode::Insert;
    }
}

// ---- completion sessions ---------------------------------------------------

/// what kind of completion a session is running.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Trigger {
    Word,
    Mention,
    Colon,
    Channel,
}

/// badge shown next to a candidate row.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Badge {
    SevenTv,
    Bttv,
    Ffz,
    Platform, // twitch/kick native
    Emoji,
    User,
    Chan,
}

impl Badge {
    pub fn label(self) -> &'static str {
        match self {
            Badge::SevenTv => "7tv",
            Badge::Bttv => "bttv",
            Badge::Ffz => "ffz",
            Badge::Platform => "tw",
            Badge::Emoji => "emoji",
            Badge::User => "user",
            Badge::Chan => "chan",
        }
    }

    pub fn from_provider(p: &str) -> Badge {
        match p {
            "7tv" => Badge::SevenTv,
            "bttv" => Badge::Bttv,
            "ffz" => Badge::Ffz,
            _ => Badge::Platform,
        }
    }
}

/// one completable item. `insert` is what accept writes, `label` the row text,
/// `matchkey` what the fuzzy scorer sees (emoji match their shortcode).
#[derive(Clone, Debug)]
pub struct Candidate {
    pub insert: String,
    pub label: String,
    pub matchkey: String,
    pub badge: Badge,
}

impl Candidate {
    pub fn plain(s: impl Into<String>, badge: Badge) -> Candidate {
        let s = s.into();
        Candidate {
            insert: s.clone(),
            label: s.clone(),
            matchkey: s,
            badge,
        }
    }
}

/// a live completion: the buffer stays untouched while navigating — only
/// accept() writes. every edit refilters from the anchored query.
pub struct Session {
    pub anchor: usize, // byte start of the region accept replaces (incl. @/:)
    pub trigger: Trigger,
    pub pool: Vec<Candidate>,
    pub filtered: Vec<usize>,
    pub sel: usize,
    pub explicit: bool, // opened by Tab (colon auto-sessions die under 2 chars)
}

const FILTER_CAP: usize = 200;
/// chars legal inside a `:shortcode` query.
fn shortcode_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-')
}

/// is the cursor inside `:xy…`? → (anchor of ':', query). the ':' must sit at
/// word start (whitespace/BOL before it), the query must be shortcode-class.
pub fn colon_query(text: &str, cur: usize) -> Option<(usize, &str)> {
    let head = &text[..cur];
    let wstart = head.rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
    let word = &head[wstart..];
    let rest = word.strip_prefix(':')?;
    if rest.is_empty() || !rest.chars().all(shortcode_char) {
        return None;
    }
    Some((wstart, rest))
}

/// the char just typed was ':' — is there a complete `:name:` behind the
/// cursor? → (anchor, name) for an exact-shortcode replace.
pub fn closing_colon(text: &str, cur: usize) -> Option<(usize, &str)> {
    let head = &text[..cur];
    let body = head.strip_suffix(':')?;
    let wstart = body.rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
    let word = &body[wstart..];
    let name = word.strip_prefix(':')?;
    if name.is_empty() || !name.chars().all(shortcode_char) {
        return None;
    }
    Some((wstart, name))
}

/// the emoji pool: every gemoji shortcode → (shortcode, glyph). built once.
pub fn emoji_pool() -> &'static [(String, &'static str)] {
    use std::sync::OnceLock;
    static POOL: OnceLock<Vec<(String, &'static str)>> = OnceLock::new();
    POOL.get_or_init(|| {
        let mut v: Vec<(String, &'static str)> = emojis::iter()
            .flat_map(|e| e.shortcodes().map(move |s| (s.to_string(), e.as_str())))
            .collect();
        v.sort();
        v
    })
}

impl Composer {
    /// after any Insert-mode edit: exact `:name:` replace, then session
    /// refilter/close. auto-open is the caller's job (needs the pool).
    fn after_edit(&mut self) {
        // exact closing-colon replace happens before any session logic
        if self.text[..self.cur].ends_with(':') {
            if let Some((anchor, name)) = closing_colon(&self.text, self.cur) {
                if let Ok(i) = emoji_pool().binary_search_by(|(s, _)| s.as_str().cmp(name)) {
                    let glyph = emoji_pool()[i].1;
                    self.text.replace_range(anchor..self.cur, glyph);
                    self.cur = anchor + glyph.len();
                    self.comp = None;
                    return;
                }
            }
        }
        self.refilter();
    }

    pub fn close_session(&mut self) {
        self.comp = None;
    }

    /// open a session over the word ending at the cursor. `anchor` is where
    /// accept will start replacing (includes any @/: sigil).
    pub fn open_session(
        &mut self,
        trigger: Trigger,
        anchor: usize,
        pool: Vec<Candidate>,
        explicit: bool,
    ) {
        self.comp = Some(Session {
            anchor,
            trigger,
            pool,
            filtered: Vec::new(),
            sel: 0,
            explicit,
        });
        self.refilter();
    }

    /// the live query for the open session (text between anchor+sigil and cursor).
    fn session_query(&self) -> Option<String> {
        let s = self.comp.as_ref()?;
        if self.cur < s.anchor {
            return None;
        }
        let raw = &self.text[s.anchor..self.cur];
        let q = match s.trigger {
            Trigger::Mention => raw.strip_prefix('@')?,
            Trigger::Colon => raw.strip_prefix(':')?,
            _ => raw,
        };
        if q.contains(char::is_whitespace) {
            return None;
        }
        Some(q.to_string())
    }

    /// re-rank the pool against the current query; close when it stops
    /// making sense (cursor left, whitespace, no matches, short colon query).
    pub fn refilter(&mut self) {
        let Some(query) = self.session_query() else {
            self.comp = None;
            return;
        };
        let Some(s) = self.comp.as_mut() else { return };
        if s.trigger == Trigger::Colon && !s.explicit && query.chars().count() < 2 {
            self.comp = None;
            return;
        }
        let filtered =
            heatsync_core::fuzzy::rank(&s.pool, |c| c.matchkey.as_str(), &query, FILTER_CAP);
        if filtered.is_empty() {
            self.comp = None;
            return;
        }
        s.filtered = filtered;
        s.sel = 0;
    }

    pub fn session_next(&mut self, back: bool) {
        if let Some(s) = self.comp.as_mut() {
            let n = s.filtered.len();
            if n > 0 {
                s.sel = if back {
                    (s.sel + n - 1) % n
                } else {
                    (s.sel + 1) % n
                };
            }
        }
    }

    /// write the selected candidate over anchor..cursor and close.
    pub fn accept(&mut self) {
        let Some(s) = self.comp.take() else { return };
        let Some(&i) = s.filtered.get(s.sel) else {
            return;
        };
        let insert = &s.pool[i].insert;
        self.text.replace_range(s.anchor..self.cur, insert);
        self.cur = s.anchor + insert.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(s: &str) -> Composer {
        let mut c = Composer::default();
        for ch in s.chars() {
            c.insert(ch);
        }
        c
    }

    /// typed text, then Esc into normal mode.
    fn normal(s: &str) -> Composer {
        let mut c = typed(s);
        c.enter_normal();
        c
    }

    fn keys(c: &mut Composer, s: &str) {
        for ch in s.chars() {
            match c.mode {
                LineMode::Insert => c.insert(ch),
                _ => {
                    let _ = c.vi_key(VKey::Char(ch));
                }
            }
        }
    }

    fn esc(c: &mut Composer) {
        match c.mode {
            LineMode::Insert => c.enter_normal(),
            _ => {
                let _ = c.vi_key(VKey::Esc);
            }
        }
    }

    // ---- basic editing (carried over from round 1) -------------------------

    #[test]
    fn cursor_editing_is_utf8_safe() {
        let mut c = typed("héllo");
        c.left();
        c.left();
        c.left();
        c.backspace(); // removes é
        assert_eq!(c.text, "hllo");
        c.insert('é');
        assert_eq!(c.text, "héllo");
        c.home();
        c.delete();
        assert_eq!(c.text, "éllo");
        c.end();
        assert_eq!(c.cur, c.text.len());
    }

    #[test]
    fn kill_word_eats_word_and_spaces() {
        let mut c = typed("send this msg  ");
        c.kill_word();
        assert_eq!(c.text, "send this ");
        c.kill_word();
        assert_eq!(c.text, "send ");
    }

    // ---- vi: motions -------------------------------------------------------

    #[test]
    fn word_motions_vim_semantics() {
        let t = "foo  bar-baz qux";
        assert_eq!(word_fwd(t, 0), 5); // foo → bar
        assert_eq!(word_fwd(t, 5), 8); // bar → '-' (punct run)
        assert_eq!(word_fwd(t, 8), 9); // '-' → baz
        assert_eq!(word_back(t, 9), 8);
        assert_eq!(word_back(t, 5), 0);
        assert_eq!(word_end(t, 0), 2); // on f → end of foo
        assert_eq!(word_end(t, 2), 7); // end of foo → end of bar
    }

    #[test]
    fn dollar_zero_caret_motions() {
        let mut c = normal("  hello");
        keys(&mut c, "$");
        assert_eq!(c.cur, c.text.len() - 1);
        keys(&mut c, "0");
        assert_eq!(c.cur, 0);
        keys(&mut c, "^");
        assert_eq!(c.cur, 2);
    }

    #[test]
    fn find_and_till_with_repeats() {
        let mut c = normal("abcabc");
        keys(&mut c, "0fb");
        assert_eq!(c.cur, 1);
        keys(&mut c, ";");
        assert_eq!(c.cur, 4);
        keys(&mut c, ",");
        assert_eq!(c.cur, 1);
        let mut c = normal("abcabc");
        keys(&mut c, "0tc");
        assert_eq!(c.cur, 1); // till stops before c
        let mut c = normal("abcabc");
        keys(&mut c, "$Fa");
        assert_eq!(c.cur, 3);
    }

    #[test]
    fn counts_multiply() {
        let mut c = normal("a b c d e");
        keys(&mut c, "03w");
        assert_eq!(c.cur, 6);
        let mut c = normal("abcdef");
        keys(&mut c, "03l");
        assert_eq!(c.cur, 3);
    }

    // ---- vi: operators -----------------------------------------------------

    #[test]
    fn dw_eats_trailing_space() {
        let mut c = normal("foo bar baz");
        keys(&mut c, "0dw");
        assert_eq!(c.text, "bar baz");
        assert_eq!(c.register, "foo ");
    }

    #[test]
    fn d2w_and_2dw() {
        let mut c = normal("a b c d");
        keys(&mut c, "0d2w");
        assert_eq!(c.text, "c d");
        let mut c = normal("a b c d");
        keys(&mut c, "02dw");
        assert_eq!(c.text, "c d");
    }

    #[test]
    fn cw_behaves_as_ce() {
        let mut c = normal("foo bar");
        keys(&mut c, "0cw");
        assert_eq!(c.text, " bar"); // space kept — ce semantics
        assert_eq!(c.mode, LineMode::Insert);
        keys(&mut c, "yo");
        assert_eq!(c.text, "yo bar");
    }

    #[test]
    fn dd_clears_line_d_dollar_truncates() {
        let mut c = normal("hello world");
        keys(&mut c, "dd");
        assert_eq!(c.text, "");
        let mut c = normal("hello world");
        keys(&mut c, "0wD");
        assert_eq!(c.text, "hello ");
    }

    #[test]
    fn db_and_x_and_tilde() {
        let mut c = normal("foo bar");
        keys(&mut c, "$db");
        assert_eq!(c.text, "foo r");
        let mut c = normal("abc");
        keys(&mut c, "0x");
        assert_eq!(c.text, "bc");
        keys(&mut c, "~");
        assert_eq!(c.text, "Bc");
    }

    #[test]
    fn df_and_dt_spans() {
        let mut c = normal("foo=bar;rest");
        keys(&mut c, "0df=");
        assert_eq!(c.text, "bar;rest");
        let mut c = normal("foo=bar;rest");
        keys(&mut c, "0dt=");
        assert_eq!(c.text, "=bar;rest");
    }

    #[test]
    fn replace_char() {
        let mut c = normal("kékw");
        keys(&mut c, "0re");
        assert_eq!(c.text, "eékw");
        keys(&mut c, "l3rX");
        assert_eq!(c.text, "eXXX");
    }

    // ---- vi: register + paste ---------------------------------------------

    #[test]
    fn yank_paste_roundtrip() {
        let mut c = normal("foo bar");
        keys(&mut c, "0yw$p");
        assert_eq!(c.text, "foo barfoo ");
        let mut c = normal("ab");
        keys(&mut c, "0ylP"); // yank 'a', paste before
        assert_eq!(c.text, "aab");
    }

    #[test]
    fn delete_fills_register() {
        let mut c = normal("foo bar");
        keys(&mut c, "0dw$p");
        assert_eq!(c.text, "barfoo "); // register holds "foo " (dw ate the space)
    }

    // ---- vi: visual --------------------------------------------------------

    #[test]
    fn visual_select_delete() {
        let mut c = normal("hello world");
        keys(&mut c, "0vllld");
        assert_eq!(c.text, "o world");
        assert_eq!(c.register, "hell");
    }

    #[test]
    fn visual_line_yank_and_change() {
        let mut c = normal("abc def");
        keys(&mut c, "Vy");
        assert_eq!(c.register, "abc def");
        keys(&mut c, "vwc");
        assert_eq!(c.mode, LineMode::Insert);
    }

    #[test]
    fn visual_swap_ends_and_paste_over() {
        let mut c = normal("one two");
        keys(&mut c, "0ywv$p"); // yank "one ", select to end, paste over
        assert_eq!(c.text, "one ");
    }

    // ---- vi: undo / redo / dot --------------------------------------------

    #[test]
    fn undo_redo() {
        let mut c = normal("hello");
        keys(&mut c, "0x");
        assert_eq!(c.text, "ello");
        keys(&mut c, "u");
        assert_eq!(c.text, "hello");
        let _ = c.vi_key(VKey::CtrlR);
        assert_eq!(c.text, "ello");
    }

    #[test]
    fn insert_session_is_one_undo_unit() {
        let mut c = normal("ab");
        keys(&mut c, "A");
        keys(&mut c, "xyz");
        esc(&mut c);
        assert_eq!(c.text, "abxyz");
        keys(&mut c, "u");
        assert_eq!(c.text, "ab");
    }

    #[test]
    fn dot_repeats_delete() {
        let mut c = normal("a b c d");
        keys(&mut c, "0dw");
        assert_eq!(c.text, "b c d");
        keys(&mut c, ".");
        assert_eq!(c.text, "c d");
    }

    #[test]
    fn dot_repeats_change_with_insert_text() {
        let mut c = normal("foo bar");
        keys(&mut c, "0cw");
        keys(&mut c, "zap");
        esc(&mut c);
        assert_eq!(c.text, "zap bar");
        keys(&mut c, "w.");
        assert_eq!(c.text, "zap zap");
    }

    #[test]
    fn esc_aborts_pending_then_exits() {
        let mut c = normal("abc");
        keys(&mut c, "d");
        assert_eq!(c.vi_key(VKey::Esc), Outcome::Stay); // aborts the d
        assert_eq!(c.vi_key(VKey::Esc), Outcome::ExitComposer);
    }

    // ---- sessions ----------------------------------------------------------

    fn pool(names: &[&str]) -> Vec<Candidate> {
        names
            .iter()
            .map(|n| Candidate::plain(*n, Badge::SevenTv))
            .collect()
    }

    #[test]
    fn session_narrows_and_accepts() {
        let mut c = typed("hi kek");
        c.open_session(Trigger::Word, 3, pool(&["KEKW", "KEKWait", "other"]), true);
        let s = c.comp.as_ref().unwrap();
        assert_eq!(s.filtered.len(), 2);
        c.insert('w'); // "hi kekw" — narrows, buffer untouched by session
        assert_eq!(c.comp.as_ref().unwrap().filtered.len(), 2);
        c.session_next(false);
        c.accept();
        assert_eq!(c.text, "hi KEKWait");
        assert!(c.comp.is_none());
    }

    #[test]
    fn session_closes_on_whitespace_or_cursor_escape() {
        let mut c = typed("kek");
        c.open_session(Trigger::Word, 0, pool(&["KEKW"]), true);
        c.insert(' ');
        assert!(c.comp.is_none());
        let mut c = typed("kek");
        c.open_session(Trigger::Word, 0, pool(&["KEKW"]), true);
        c.home();
        assert!(c.comp.is_none());
    }

    #[test]
    fn session_closes_on_no_matches() {
        let mut c = typed("kek");
        c.open_session(Trigger::Word, 0, pool(&["KEKW"]), true);
        c.insert('z');
        c.insert('z');
        assert!(c.comp.is_none());
    }

    #[test]
    fn mention_session_replaces_with_sigil() {
        let mut c = typed("yo @cl");
        let cand = vec![Candidate {
            insert: "@Clara".into(),
            label: "Clara".into(),
            matchkey: "Clara".into(),
            badge: Badge::User,
        }];
        c.open_session(Trigger::Mention, 3, cand, true);
        assert_eq!(c.comp.as_ref().unwrap().filtered.len(), 1);
        c.accept();
        assert_eq!(c.text, "yo @Clara");
    }

    // ---- colon / emoji -----------------------------------------------------

    #[test]
    fn colon_query_rules() {
        assert_eq!(colon_query("hi :jo", 6), Some((3, "jo")));
        assert_eq!(colon_query(":jo", 3), Some((0, "jo")));
        assert_eq!(colon_query("foo:jo", 6), None); // ':' not at word start
        assert_eq!(colon_query("https://x", 9), None);
        assert_eq!(colon_query("::", 2), None); // empty query
        assert_eq!(colon_query("hi :", 4), None);
    }

    #[test]
    fn closing_colon_exact_emoji_replaces() {
        let mut c = typed("gg :joy");
        c.insert(':');
        assert_eq!(c.text, "gg 😂");
        assert!(c.comp.is_none());
    }

    #[test]
    fn closing_colon_unknown_stays_literal() {
        let mut c = typed("gg :notanemoji");
        c.insert(':');
        assert_eq!(c.text, "gg :notanemoji:");
    }

    #[test]
    fn colon_session_from_pool_strips_colon_for_emotes() {
        let mut c = typed(":kek");
        c.open_session(Trigger::Colon, 0, pool(&["KEKW"]), false);
        assert_eq!(c.comp.as_ref().unwrap().filtered.len(), 1);
        c.accept();
        assert_eq!(c.text, "KEKW");
    }

    #[test]
    fn colon_auto_session_dies_under_two_chars() {
        let mut c = typed(":ke");
        c.open_session(Trigger::Colon, 0, pool(&["KEKW"]), false);
        assert!(c.comp.is_some());
        c.backspace(); // ":k" — under 2 chars, auto session dies
        assert!(c.comp.is_none());
    }
}
