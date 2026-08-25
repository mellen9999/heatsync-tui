//! vi navigation over the chat ring: a cursor, a linewise visual selection,
//! counts, and search. no rendering and no I/O — main.rs owns the keymap and the
//! drawing, this owns the model, which is what makes the motions testable.
//!
//! COORDINATES. everything here is "index back from newest": 0 is the newest
//! message, larger is older. that reads backwards until you picture the pane as
//! a file — oldest at the top, newest at the bottom, which is how chat is drawn.
//! then it lines up with vi exactly: `j` moves DOWN the file toward newer (index
//! shrinks), `k` moves UP toward older (index grows), `G` is the newest line at
//! the bottom, `gg` is the oldest one loaded.

use std::cell::Cell;

/// what the last draw actually fit on screen. only the layout knows, because
/// messages are 1 or 2 rows tall depending on whether they carry an emote — so
/// paging and keeping the cursor visible have to read it back from the frame.
#[derive(Clone, Copy, Default)]
pub struct View {
    pub count: usize,
}

/// which way a search runs. forward = down the file = toward newer messages.
#[derive(Clone, Copy, PartialEq)]
pub enum Dir {
    Fwd,
    Back,
}

impl Dir {
    pub fn flip(self) -> Dir {
        match self {
            Dir::Fwd => Dir::Back,
            Dir::Back => Dir::Fwd,
        }
    }
}

#[derive(Default)]
pub struct Vi {
    /// selected message. only meaningful once the user has started navigating.
    pub cursor: usize,
    /// index of the newest message on screen — the viewport anchor.
    pub scroll: usize,
    /// visual-mode anchor; the selection runs between this and the cursor.
    pub visual: Option<usize>,
    /// pending motion count (`5j`). 0 means none typed.
    pub count: usize,
    /// saw a bare `g`, waiting to see whether it's `gg` / `gt` / `gT`.
    pub g_pending: bool,
    /// last search pattern and direction, for `n` / `N`.
    pub pattern: String,
    pub dir: Option<Dir>,
    pub view: Cell<View>,
}

impl Vi {
    /// back to following live chat: no selection, pinned to the newest message.
    pub fn reset(&mut self) {
        self.cursor = 0;
        self.scroll = 0;
        self.visual = None;
        self.count = 0;
        self.g_pending = false;
    }

    /// are we pinned to the bottom with nothing selected? while true the cursor
    /// bar stays hidden and new messages scroll in, so live chat reads clean and
    /// the first `k` is what reveals the cursor (less/copy-mode behaviour).
    pub fn following(&self) -> bool {
        self.cursor == 0 && self.scroll == 0 && self.visual.is_none()
    }

    /// consume the pending count, defaulting to 1.
    pub fn take_count(&mut self) -> usize {
        let n = self.count.max(1);
        self.count = 0;
        n
    }

    /// accumulate a count digit. `0` only counts as a digit mid-number, matching
    /// vi (a bare `0` is a motion, and here there is no column to move to).
    pub fn push_digit(&mut self, d: u32) -> bool {
        if d == 0 && self.count == 0 {
            return false;
        }
        self.count = (self.count.saturating_mul(10))
            .saturating_add(d as usize)
            .min(1_000_000);
        true
    }

    /// move toward newer (down the file). saturates at the newest message.
    pub fn down(&mut self, n: usize) {
        self.cursor = self.cursor.saturating_sub(n);
    }

    /// move toward older (up the file). saturates at the oldest one loaded.
    pub fn up(&mut self, n: usize, last: usize) {
        self.cursor = self.cursor.saturating_add(n).min(last);
    }

    /// `G` — the newest message.
    pub fn to_newest(&mut self) {
        self.cursor = 0;
    }

    /// `gg` — the oldest message still in the ring.
    pub fn to_oldest(&mut self, last: usize) {
        self.cursor = last;
    }

    /// the selected range as (newest_index, oldest_index), inclusive. `None`
    /// outside visual mode.
    pub fn selection(&self) -> Option<(usize, usize)> {
        let a = self.visual?;
        Some((a.min(self.cursor), a.max(self.cursor)))
    }

    /// is this message inside the visual selection?
    pub fn selected(&self, idx: usize) -> bool {
        self.selection()
            .is_some_and(|(lo, hi)| (lo..=hi).contains(&idx))
    }

    /// pull the viewport so the cursor is on screen. `count` is what the last
    /// frame fit; heights vary, so this can be a row out for one frame and then
    /// settles — far cheaper than re-running layout inside the key handler.
    pub fn keep_visible(&mut self) {
        let count = self.view.get().count.max(1);
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + count {
            self.scroll = self.cursor + 1 - count;
        }
    }

    /// new messages arrived. while the user is reading scrollback we shift the
    /// viewport and cursor by the same amount, so the pane holds still and the
    /// cursor stays on the message it was on instead of sliding underneath.
    /// once they're back at the bottom, chat follows live again.
    pub fn absorb_new(&mut self, added: usize, last: usize) {
        if added == 0 || self.following() {
            return;
        }
        self.cursor = self.cursor.saturating_add(added).min(last);
        self.scroll = self.scroll.saturating_add(added).min(last);
        if let Some(a) = self.visual {
            self.visual = Some(a.saturating_add(added).min(last));
        }
    }

    /// clamp to a ring that may have shrunk (channel switched, messages evicted).
    pub fn clamp(&mut self, last: usize) {
        self.cursor = self.cursor.min(last);
        self.scroll = self.scroll.min(last);
        if let Some(a) = self.visual {
            self.visual = Some(a.min(last));
        }
    }
}

/// find the next match from `from`, exclusive, wrapping once (vi's `wrapscan`).
/// `is_match(i)` tests the message at index-back-from-newest `i`. returns the
/// index, or None if nothing in the ring matches.
pub fn search(
    len: usize,
    from: usize,
    dir: Dir,
    is_match: impl Fn(usize) -> bool,
) -> Option<usize> {
    if len == 0 {
        return None;
    }
    // walk every other index once, in order, starting one step past the cursor.
    for step in 1..=len {
        let i = match dir {
            // forward = toward newer = decreasing index
            Dir::Fwd => (from + len - (step % len)) % len,
            Dir::Back => (from + step) % len,
        };
        if is_match(i) {
            return Some(i);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vi_with(count: usize) -> Vi {
        let v = Vi::default();
        v.view.set(View { count });
        v
    }

    #[test]
    fn starts_following_live() {
        let v = Vi::default();
        assert!(v.following(), "a fresh pane pins to the newest message");
    }

    #[test]
    fn j_and_k_read_as_down_and_up_the_file() {
        let mut v = Vi::default();
        v.up(3, 100); // k — toward older, up the pane
        assert_eq!(v.cursor, 3);
        v.down(1); // j — back toward newer
        assert_eq!(v.cursor, 2);
        assert!(!v.following());
    }

    #[test]
    fn motions_saturate_at_both_ends() {
        let mut v = Vi::default();
        v.down(50); // already newest
        assert_eq!(v.cursor, 0);
        v.up(999, 9); // only 10 messages loaded
        assert_eq!(v.cursor, 9);
    }

    #[test]
    fn g_and_shift_g_jump_to_the_ends() {
        let mut v = Vi::default();
        v.to_oldest(42);
        assert_eq!(v.cursor, 42);
        v.to_newest();
        assert_eq!(v.cursor, 0);
    }

    // ---- counts ----------------------------------------------------------

    #[test]
    fn counts_accumulate_and_are_consumed_once() {
        let mut v = Vi::default();
        assert!(v.push_digit(1));
        assert!(v.push_digit(2));
        assert_eq!(v.take_count(), 12);
        assert_eq!(v.take_count(), 1, "count resets after use");
    }

    #[test]
    fn leading_zero_is_not_a_count() {
        let mut v = Vi::default();
        assert!(!v.push_digit(0), "bare 0 is a motion in vi, not a count");
        assert_eq!(v.count, 0);
        v.push_digit(1);
        assert!(v.push_digit(0), "0 counts once a number is under way");
        assert_eq!(v.take_count(), 10);
    }

    // ---- viewport --------------------------------------------------------

    #[test]
    fn viewport_follows_the_cursor_up_and_back_down() {
        let mut v = vi_with(10);
        v.up(15, 100); // cursor older than the top of the view
        v.keep_visible();
        assert_eq!(v.scroll, 6, "pulls the window so cursor 15 is the top row");
        assert!(v.cursor >= v.scroll && v.cursor < v.scroll + 10);

        v.down(15); // back to the newest
        v.keep_visible();
        assert_eq!(v.scroll, 0);
    }

    #[test]
    fn viewport_does_not_move_while_the_cursor_is_on_screen() {
        let mut v = vi_with(10);
        v.up(5, 100);
        v.keep_visible();
        assert_eq!(v.scroll, 0, "cursor 5 already fits in a 10-row view");
    }

    // ---- live chat arriving under the reader ------------------------------

    #[test]
    fn scrollback_holds_still_when_messages_arrive() {
        let mut v = vi_with(10);
        v.up(20, 500); // reading history
        v.keep_visible();
        let (c, s) = (v.cursor, v.scroll);
        v.absorb_new(3, 500);
        assert_eq!(v.cursor, c + 3, "cursor stays on the same message");
        assert_eq!(v.scroll, s + 3, "and the pane does not jump");
    }

    #[test]
    fn live_view_keeps_following_when_messages_arrive() {
        let mut v = vi_with(10);
        v.absorb_new(5, 500);
        assert!(
            v.following(),
            "at the bottom, new chat scrolls in as normal"
        );
        assert_eq!(v.cursor, 0);
    }

    #[test]
    fn visual_anchor_travels_with_the_selection() {
        let mut v = vi_with(10);
        v.up(10, 500);
        v.visual = Some(v.cursor);
        v.up(2, 500);
        let before = v.selection().unwrap();
        v.absorb_new(4, 500);
        let after = v.selection().unwrap();
        assert_eq!((after.0 - before.0, after.1 - before.1), (4, 4));
    }

    // ---- visual selection -------------------------------------------------

    #[test]
    fn selection_covers_both_directions() {
        let mut v = Vi::default();
        v.cursor = 5;
        v.visual = Some(2);
        assert_eq!(v.selection(), Some((2, 5)));
        assert!(v.selected(2) && v.selected(4) && v.selected(5));
        assert!(!v.selected(1) && !v.selected(6));

        v.cursor = 1; // dragged past the anchor
        assert_eq!(v.selection(), Some((1, 2)));
    }

    #[test]
    fn no_selection_outside_visual_mode() {
        let mut v = Vi::default();
        v.cursor = 3;
        assert_eq!(v.selection(), None);
        assert!(!v.selected(3));
    }

    // ---- search -----------------------------------------------------------

    #[test]
    fn forward_search_walks_toward_newer() {
        // 10 messages; 2 and 7 match. from 8, forward (toward newer) hits 7.
        let hit = |i: usize| i == 2 || i == 7;
        assert_eq!(search(10, 8, Dir::Fwd, hit), Some(7));
        assert_eq!(search(10, 7, Dir::Fwd, hit), Some(2));
    }

    #[test]
    fn backward_search_walks_toward_older() {
        let hit = |i: usize| i == 2 || i == 7;
        assert_eq!(search(10, 0, Dir::Back, hit), Some(2));
        assert_eq!(search(10, 2, Dir::Back, hit), Some(7));
    }

    #[test]
    fn search_wraps_once_like_wrapscan() {
        let hit = |i: usize| i == 9;
        // going forward from 5 runs out of newer messages and wraps to the oldest
        assert_eq!(search(10, 5, Dir::Fwd, hit), Some(9));
    }

    #[test]
    fn search_finds_the_cursor_message_only_after_a_full_lap() {
        let hit = |i: usize| i == 4;
        assert_eq!(
            search(10, 4, Dir::Fwd, hit),
            Some(4),
            "wraps all the way round"
        );
        assert_eq!(search(10, 3, Dir::Fwd, hit), Some(4));
    }

    #[test]
    fn search_reports_no_match_and_never_hangs() {
        assert_eq!(search(10, 3, Dir::Fwd, |_| false), None);
        assert_eq!(search(0, 0, Dir::Fwd, |_| true), None, "empty ring");
    }

    #[test]
    fn clamp_survives_a_shrinking_ring() {
        let mut v = Vi::default();
        v.cursor = 90;
        v.scroll = 80;
        v.visual = Some(95);
        v.clamp(10); // switched to a channel with 11 messages
        assert_eq!((v.cursor, v.scroll, v.visual), (10, 10, Some(10)));
    }
}
