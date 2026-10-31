//! a crossterm backend that holds ratatui's diff instead of writing it, so the
//! caller can decide how it reaches the terminal: as-is, or after a real
//! terminal scroll that already moved the pixels and text (see scroll.rs).
//! everything but `draw` is passed straight through.

use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use std::io::{self, Stdout, Write};

use crate::scroll::Shift;

type Held = Vec<(u16, u16, Cell)>;

pub struct HeldBackend {
    inner: CrosstermBackend<Stdout>,
    held: Held,
}

impl HeldBackend {
    pub fn new() -> Self {
        Self {
            inner: CrosstermBackend::new(io::stdout()),
            held: Vec::new(),
        }
    }

    /// write the held diff exactly as ratatui produced it.
    pub fn release(&mut self) -> io::Result<()> {
        let held = std::mem::take(&mut self.held);
        self.paint(&held)
    }

    /// scroll the rows `shift` names with a real terminal scroll, then write
    /// `cells` (the diff against the scrolled screen) instead of the held diff.
    pub fn scroll_then_paint(
        &mut self,
        shift: Shift,
        cells: &[(u16, u16, Cell)],
    ) -> io::Result<()> {
        self.held.clear();
        // reset first: the scroll fills new rows with the current background.
        // DECSTBM homes the cursor and the region is reset right after, so
        // nothing leaks into the cell writes that follow (each starts with a move).
        write!(
            &mut self.inner,
            "\x1b[0m\x1b[{};{}r\x1b[{}S\x1b[r",
            shift.top + 1,
            shift.top + shift.h,
            shift.k
        )?;
        self.paint(cells)
    }

    fn paint(&mut self, cells: &[(u16, u16, Cell)]) -> io::Result<()> {
        self.inner.draw(cells.iter().map(|(x, y, c)| (*x, *y, c)))?;
        Backend::flush(&mut self.inner)
    }
}

impl Backend for HeldBackend {
    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.held.extend(content.map(|(x, y, c)| (x, y, c.clone())));
        Ok(())
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.inner.append_lines(n)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> io::Result<Size> {
        self.inner.size()
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.inner)
    }
}
