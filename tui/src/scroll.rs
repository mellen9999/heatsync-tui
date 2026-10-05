//! scrollback state: a message cursor over one channel's ring buffer.
//!
//! everything is a distance from the newest line (0 = newest), so lines
//! arriving at the live end don't move what you're reading: `sync` shifts the
//! cursor and the view back by however many arrived, and the view freezes.

use std::cell::Cell;

pub struct Scroll {
    /// the tab this belongs to — scrolling never follows you to another tab.
    pub chan: usize,
    pub name: String,
    /// the highlighted line.
    pub cursor: usize,
    /// the newest line in view. the draw pass moves it to keep the cursor
    /// visible (it alone knows how tall the lines are), hence the Cell.
    pub bot: Cell<usize>,
    /// channel `seq` as of the last sync.
    seq: u64,
    /// channel `seq` when scrolling began — "N new" counts from here.
    entered: u64,
}

impl Scroll {
    /// start at the newest line.
    pub fn enter(chan: usize, name: &str, seq: u64) -> Scroll {
        Scroll {
            chan,
            name: name.to_string(),
            cursor: 0,
            bot: Cell::new(0),
            seq,
            entered: seq,
        }
    }

    /// keep pointing at the same lines while new ones land (`seq` = the
    /// channel's append count, `len` = lines still in the ring).
    pub fn sync(&mut self, seq: u64, len: usize) {
        let delta = seq.saturating_sub(self.seq) as usize;
        self.seq = seq;
        let last = len.saturating_sub(1);
        self.cursor = (self.cursor + delta).min(last);
        self.bot.set((self.bot.get() + delta).min(last));
    }

    /// toward older lines.
    pub fn up(&mut self, n: usize, len: usize) {
        self.cursor = (self.cursor + n).min(len.saturating_sub(1));
    }

    /// toward the live end (stops at the newest line).
    pub fn down(&mut self, n: usize) {
        self.cursor = self.cursor.saturating_sub(n);
    }

    /// the oldest line in the ring.
    pub fn top(&mut self, len: usize) {
        self.cursor = len.saturating_sub(1);
    }

    /// lines that arrived since scrolling began.
    pub fn new_lines(&self) -> u64 {
        self.seq.saturating_sub(self.entered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enters_on_the_newest_line_and_moves_within_bounds() {
        let mut s = Scroll::enter(0, "x", 10);
        assert_eq!(s.cursor, 0);
        s.up(3, 5);
        assert_eq!(s.cursor, 3);
        s.up(99, 5);
        assert_eq!(s.cursor, 4, "stops at the oldest");
        s.down(2);
        assert_eq!(s.cursor, 2);
        s.down(99);
        assert_eq!(s.cursor, 0, "stops at the newest");
        s.top(5);
        assert_eq!(s.cursor, 4);
    }

    #[test]
    fn arrivals_shift_the_cursor_so_the_view_stays_put() {
        let mut s = Scroll::enter(0, "x", 10);
        s.up(2, 8);
        s.bot.set(1);
        s.sync(13, 8); // three new lines landed
        assert_eq!((s.cursor, s.bot.get()), (5, 4));
        assert_eq!(s.new_lines(), 3);
        s.sync(13, 8);
        assert_eq!(s.cursor, 5, "no arrivals, no shift");
    }

    #[test]
    fn eviction_clamps_to_the_oldest_line() {
        let mut s = Scroll::enter(0, "x", 0);
        s.up(3, 4);
        s.sync(5, 4); // ring is full: five arrived, only four remain
        assert_eq!(s.cursor, 3);
        assert_eq!(s.bot.get(), 3);
    }

    #[test]
    fn empty_ring_is_safe() {
        let mut s = Scroll::enter(0, "x", 0);
        s.up(5, 0);
        s.top(0);
        s.sync(3, 0);
        assert_eq!(s.cursor, 0);
    }
}
