//! sixel placement queue. ratatui-image would put a whole sixel escape into one
//! buffer cell; ratatui's diff then sees a cell thousands of columns wide and
//! re-emits every cell after it on EVERY draw — under tmux each re-sent sixel
//! repaints the image, a visible flicker even when nothing changed. so draw
//! reserves the cells with NBSP (plain text, diffs normally) and queues the
//! image here; after the text flushes, only placements that actually changed are
//! written.

use ratatui::layout::Rect;
use std::collections::HashMap;

/// an image to put at `rect`: which emote, and which animation frame of it.
pub struct Queued {
    pub rect: Rect,
    pub key: String,
    pub idx: usize,
    /// same emote, same rect, only the animation frame advanced: the cells
    /// under it are untouched, so the frame can be painted in place.
    pub in_place: bool,
}

#[derive(Default)]
pub struct Blitter {
    area: Rect,
    queue: Vec<Queued>,
    /// what the terminal shows at each rect right now (emote key + frame).
    last: HashMap<Rect, (String, usize)>,
}

impl Blitter {
    pub fn push(&mut self, rect: Rect, key: &str, idx: usize) {
        self.queue.push(Queued {
            rect,
            key: key.to_owned(),
            idx,
            in_place: false,
        });
    }

    /// the terminal scrolled rows `top..top+h` up by `k`: move what we know is on
    /// screen with it. an image half scrolled off the top is forgotten (the
    /// frame's diff rewrites its remaining rows). false (nothing changed) when
    /// an image straddles the region's own edge — the caller then doesn't scroll.
    pub fn shift_up(&mut self, top: u16, h: u16, k: u16) -> bool {
        let (end, cut) = (top + h, top + k);
        let mut next = HashMap::with_capacity(self.last.len());
        for (r, id) in &self.last {
            let (y0, y1) = (r.y, r.y + r.height);
            if y1 <= top || y0 >= end {
                next.insert(*r, id.clone()); // untouched by the scroll
            } else if y0 < top || y1 > end {
                return false; // straddles an edge of the region
            } else if y0 >= cut {
                next.insert(Rect { y: y0 - k, ..*r }, id.clone());
            } // else: scrolled off the top, gone
        }
        self.last = next;
        true
    }

    /// the placements that must be written this frame. a rect whose emote and
    /// frame match what is already on screen is skipped: its NBSP cells did not
    /// change, so ratatui left the pixels alone. anything not queued this frame
    /// is forgotten — ratatui wrote text over its cells and the terminal erased
    /// it. a size change forgets everything (ratatui repaints the whole screen).
    pub fn take_emits(&mut self, area: Rect) -> Vec<Queued> {
        if area != self.area {
            self.area = area;
            self.last.clear();
        }
        let mut next = HashMap::with_capacity(self.queue.len());
        let mut emits = Vec::new();
        for q in std::mem::take(&mut self.queue) {
            let ident = (q.key, q.idx);
            let prev = self.last.get(&q.rect);
            let same = prev == Some(&ident);
            let in_place = prev.is_some_and(|p| p.0 == ident.0);
            next.insert(q.rect, ident.clone());
            if !same {
                emits.push(Queued {
                    rect: q.rect,
                    key: ident.0,
                    idx: ident.1,
                    in_place,
                });
            }
        }
        self.last = next;
        emits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Rect = Rect::new(0, 0, 80, 24);

    fn r(x: u16, y: u16) -> Rect {
        Rect::new(x, y, 2, 2)
    }

    fn frame(b: &mut Blitter, area: Rect, ps: &[(Rect, &str, usize)]) -> usize {
        for (rect, key, idx) in ps {
            b.push(*rect, key, *idx);
        }
        b.take_emits(area).len()
    }

    fn in_place(b: &mut Blitter, ps: &[(Rect, &str, usize)]) -> Vec<bool> {
        for (rect, key, idx) in ps {
            b.push(*rect, key, *idx);
        }
        b.take_emits(A).iter().map(|q| q.in_place).collect()
    }

    #[test]
    fn only_a_frame_advance_is_in_place() {
        let mut b = Blitter::default();
        assert_eq!(in_place(&mut b, &[(r(1, 1), "k", 0)]), [false]); // first
        assert_eq!(in_place(&mut b, &[(r(1, 1), "k", 1)]), [true]); // frame++
        assert_eq!(in_place(&mut b, &[(r(1, 2), "k", 1)]), [false]); // moved
        assert_eq!(in_place(&mut b, &[(r(1, 2), "j", 1)]), [false]); // other emote
        assert_eq!(in_place(&mut b, &[]), Vec::<bool>::new());
        assert_eq!(in_place(&mut b, &[(r(1, 2), "j", 2)]), [false]); // overwritten
    }

    #[test]
    fn scroll_moves_known_placements() {
        let mut b = Blitter::default();
        frame(
            &mut b,
            A,
            &[(r(1, 2), "k", 0), (r(1, 5), "j", 0), (r(1, 20), "f", 0)],
        );
        // region rows 2..12 scrolled up 3: k scrolls off, j lands on row 2
        assert!(b.shift_up(2, 10, 3));
        assert_eq!(
            frame(
                &mut b,
                A,
                &[(r(1, 2), "j", 0), (r(1, 20), "f", 0), (r(1, 9), "n", 0)]
            ),
            1, // only the new image at the bottom is written
        );
        // half scrolled off: forgotten, so it is re-sent if it is still wanted
        let mut b = Blitter::default();
        frame(&mut b, A, &[(r(1, 3), "k", 0)]);
        assert!(b.shift_up(2, 10, 2)); // rows 3..5 cross the cut at row 4
        assert_eq!(frame(&mut b, A, &[(r(1, 1), "k", 0)]), 1);
        // an image across the region's own edge forbids scrolling
        let mut b = Blitter::default();
        frame(&mut b, A, &[(r(1, 1), "k", 0)]);
        assert!(!b.shift_up(2, 10, 2));
    }

    #[test]
    fn unchanged_skips() {
        let mut b = Blitter::default();
        assert_eq!(frame(&mut b, A, &[(r(1, 1), "k", 0)]), 1);
        assert_eq!(frame(&mut b, A, &[(r(1, 1), "k", 0)]), 0);
    }

    #[test]
    fn moved_emits() {
        let mut b = Blitter::default();
        frame(&mut b, A, &[(r(1, 1), "k", 0)]);
        assert_eq!(frame(&mut b, A, &[(r(1, 2), "k", 0)]), 1);
    }

    #[test]
    fn frame_advance_emits() {
        let mut b = Blitter::default();
        frame(&mut b, A, &[(r(1, 1), "k", 0)]);
        assert_eq!(frame(&mut b, A, &[(r(1, 1), "k", 1)]), 1);
    }

    #[test]
    fn other_emote_emits() {
        let mut b = Blitter::default();
        frame(&mut b, A, &[(r(1, 1), "k", 0)]);
        assert_eq!(frame(&mut b, A, &[(r(1, 1), "j", 0)]), 1);
    }

    #[test]
    fn overwritten_rect_emits_when_it_returns() {
        let mut b = Blitter::default();
        frame(&mut b, A, &[(r(1, 1), "k", 0)]);
        // a frame without it: text went over the cells
        assert_eq!(frame(&mut b, A, &[]), 0);
        assert_eq!(frame(&mut b, A, &[(r(1, 1), "k", 0)]), 1);
    }

    #[test]
    fn resize_reemits() {
        let mut b = Blitter::default();
        frame(&mut b, A, &[(r(1, 1), "k", 0)]);
        assert_eq!(
            frame(&mut b, Rect::new(0, 0, 90, 24), &[(r(1, 1), "k", 0)]),
            1
        );
    }
}
