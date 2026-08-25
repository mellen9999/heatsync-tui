//! How often to ask for the next frame.
//!
//! A chat client spends most of its life not being looked at, so this is the
//! difference between a background window costing nothing and costing a core.
//! Found the hard way: with the monitor off, eframe spun at ~100% CPU —
//! it runs `logic()` rather than `ui()` while hidden, and keeps going.
//!
//! Part of that is not ours to fix. eframe decides visibility from
//! `WindowEvent::Occluded`, which winit does not emit on Wayland, so
//! `ViewportInfo::visible()` returns `None` there and eframe assumes visible.
//! What IS ours is never *asking* for frames we do not need, which is also just
//! correct behaviour: nobody needs emotes animating at 30fps in a window they
//! are not looking at.
//!
//! Kept as a pure function so the policy is testable without a window — the
//! condition that exposed the bug is one a windowed test cannot easily reach.

use std::time::Duration;

/// What the platform is telling us about the window. `None` means it did not
/// say, which is the normal case for occlusion on Wayland.
#[derive(Clone, Copy, Debug, Default)]
pub struct Visibility {
    pub visible: Option<bool>,
    pub focused: Option<bool>,
}

/// Background windows still animate, just slowly enough to be free. Fast enough
/// that un-focusing and re-focusing does not show a frozen frame.
pub const UNFOCUSED_MS: u32 = 500;

/// Delay until the next requested repaint, or `None` to request nothing at all
/// and let an input event wake us.
///
/// `animation_tick_ms` is the shortest frame delay anything on screen needs,
/// or `None` when nothing is animating.
pub fn repaint_delay(vis: Visibility, animation_tick_ms: Option<u32>) -> Option<Duration> {
    // Definitely not on screen: draw nothing. An input event or a visibility
    // change will wake us.
    if vis.visible == Some(false) {
        return None;
    }

    let tick = animation_tick_ms?;

    // On screen but not focused — animate, slowly. Unknown focus is treated as
    // focused so a platform that never reports it is not permanently throttled.
    if vis.focused == Some(false) {
        return Some(Duration::from_millis(tick.max(UNFOCUSED_MS) as u64));
    }

    Some(Duration::from_millis(tick as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vis(visible: Option<bool>, focused: Option<bool>) -> Visibility {
        Visibility { visible, focused }
    }

    #[test]
    fn a_hidden_window_asks_for_nothing() {
        assert_eq!(repaint_delay(vis(Some(false), Some(false)), Some(16)), None);
        // even with something animating, and even if focus somehow says true
        assert_eq!(repaint_delay(vis(Some(false), Some(true)), Some(16)), None);
    }

    #[test]
    fn nothing_animating_asks_for_nothing() {
        assert_eq!(repaint_delay(vis(Some(true), Some(true)), None), None);
    }

    #[test]
    fn a_focused_window_runs_at_the_animation_tick() {
        assert_eq!(
            repaint_delay(vis(Some(true), Some(true)), Some(16)),
            Some(Duration::from_millis(16))
        );
    }

    #[test]
    fn an_unfocused_window_is_throttled_but_not_frozen() {
        let d = repaint_delay(vis(Some(true), Some(false)), Some(16)).unwrap();
        assert_eq!(d, Duration::from_millis(UNFOCUSED_MS as u64));
        assert!(d > Duration::from_millis(16), "must be slower than focused");
    }

    #[test]
    fn throttling_never_speeds_a_slow_animation_up() {
        // a 2s-per-frame emote stays at 2s in the background, not 500ms
        assert_eq!(
            repaint_delay(vis(Some(true), Some(false)), Some(2000)),
            Some(Duration::from_millis(2000))
        );
    }

    #[test]
    fn unknown_visibility_is_treated_as_visible() {
        // Wayland does not report occlusion; assuming hidden would freeze the
        // window for everyone on that platform.
        assert_eq!(
            repaint_delay(vis(None, Some(true)), Some(16)),
            Some(Duration::from_millis(16))
        );
    }

    #[test]
    fn unknown_focus_is_treated_as_focused() {
        assert_eq!(
            repaint_delay(vis(Some(true), None), Some(16)),
            Some(Duration::from_millis(16))
        );
    }

    #[test]
    fn hidden_beats_unknown_focus() {
        assert_eq!(repaint_delay(vis(Some(false), None), Some(16)), None);
    }
}
