//! chat scrolls by one new message at a time: every row moves up and only the
//! bottom is new. redrawing that cell by cell makes the terminal re-place every
//! image. instead, when the chat body is provably last frame's body shifted up
//! by k rows, scroll the region for real — text and sixels move by themselves —
//! and write only what differs from the scrolled screen.
//!
//! the proof is exact cell equality against the frame ratatui drew last time;
//! anything that doesn't match falls back to the plain diff.

use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::Rect;

/// scroll `h` rows starting at row `top` up by `k`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shift {
    pub top: u16,
    pub h: u16,
    pub k: u16,
}

/// last frame's buffer + the scroll worth doing for this one.
#[derive(Default)]
pub struct Scroller {
    prev: Option<Buffer>,
}

pub type Cells = Vec<(u16, u16, Cell)>;

impl Scroller {
    /// feed every drawn frame. `body` is the full-width chat region when
    /// scrolling is allowed (None otherwise). returns the shift and the diff
    /// against the scrolled screen when scrolling is a win.
    pub fn observe(&mut self, next: &Buffer, body: Option<Rect>) -> Option<(Shift, Cells)> {
        let plan = match (&self.prev, body) {
            (Some(prev), Some(body)) => plan(prev, next, body),
            _ => None,
        };
        self.prev = body.map(|_| next.clone());
        plan
    }
}

fn plan(prev: &Buffer, next: &Buffer, body: Rect) -> Option<(Shift, Cells)> {
    let mut best: Option<(Shift, Cells)> = None;
    for s in candidates(prev, next, body) {
        let cells: Cells = shifted(prev, s)
            .diff(next)
            .into_iter()
            .map(|(x, y, c)| (x, y, c.clone()))
            .collect();
        if best.as_ref().is_none_or(|(_, b)| cells.len() < b.len()) {
            best = Some((s, cells));
        }
    }
    // only worth it if the scroll actually saves cells.
    let (s, cells) = best?;
    (cells.len() < prev.diff(next).len()).then_some((s, cells))
}

/// every k for which the bottom of next's body is prev's body moved up k. the
/// top rows may differ: chat drops whole messages, so the one that no longer
/// fits leaves a blank gap there — the diff against the scrolled screen
/// rewrites those rows (and erases whatever is left of a half-scrolled image).
fn candidates(prev: &Buffer, next: &Buffer, body: Rect) -> Vec<Shift> {
    let area = next.area;
    if prev.area != area
        || area.x != 0
        || area.y != 0
        || body.x != 0
        || body.width != area.width
        || body.height < 3
        || body.bottom() > area.bottom()
    {
        return Vec::new();
    }
    let w = usize::from(area.width);
    fn row(b: &Buffer, y: u16, w: usize) -> &[Cell] {
        let at = usize::from(y) * w;
        &b.content[at..at + w]
    }
    (1..=body.height / 2)
        .filter(|&k| {
            // rows at the bottom of the kept span must match; at least two, so a
            // lone blank row can't vouch for a scroll.
            let kept = body.height - k;
            let run = (0..kept)
                .rev()
                .take_while(|&i| row(next, body.y + i, w) == row(prev, body.y + i + k, w))
                .count();
            run >= 2.min(usize::from(kept))
        })
        .map(|k| Shift {
            top: body.y,
            h: body.height,
            k,
        })
        .collect()
}

/// what the screen holds after the scroll: rows moved up, fresh rows blank.
fn shifted(prev: &Buffer, s: Shift) -> Buffer {
    let w = usize::from(prev.area.width);
    let mut b = prev.clone();
    let at = |y: u16| usize::from(y - prev.area.y) * w;
    for i in 0..s.h {
        let to = at(s.top + i);
        if i + s.k < s.h {
            let from = at(s.top + i + s.k);
            b.content[to..to + w].clone_from_slice(&prev.content[from..from + w]);
        } else {
            b.content[to..to + w].fill(Cell::default());
        }
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u16 = 12;
    const H: u16 = 10;

    /// tiny deterministic generator — no dependency for a test.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self, n: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) % n
        }
    }

    /// a screen: header row, chat body (rows 1..H-2), footer rows.
    fn screen(body: &[String]) -> Buffer {
        let mut b = Buffer::empty(Rect::new(0, 0, W, H));
        b.set_string(0, 0, "HEAD", ratatui::style::Style::default());
        for (i, line) in body.iter().enumerate() {
            b.set_string(0, 1 + i as u16, line, ratatui::style::Style::default());
        }
        b.set_string(0, H - 1, "FOOT", ratatui::style::Style::default());
        b
    }

    const BODY: Rect = Rect::new(0, 1, W, H - 2);

    /// the terminal, as a grid: applies a scroll and cell writes.
    fn apply(screen: &mut Buffer, s: Option<Shift>, cells: &Cells) {
        if let Some(s) = s {
            *screen = shifted(screen, s);
        }
        for (x, y, c) in cells {
            screen[(*x, *y)] = c.clone();
        }
    }

    #[test]
    fn new_messages_scroll_and_match_a_full_redraw() {
        let mut rng = Rng(7);
        let rows = usize::from(BODY.height);
        let mut log: Vec<String> = Vec::new();
        let mut sc = Scroller::default();
        let mut term = Buffer::empty(Rect::new(0, 0, W, H));
        let mut scrolled = 0;
        for n in 0..400 {
            // 1-3 new lines most frames; sometimes edit an old line or change nothing.
            match rng.next(6) {
                0 if !log.is_empty() => {
                    let i = rng.next(log.len() as u64) as usize;
                    log[i] = format!("edit{n}");
                }
                1 => {}
                _ => {
                    for _ in 0..=rng.next(3) {
                        log.push(format!("m{n}-{}", rng.next(1000)));
                    }
                }
            }
            let mut tail: Vec<String> = log[log.len().saturating_sub(rows)..].to_vec();
            if rng.next(3) == 0 {
                tail[0].clear(); // a message too tall for the top row is dropped whole
            }
            let next = screen(&tail);
            let before = term.clone();
            match sc.observe(&next, Some(BODY)) {
                Some((s, cells)) => {
                    scrolled += 1;
                    apply(&mut term, Some(s), &cells);
                }
                None => {
                    let cells: Cells = before
                        .diff(&next)
                        .into_iter()
                        .map(|(x, y, c)| (x, y, c.clone()))
                        .collect();
                    apply(&mut term, None, &cells);
                }
            }
            assert_eq!(term, next, "frame {n} diverged from a full redraw");
        }
        assert!(scrolled > 50, "scroll path never taken ({scrolled})");
    }

    #[test]
    fn region_must_be_full_width_and_unchanged_elsewhere() {
        let lines: Vec<String> = (0..9).map(|i| format!("line{i}")).collect();
        let a = screen(&lines[..8]);
        let b = screen(&lines[1..]);
        let mut sc = Scroller::default();
        assert!(sc.observe(&a, Some(BODY)).is_none()); // nothing to compare yet
        let (s, _) = sc.observe(&b, Some(BODY)).expect("shift by one");
        assert_eq!((s.top, s.h, s.k), (1, H - 2, 1));
        // narrower than the screen (side tab bar): never scrolls
        let mut sc = Scroller::default();
        sc.observe(&a, Some(Rect::new(0, 1, W - 2, H - 2)));
        assert!(sc
            .observe(&b, Some(Rect::new(0, 1, W - 2, H - 2)))
            .is_none());
        // scrolling off: no state, no scroll
        let mut sc = Scroller::default();
        sc.observe(&a, None);
        assert!(sc.observe(&b, None).is_none());
    }
}
