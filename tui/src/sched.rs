//! when chat may repaint. a busy channel scrolls the whole chat area on every
//! line, and over mosh (slow, lossy phone link) screen diffs queue ahead of the
//! keystroke echo. so chat updates are throttled leading-edge: the first line
//! after a lull shows at once, the rest are batched into one frame per gap — a
//! wider gap while the composer is being typed in, so a key repaints only the
//! input line.

use std::time::{Duration, Instant};

/// min time between chat repaints on a busy channel (4fps).
pub const CHAT_GAP: Duration = Duration::from_millis(250);
/// the same, while typing in the composer (2fps).
pub const TYPING_GAP: Duration = Duration::from_millis(500);
/// how long after the last key the composer still counts as being typed in.
pub const TYPING_HOLD: Duration = Duration::from_millis(1500);
/// how often the loop looks for new chat when nothing else wakes it — the
/// latency a line on a quiet channel can wait.
pub const POLL: Duration = Duration::from_millis(50);

#[derive(Default)]
pub struct ChatGate {
    last: Option<Instant>,
    key: Option<Instant>,
}

impl ChatGate {
    pub fn note_key(&mut self, now: Instant) {
        self.key = Some(now);
    }

    pub fn typing(&self, now: Instant) -> bool {
        self.key
            .is_some_and(|k| now.saturating_duration_since(k) < TYPING_HOLD)
    }

    pub fn gap(&self, now: Instant) -> Duration {
        if self.typing(now) {
            TYPING_GAP
        } else {
            CHAT_GAP
        }
    }

    /// may queued chat be applied (and so repainted) right now?
    pub fn due(&self, now: Instant) -> bool {
        self.last
            .is_none_or(|l| now.saturating_duration_since(l) >= self.gap(now))
    }

    /// chat was just applied.
    pub fn mark(&mut self, now: Instant) {
        self.last = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn first_line_after_a_lull_is_immediate() {
        let t = Instant::now();
        let mut g = ChatGate::default();
        assert!(g.due(t));
        g.mark(t);
        assert!(g.due(at(t, 10_000)));
    }

    #[test]
    fn burst_is_coalesced_to_one_frame_per_gap() {
        let t = Instant::now();
        let mut g = ChatGate::default();
        g.mark(t);
        assert!(!g.due(at(t, 1)));
        assert!(!g.due(at(t, CHAT_GAP.as_millis() as u64 - 1)));
        assert!(g.due(at(t, CHAT_GAP.as_millis() as u64)));
    }

    #[test]
    fn typing_widens_the_gap_then_relaxes() {
        let t = Instant::now();
        let mut g = ChatGate::default();
        g.note_key(t);
        g.mark(t);
        assert!(g.typing(at(t, 100)));
        assert!(!g.due(at(t, CHAT_GAP.as_millis() as u64 + 1)));
        assert!(g.due(at(t, TYPING_GAP.as_millis() as u64)));
        let later = at(t, TYPING_HOLD.as_millis() as u64);
        assert!(!g.typing(later));
        assert_eq!(g.gap(later), CHAT_GAP);
    }

    #[test]
    fn keys_never_defer_themselves() {
        // input priority: due() only gates chat, so a key arriving mid-gap
        // changes the typing state but is never itself queued.
        let t = Instant::now();
        let mut g = ChatGate::default();
        g.mark(t);
        g.note_key(at(t, 5));
        assert!(!g.due(at(t, 6)));
        assert!(g.typing(at(t, 6)));
    }
}
