//! End-to-end tests that drive the real UI.
//!
//! Everything else in this crate tests a pure function. These build the actual
//! widget tree through `egui_kittest`, which is built on AccessKit — assertions
//! read the accessibility tree and widget geometry rather than pixels, so there
//! is **no gpu involved** and this runs unchanged on all three CI runners. The
//! app itself is gpu-accelerated through glow; that is unrelated to how it is
//! asserted on here.
//!
//! Each test maps to a claim that was originally verified by hand, once, on one
//! machine. The point of having them is that none of those claims can quietly
//! stop being true.
//!
//! Two things about the harness API, learned the hard way and worth keeping:
//! `query_by_*` **panics when more than one node matches** (use `query_all_by_*`
//! and count), and a node matched by `*_by_label_contains` may still return
//! `None` from `label()` — so assert on the match itself, never on re-reading
//! the label back off the node.

#![cfg(test)]

use egui::Vec2;
use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;

use crate::chat::{Message, View};
use crate::emote::Cache;
use crate::paint::Paint;
use heatsync_core::emote::{Emote, EmoteSet};

const WIDE: Vec2 = Vec2::new(900.0, 600.0);
const NARROW: Vec2 = Vec2::new(320.0, 600.0);

fn set() -> EmoteSet {
    EmoteSet::from_list([Emote {
        name: "KEKW".into(),
        url: "synthetic://KEKW".into(),
        provider: "7tv".into(),
        id: "KEKW".into(),
        animated: false,
        zero_width: false,
    }])
}

/// `n` messages, each with a username that is unique and easy to query for.
fn msgs(n: usize, text: &str) -> Vec<Message> {
    let s = set();
    (0..n)
        .map(|i| Message::parse(&format!("user{i:05}"), None, text, &s, (i % 21) as f64))
        .collect()
}

fn harness(messages: Vec<Message>, size: Vec2) -> Harness<'static, ()> {
    let mut view = View::default();
    let cache = Cache::default();
    let mut h = Harness::builder().with_size(size).build_ui(move |ui| {
        view.show(ui, &messages, &cache, 0);
    });
    h.run();
    h
}

/// How many message rows are on screen — counted by their unique usernames.
fn rows_on_screen(h: &Harness<'_, ()>) -> usize {
    h.query_all_by_label_contains("user").count()
}

#[test]
fn a_ten_thousand_message_backlog_only_renders_a_screenful() {
    let h = harness(msgs(10_000, "hello there"), WIDE);
    let rows = rows_on_screen(&h);
    assert!(
        rows > 0,
        "nothing rendered at all — the list is not drawing"
    );
    assert!(
        rows < 200,
        "virtualisation is not working: {rows} of 10000 rows reached the tree"
    );
}

#[test]
fn a_small_backlog_renders_every_message() {
    // The counterpart to the test above: with few enough messages nothing is
    // culled, which proves the low count there is culling rather than a list
    // that simply fails to draw.
    let h = harness(msgs(3, "hello there"), WIDE);
    assert_eq!(rows_on_screen(&h), 3);
}

#[test]
fn message_text_actually_reaches_the_screen() {
    let h = harness(msgs(1, "unmistakable"), WIDE);
    assert_eq!(
        h.query_all_by_label("unmistakable").count(),
        1,
        "a word from the message body should be in the tree"
    );
}

#[test]
fn an_emote_does_not_push_the_following_word_onto_another_line() {
    // The single most important claim about this renderer: an emote sits INLINE
    // in the text. If it were laid out as its own block, the word after it
    // would land on the next line — a different y.
    let h = harness(msgs(1, "before KEKW after"), WIDE);
    let b = h.get_by_label("before").rect();
    let a = h.get_by_label("after").rect();
    assert!(
        (b.min.y - a.min.y).abs() < 4.0,
        "'before' and 'after' straddle an emote but sit on different lines \
         (y {:.1} vs {:.1}) — the emote is not inline",
        b.min.y,
        a.min.y
    );
    assert!(
        a.min.x > b.max.x,
        "'after' should sit to the right of 'before' on the same line"
    );
}

#[test]
fn a_long_message_wraps_onto_more_than_one_line() {
    let long = "alpha bravo charlie delta echo foxtrot golf hotel india juliet \
                kilo lima mike november oscar papa quebec romeo sierra tango";
    let h = harness(msgs(1, long), NARROW);
    let f = h.get_by_label("alpha").rect();
    let l = h.get_by_label("tango").rect();
    assert!(
        l.min.y > f.min.y,
        "a long message in a {}px window should wrap, but 'tango' shares a line \
         with 'alpha' (y {:.1} vs {:.1})",
        NARROW.x,
        f.min.y,
        l.min.y
    );
}

#[test]
fn a_painted_username_still_renders_as_one_readable_name() {
    // Paints colour a name per character via one LayoutJob section per char.
    // That must not shatter the name into one node per glyph.
    let s = set();
    let paint = Paint::animated(vec![egui::Color32::RED, egui::Color32::BLUE], 0.5);
    let messages = vec![Message::parse("mellen", Some(paint), "hi", &s, 3.0)];
    let h = harness(messages, WIDE);
    assert_eq!(
        h.query_all_by_label("mellen").count(),
        1,
        "a painted name should be one label, not one node per glyph"
    );
}

#[test]
fn narrowing_the_window_fits_fewer_messages() {
    let text = "alpha bravo charlie delta echo foxtrot golf hotel india juliet";
    let wide = rows_on_screen(&harness(msgs(400, text), Vec2::new(1200.0, 600.0)));
    let narrow = rows_on_screen(&harness(msgs(400, text), NARROW));
    assert!(
        wide > 0 && narrow > 0,
        "both widths should render something"
    );
    assert!(
        narrow < wide,
        "messages wrap taller in a narrow window, so fewer should fit: \
         narrow {narrow} vs wide {wide}"
    );
}

#[test]
fn an_empty_backlog_renders_without_panicking() {
    // The prefix-sum row search does a saturating_sub on an empty list; this is
    // the guard that it stays that way.
    let h = harness(Vec::new(), WIDE);
    assert_eq!(rows_on_screen(&h), 0);
}

/// The repaint rate must follow what is ON SCREEN, not what is loaded.
///
/// This is the whole reason the tick moved out of `Cache` and off the message
/// list: a channel's 7TV set is thousands of emotes, and asking the cache for
/// its shortest frame delay meant one fast emote pinned the window's repaint
/// rate forever — even scrolled ten thousand messages away from it. Same for a
/// name paint on a message long since off screen.
mod cadence_follows_the_screen {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    const ANIM_DELAY_MS: u32 = 40;

    /// The cache is keyed by a stack's URLs, not by the emote's name, so a
    /// hardcoded "KEKW" silently misses and `stack()` takes its not-loaded
    /// path. Ask the parser for the key the renderer will actually look up.
    fn kekw_key() -> String {
        let s = set();
        heatsync_core::emote::segments("KEKW", &s)
            .into_iter()
            .find_map(|seg| match seg {
                heatsync_core::emote::Segment::Stack(st) => Some(st.key()),
                _ => None,
            })
            .expect("KEKW should parse as an emote stack")
    }

    /// A cache holding one genuinely animated two-frame stack for `KEKW`.
    ///
    /// The first version of these tests used `Cache::default()`, which is
    /// empty — so `stack()` took its not-loaded fallback and the emote path
    /// under test never ran. Every assertion passed for the wrong reason, and a
    /// mutation pinning the clock unconditionally went undetected.
    fn animated_cache(ctx: &egui::Context) -> Cache {
        let mut cache = Cache::default();
        let px = vec![255u8; 4]; // 1x1 rgba
        cache.insert(
            ctx,
            &kekw_key(),
            false,
            vec![vec![
                (px.clone(), 1, 1, ANIM_DELAY_MS),
                (px, 1, 1, ANIM_DELAY_MS),
            ]],
        );
        cache
    }

    /// Drive the view to a settled state and report the cadence it ended up
    /// asking for.
    ///
    /// Read AFTER `run()`, never inside the frame closure. The list sticks to
    /// the bottom and measures row heights as it goes, so the first pass or two
    /// are drawn against estimates and show the wrong rows — asserting inside
    /// the closure asserts against that transient.
    fn settled_tick(messages: Vec<Message>) -> Option<u32> {
        let seen = Rc::new(Cell::new(None));
        let out = Rc::clone(&seen);
        let mut view = View::default();
        let mut h = Harness::builder().with_size(WIDE).build_ui(move |ui| {
            let cache = animated_cache(ui.ctx());
            view.show(ui, &messages, &cache, 0);
            out.set(view.tick_ms());
        });
        h.run();
        seen.get()
    }

    fn plain(n: usize) -> Vec<Message> {
        let s = set();
        (0..n)
            .map(|i| Message::parse(&format!("user{i:05}"), None, "plain text", &s, 1.0))
            .collect()
    }

    #[test]
    fn a_visible_animated_emote_asks_for_its_own_frame_delay() {
        let s = set();
        let messages = vec![Message::parse("mellen", None, "KEKW", &s, 1.0)];
        assert_eq!(
            settled_tick(messages),
            Some(ANIM_DELAY_MS),
            "a drawn animated emote should ask for its own frame delay"
        );
    }

    #[test]
    fn an_offscreen_animation_does_not_drive_repaints() {
        // The list sticks to the bottom, so the OLDEST message is the one
        // virtualisation leaves undrawn. The emote is loaded and animating —
        // just not visible — which is exactly the case that used to pin the
        // repaint clock through the whole-cache scan.
        let s = set();
        let mut messages = plain(500);
        messages[0] = Message::parse("user00000", None, "KEKW", &s, 1.0);
        assert_eq!(
            settled_tick(messages),
            None,
            "an emote outside the viewport is still driving the repaint clock"
        );
    }

    #[test]
    fn a_visible_animated_paint_asks_for_its_own_cadence() {
        let s = set();
        let paint = Paint::animated(vec![egui::Color32::RED, egui::Color32::BLUE], 0.5);
        let messages = vec![Message::parse("mellen", Some(paint), "hi", &s, 3.0)];
        assert_eq!(
            settled_tick(messages),
            Some(crate::paint::TICK_MS),
            "a painted name on screen should ask for the paint cadence"
        );
    }

    #[test]
    fn a_still_screen_asks_for_nothing() {
        // Animation loaded in the cache, none of it on screen.
        assert_eq!(
            settled_tick(plain(3)),
            None,
            "a still screen must not repaint at all"
        );
    }
}
