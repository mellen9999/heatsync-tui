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
        });
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
            let same = self.last.get(&q.rect) == Some(&ident);
            next.insert(q.rect, ident.clone());
            if !same {
                emits.push(Queued {
                    rect: q.rect,
                    key: ident.0,
                    idx: ident.1,
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
