//! message-cursor math. a cursor names a line by its absolute sequence number
//! (how many lines the channel had appended before it), which stays put while
//! the ring evicts old lines, grows at the live end, or gets history
//! backfilled in front — so the view stays anchored without any bookkeeping on
//! arrival. `seq` = lines ever appended, `len` = lines held now; the held
//! lines are `seq - len .. seq`.

/// absolute number of the held line at `idx`.
pub fn abs_of(seq: u64, len: usize, idx: usize) -> u64 {
    seq - len as u64 + idx as u64
}

/// index of absolute line `abs`, clamped into what is still held (an evicted
/// line snaps to the oldest, a future one to the newest). `None` when empty.
pub fn index_of(seq: u64, len: usize, abs: u64) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let first = seq - len as u64;
    Some(abs.saturating_sub(first).min(len as u64 - 1) as usize)
}

/// move a cursor by `delta` lines, staying inside the held lines.
pub fn step(seq: u64, len: usize, abs: u64, delta: i64) -> u64 {
    let Some(i) = index_of(seq, len, abs) else {
        return abs;
    };
    let to = (i as i64).saturating_add(delta).clamp(0, len as i64 - 1);
    abs_of(seq, len, to as usize)
}

/// lines newer than the cursor — what arrived while the view is parked.
pub fn newer(seq: u64, len: usize, abs: u64) -> u64 {
    match index_of(seq, len, abs) {
        Some(i) => (len - 1 - i) as u64,
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_numbers_survive_eviction_growth_and_backfill() {
        // 5 lines held, 105 ever seen → held are 100..105
        assert_eq!(abs_of(105, 5, 0), 100);
        assert_eq!(abs_of(105, 5, 4), 104);
        // a new line arrives and the ring evicts one: same line, same number
        let cursor = abs_of(105, 5, 3); // 103
        assert_eq!(index_of(106, 5, cursor), Some(2));
        // history backfilled in front: len grows, seq doesn't
        assert_eq!(index_of(105, 9, cursor), Some(7));
        assert_eq!(abs_of(105, 9, 7), cursor);
    }

    #[test]
    fn index_clamps_to_what_is_held() {
        assert_eq!(index_of(105, 5, 10), Some(0), "evicted → oldest");
        assert_eq!(index_of(105, 5, 999), Some(4), "future → newest");
        assert_eq!(index_of(0, 0, 0), None);
    }

    #[test]
    fn step_moves_and_stops_at_both_ends() {
        let (seq, len) = (105, 5);
        let mid = abs_of(seq, len, 2);
        assert_eq!(step(seq, len, mid, -1), 101);
        assert_eq!(step(seq, len, mid, 1), 103);
        assert_eq!(step(seq, len, mid, -50), 100, "top");
        assert_eq!(step(seq, len, mid, 50), 104, "bottom");
        assert_eq!(
            step(seq, len, 3, 1),
            101,
            "an evicted cursor snaps in first"
        );
        assert_eq!(step(0, 0, 7, 3), 7, "empty buffer: unchanged");
        assert_eq!(step(seq, len, mid, i64::MIN), 100);
        assert_eq!(step(seq, len, mid, i64::MAX), 104);
    }

    #[test]
    fn newer_counts_lines_below_the_cursor() {
        assert_eq!(newer(105, 5, 104), 0);
        assert_eq!(newer(105, 5, 100), 4);
        // parked at 102 while 3 more arrive (ring evicts 3)
        // parked at 102 while 3 more arrive: 103..108 held, 102 evicted → oldest
        assert_eq!(newer(108, 5, 102), 4);
        assert_eq!(newer(108, 5, 105), 2);
        assert_eq!(newer(0, 0, 0), 0);
    }
}
