use crate::emote::fb::FbEmotes;
use crate::emote::render::{EmoteStore, EMOTE_H};
use crate::paints::UserLook;
use crate::palette;
use heatsync_core::emote::{segments_with, EmoteSet, Segment};
use heatsync_core::paint::Paint;
use heatsync_core::{Badge, Message, Platform};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::time::Duration;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// which emote backend a channel column should draw with this frame.
#[derive(Clone, Copy)]
pub(crate) enum EmoteMode<'a> {
    Term(&'a EmoteStore),
    Fb(&'a FbEmotes),
    Text,
}

impl EmoteMode<'_> {
    /// this emote's width in cells once it's loaded, else None. width is
    /// per-emote (derived from its real aspect and the terminal's cell aspect),
    /// so a square emote reads square and a wide one isn't crushed. `None`
    /// doubles as "not ready" — the layout falls back to the emote's name.
    pub(crate) fn cells(&self, url: &str) -> Option<u16> {
        match self {
            EmoteMode::Term(s) => s.cells(url),
            EmoteMode::Fb(f) => f.cells(url),
            EmoteMode::Text => None,
        }
    }

    /// the footprint a not-yet-loaded emote is laid out at. `None` in text mode,
    /// where emote names are just words on the line.
    pub(crate) fn square_cells(&self) -> Option<u16> {
        match self {
            EmoteMode::Term(s) => Some(s.square_cells()),
            EmoteMode::Fb(f) => Some(f.square_cells()),
            EmoteMode::Text => None,
        }
    }

    /// how long until any loaded emote flips to its next frame.
    pub(crate) fn next_flip_in(&self) -> Option<Duration> {
        match self {
            EmoteMode::Term(s) => s.next_flip_in(),
            EmoteMode::Fb(f) => f.next_flip_in(),
            EmoteMode::Text => None,
        }
    }
}

/// walk a message's emote STACKS in order, yielding each stack's cache key.
/// core's [`segments`] owns the grammar — zero-width overlays, `w!`-style BTTV
/// prefixes, `z!` forcing, FFZ effect words. shared by layout and prefetch.
pub(crate) fn each_stack(text: &str, set: &EmoteSet, refs: &EmoteSet, mut f: impl FnMut(&str)) {
    for seg in segments_with(text, set, refs) {
        if let Segment::Stack(s) = seg {
            f(&s.key());
        }
    }
}

/// truncate a label to `w` display columns (approximate; ASCII-dominant labels).
pub(crate) fn fit(s: String, w: u16) -> String {
    let w = w as usize;
    if UnicodeWidthStr::width(s.as_str()) <= w {
        s
    } else {
        s.chars().take(w.saturating_sub(1)).collect()
    }
}

/// clip or pad `s` to EXACTLY `w` display columns. the emote placeholder has to
/// occupy the same footprint the image will, or the line shifts when it loads.
fn fit_exact(s: &str, w: u16) -> String {
    let w = w as usize;
    let mut out = String::with_capacity(w);
    let mut used = 0usize;
    for c in s.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + cw > w {
            break;
        }
        out.push(c);
        used += cw;
    }
    out.extend(std::iter::repeat_n(' ', w - used));
    out
}

/// a reserved emote slot: column offset within the channel body + its stack key.
pub(crate) struct Place {
    pub(crate) col: u16,
    /// this emote's own width in cells — square emotes and wide emotes get
    /// different footprints instead of one fixed block.
    pub(crate) w: u16,
    pub(crate) key: String,
}

/// does `text` address `me` — as a word, with or without a leading `@` and
/// trailing punctuation ("@Mellen," pings mellen).
pub(crate) fn mentions(text: &str, me: &str) -> bool {
    text.split_whitespace().any(|w| {
        w.trim_start_matches('@')
            .trim_end_matches(|c: char| !c.is_alphanumeric() && c != '_')
            .eq_ignore_ascii_case(me)
    })
}

/// does `text` address any of my names (twitch + kick logins).
pub(crate) fn mentions_any(text: &str, me: &[String]) -> bool {
    me.iter().any(|n| mentions(text, n))
}

/// is `reply_to` (the display name a line answers) one of my names?
pub(crate) fn replies_to_me(reply_to: Option<&str>, me: &[String]) -> bool {
    reply_to.is_some_and(|r| me.iter().any(|n| n.eq_ignore_ascii_case(r)))
}

/// how a message is laid out beyond its own fields.
#[derive(Default)]
pub(crate) struct Look<'a> {
    /// my names — a line that mentions or answers one gets the ping bar.
    pub(crate) me: &'a [String],
    /// merged tab: a bar in the platform's hue marks each line's origin.
    pub(crate) tag_platform: bool,
    /// dim `HH:MM` before each line.
    pub(crate) stamps: bool,
    /// the sender's heatsync name paint / picked color, once resolved.
    pub(crate) user: Option<&'a UserLook>,
}

/// one-cell role badge: black glyph on the role's color. same black-on-color
/// scheme as the active tab — square, dense, no brackets.
fn badge_span(b: Badge) -> Span<'static> {
    Span::styled(b.glyph().to_string(), palette::badge(b))
}

/// continuation rows hang under the message body by this many columns.
const WRAP_INDENT: u16 = 2;
/// visual rows one message may occupy — bounds an emote/text wall so a single
/// line can never eat the whole pane. twitch/kick cap text at 500 chars, which
/// wraps well inside this on any sane width.
const MAX_MSG_ROWS: usize = 8;

/// one visual row of a laid-out message: its text line, its reserved emote
/// slots, and its height (EMOTE_H when it holds an image, else 1).
pub(crate) struct RowPlan {
    pub(crate) line: Line<'static>,
    pub(crate) places: Vec<Place>,
    pub(crate) h: u16,
}

/// the wrapping layouter: words and emote stacks flow left to right and wrap
/// onto indented continuation rows. shared by chat rows and the composer
/// preview.
pub(crate) struct Rows {
    pub(crate) maxw: u16,
    indent: u16, // hanging indent of continuation rows
    pub(crate) rows: Vec<RowPlan>,
    pub(crate) spans: Vec<Span<'static>>,
    pub(crate) places: Vec<Place>,
    pub(crate) col: u16,
    pub(crate) row_start: u16, // col where content on this row begins (prefix or indent)
    pub(crate) has_stack: bool,
    pub(crate) full: bool, // hit MAX_MSG_ROWS — stop consuming
}

impl Rows {
    pub(crate) fn new(maxw: u16) -> Rows {
        Rows {
            maxw: maxw.max(WRAP_INDENT + 1),
            indent: WRAP_INDENT,
            rows: Vec::new(),
            spans: Vec::new(),
            places: Vec::new(),
            col: 0,
            row_start: 0,
            has_stack: false,
            full: false,
        }
    }

    /// append a prefix span that never wraps (badges, username).
    pub(crate) fn prefix(&mut self, span: Span<'static>, w: u16) {
        self.spans.push(span);
        self.col += w;
        self.row_start = self.col.min(self.maxw);
    }

    fn at_row_start(&self) -> bool {
        self.col == self.row_start
    }

    /// close the current visual row and open an indented continuation.
    fn newline(&mut self) {
        let h = if self.has_stack { EMOTE_H } else { 1 };
        self.rows.push(RowPlan {
            line: Line::from(std::mem::take(&mut self.spans)),
            places: std::mem::take(&mut self.places),
            h,
        });
        self.has_stack = false;
        self.col = self.indent;
        self.row_start = self.indent;
        self.spans.push(Span::raw(" ".repeat(self.indent as usize)));
        if self.rows.len() >= MAX_MSG_ROWS {
            self.full = true;
        }
    }

    /// one word of styled text, wrapping first when it doesn't fit and
    /// hard-splitting anything wider than a whole row (urls).
    fn word(&mut self, w: &str, style: Style) {
        if self.full {
            return;
        }
        let ww = UnicodeWidthStr::width(w) as u16;
        let lead = u16::from(!self.at_row_start());
        if self.col + lead + ww > self.maxw && !self.at_row_start() {
            self.newline();
            if self.full {
                return;
            }
        }
        if self.col + ww <= self.maxw {
            let s = if self.at_row_start() {
                w.to_string()
            } else {
                format!(" {w}")
            };
            self.col += UnicodeWidthStr::width(s.as_str()) as u16;
            self.spans.push(Span::styled(s, style));
            return;
        }
        // wider than a full row: hard-split into row-width chunks.
        let mut rest = w;
        while !rest.is_empty() && !self.full {
            let avail = self.maxw - self.col;
            let chunk: String = {
                let mut used = 0u16;
                rest.chars()
                    .take_while(|c| {
                        used += UnicodeWidthChar::width(*c).unwrap_or(0) as u16;
                        used <= avail
                    })
                    .collect()
            };
            if chunk.is_empty() {
                break; // a glyph wider than the remaining row — drop it
            }
            rest = &rest[chunk.len()..];
            self.col += UnicodeWidthStr::width(chunk.as_str()) as u16;
            self.spans.push(Span::styled(chunk, style));
            if !rest.is_empty() {
                self.newline();
            }
        }
    }

    /// one emote stack: reserve its cells (image or placeholder), wrapping
    /// when it doesn't fit; one too wide for any row falls back to its name.
    fn stack(&mut self, s: heatsync_core::emote::Stack, mode: EmoteMode) {
        if self.full {
            return;
        }
        let key = s.key();
        // a loaded emote knows its width; a loading one is laid out at the
        // provisional square footprint (doubled for w!/ffzW) so nothing
        // reflows when the image lands.
        let sized = match mode.cells(&key) {
            Some(w) => Some((w, true)),
            None => mode
                .square_cells()
                .map(|w| (if s.wide() { (w * 2).min(8) } else { w }, false)),
        };
        let Some((w, ready)) = sized else {
            // text tier — the name is just a word on the line.
            return self.word(&s.base, palette::TEXT);
        };
        if self.col + w > self.maxw && !self.at_row_start() {
            self.newline();
            if self.full {
                return;
            }
        }
        if self.col + w > self.maxw {
            return self.word(&s.base, palette::TEXT);
        }
        self.has_stack = true;
        if ready {
            self.places.push(Place {
                col: self.col,
                w,
                key,
            });
            self.spans.push(Span::raw(" ".repeat(w as usize)));
        } else {
            // loading: hold the exact footprint and show what fits of the
            // name, so the image swaps in place instead of shoving the line.
            self.spans
                .push(Span::styled(fit_exact(&s.base, w), palette::TEXT));
        }
        self.col += w;
    }

    /// close out the layout.
    pub(crate) fn finish(mut self) -> Vec<RowPlan> {
        if self.full {
            // truncated: drop the empty continuation, mark the last real row.
            if let Some(last) = self.rows.last_mut() {
                last.line.spans.push(Span::styled("…", palette::DIM));
            }
        } else if !self.spans.is_empty() || self.rows.is_empty() {
            let h = if self.has_stack { EMOTE_H } else { 1 };
            self.rows.push(RowPlan {
                line: Line::from(std::mem::take(&mut self.spans)),
                places: std::mem::take(&mut self.places),
                h,
            });
        }
        self.rows
    }
}

/// flow `text`'s segments (words + emote stacks) into the layouter.
pub(crate) fn layout_text(
    b: &mut Rows,
    text: &str,
    set: &EmoteSet,
    refs: &EmoteSet,
    mode: EmoteMode,
) {
    // message text is plain white — heat lives in the bar and tab numbers,
    // never in the reading surface.
    layout_text_in(b, text, set, refs, mode, palette::TEXT);
}

/// `layout_text` in a given style (`/me` and highlighted lines).
fn layout_text_in(
    b: &mut Rows,
    text: &str,
    set: &EmoteSet,
    refs: &EmoteSet,
    mode: EmoteMode,
    text_hue: Style,
) {
    for seg in segments_with(text, set, refs) {
        if b.full {
            break;
        }
        match seg {
            Segment::Text(t) => {
                for w in t.split(' ') {
                    b.word(w, text_hue);
                }
            }
            Segment::Stack(s) => b.stack(s, mode),
        }
    }
}

/// the bold username prefix. a paint colors it per character where the
/// terminal can show 24-bit color; anywhere else the plain name color stands.
fn name(b: &mut Rows, user: &str, color: Color, paint: Option<&Paint>) {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let Some(p) = paint.filter(|_| truecolor()) else {
        let uw = UnicodeWidthStr::width(user) as u16;
        b.prefix(Span::styled(user.to_string(), bold.fg(color)), uw);
        return;
    };
    let chars: Vec<char> = user.chars().collect();
    let mut run = String::new();
    let mut run_w = 0u16;
    let mut run_rgb = [0u8; 3];
    for (c, rgb) in chars.iter().zip(p.colors(chars.len())) {
        if !run.is_empty() && rgb != run_rgb {
            let fg = Color::Rgb(run_rgb[0], run_rgb[1], run_rgb[2]);
            b.prefix(Span::styled(std::mem::take(&mut run), bold.fg(fg)), run_w);
            run_w = 0;
        }
        run_rgb = rgb;
        run.push(*c);
        run_w += UnicodeWidthChar::width(*c).unwrap_or(0) as u16;
    }
    if !run.is_empty() {
        let fg = Color::Rgb(run_rgb[0], run_rgb[1], run_rgb[2]);
        b.prefix(Span::styled(run, bold.fg(fg)), run_w);
    }
}

/// lay a message out as wrapped visual rows: badges + user prefix on the
/// first, indented continuations after, emote cells reserved wherever their
/// stack lands.
pub(crate) fn layout_message(
    m: &Message,
    set: &EmoteSet,
    mode: EmoteMode,
    maxw: u16,
    look: &Look,
) -> Vec<RowPlan> {
    // the chat color wins; a heatsync-picked one fills in for a user with none.
    let user_color = m
        .color
        .as_deref()
        .or(look.user.and_then(|l| l.color.as_deref()))
        .and_then(parse_hex)
        .unwrap_or(Color::White);
    let paint = look.user.and_then(|l| l.paint.as_ref());
    let mut b = Rows::new(maxw);
    // dim `HH:MM` first when timestamps are on.
    let mut hang = 0;
    if look.stamps && m.extra.ts > 0 {
        let (h, mi) = crate::clock::hhmm(m.extra.ts);
        b.prefix(Span::styled(format!("{h:02}:{mi:02} "), palette::STAMP), 6);
        hang = 6;
    }
    // merged tab: a `[T]`/`[K]`/`[Y]` tag in the platform's hue marks each
    // line's origin; wrapped rows hang under the text after it.
    if look.tag_platform {
        let tag = match m.platform {
            Platform::Twitch => "[T] ",
            Platform::Kick => "[K] ",
            Platform::Youtube => "[Y] ",
        };
        b.prefix(Span::styled(tag, palette::platform(m.platform)), 4);
        hang += 4;
    }
    if hang > 0 {
        b.indent = hang.min(b.maxw - 1);
    }
    // an event line: glyph + actor + headline in the event's hue, then any
    // attached chat text (resub message, kicks message) laid out like chat.
    if let Some(n) = &m.note {
        let (glyph, hue) = palette::note(n.kind);
        b.prefix(Span::styled(glyph, hue), 1);
        b.prefix(Span::raw(" "), 1);
        if !m.user.is_empty() {
            name(&mut b, &m.user, user_color, paint);
            b.prefix(Span::raw(" "), 1);
        }
        for w in n.what.split(' ') {
            b.word(w, hue);
        }
        if !m.text.is_empty() {
            b.word("·", palette::DIM);
            layout_text(&mut b, &m.text, set, m.refs(), mode);
        }
        return b.finish();
    }
    // role badges, capped — a badge wall must not eat the line.
    for &bd in m.badges.iter().take(3) {
        b.prefix(badge_span(bd), 1);
    }
    if !m.badges.is_empty() {
        b.prefix(Span::raw(" "), 1);
    }
    name(&mut b, &m.user, user_color, paint);
    // a line that pings you swaps its ": " for a blue bar — semantic, not decor.
    let pinged = mentions_any(&m.text, look.me) || replies_to_me(m.reply_to.as_deref(), look.me);
    b.prefix(
        if pinged {
            Span::styled("▌ ", palette::PING)
        } else {
            Span::styled(": ", palette::DIM)
        },
        2,
    );
    // /me: the user's color, italic. a highlighted line is reverse-video.
    let mut hue = palette::TEXT;
    if m.extra.action {
        hue = Style::default()
            .fg(user_color)
            .add_modifier(Modifier::ITALIC);
    }
    if m.extra.highlight {
        hue = hue.add_modifier(Modifier::REVERSED);
    }
    layout_text_in(&mut b, &m.text, set, m.refs(), mode, hue);
    if m.extra.first {
        b.word("◇ first message", palette::DIM_TAG);
    }
    let mut rows = match &m.gone {
        Some(g) => {
            b.word(&format!("[{g}]"), palette::GONE_TAG);
            let mut rows = b.finish();
            strike(&mut rows);
            rows
        }
        None => b.finish(),
    };
    // a reply shows what it answers on a dim row above, cut to the pane.
    if let Some(r) = &m.reply_to {
        let ctx = if m.extra.reply_text.is_empty() {
            format!("↪@{r}")
        } else {
            format!("↪@{r}: {}", m.extra.reply_text)
        };
        rows.insert(
            0,
            RowPlan {
                line: Line::from(Span::styled(fit(ctx, maxw), palette::DIM_TAG)),
                places: Vec::new(),
                h: 1,
            },
        );
    }
    rows
}

/// a removed line: gray + crossed out, everything but its own `[tag]`.
fn strike(rows: &mut [RowPlan]) {
    for r in rows {
        for sp in &mut r.line.spans {
            if sp.style != palette::GONE_TAG {
                sp.style = palette::GONE;
            }
        }
    }
}

/// does this terminal take 24-bit color? decided once from `COLORTERM`.
fn truecolor() -> bool {
    static TRUECOLOR: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *TRUECOLOR.get_or_init(|| {
        std::env::var("COLORTERM")
            .map(|v| v.contains("truecolor") || v.contains("24bit"))
            .unwrap_or(false)
    })
}

/// `#rrggbb` → terminal color. truecolor terminals get the exact rgb; anything
/// else (linux vt, old xterms, DEC hardware) gets the nearest xterm-256 index —
/// emitting 24-bit escapes at a terminal that can't parse them turns user
/// colors into garbage instead of an approximation.
fn parse_hex(hex: &str) -> Option<Color> {
    let h = hex.strip_prefix('#')?;
    if h.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&h[0..2], 16).ok()?;
    let g = u8::from_str_radix(&h[2..4], 16).ok()?;
    let b = u8::from_str_radix(&h[4..6], 16).ok()?;
    Some(if truecolor() {
        Color::Rgb(r, g, b)
    } else {
        Color::Indexed(nearest_256(r, g, b))
    })
}

/// nearest xterm-256 index for an rgb triple: best of the 6×6×6 color cube
/// (16–231) and the gray ramp (232–255).
fn nearest_256(r: u8, g: u8, b: u8) -> u8 {
    // cube levels are 0,95,135,175,215,255
    let q = |v: u8| -> u8 {
        if v < 48 {
            0
        } else if v < 115 {
            1
        } else {
            ((v as u16 - 35) / 40) as u8
        }
    };
    let lv = |i: u8| -> u8 {
        if i == 0 {
            0
        } else {
            55 + 40 * i
        }
    };
    let (qr, qg, qb) = (q(r), q(g), q(b));
    let cube = (16 + 36 * qr as u16 + 6 * qg as u16 + qb as u16) as u8;
    let (cr, cg, cb) = (lv(qr), lv(qg), lv(qb));
    // gray ramp levels are 8,18,…,238
    let avg = ((r as u16 + g as u16 + b as u16) / 3) as i16;
    let gi = (((avg - 3) / 10).clamp(0, 23)) as u8;
    let gv = 8 + 10 * gi;
    let d2 = |x: (u8, u8, u8), y: (u8, u8, u8)| -> i32 {
        let d = |a: u8, b: u8| (a as i32 - b as i32).pow(2);
        d(x.0, y.0) + d(x.1, y.1) + d(x.2, y.2)
    };
    if d2((r, g, b), (gv, gv, gv)) < d2((r, g, b), (cr, cg, cb)) {
        232 + gi
    } else {
        cube
    }
}

#[cfg(test)]
mod placeholder_tests {
    use super::fit_exact;
    use unicode_width::UnicodeWidthStr;

    /// the loading placeholder MUST occupy exactly the cells the image will, or
    /// the line shifts under the reader the moment the emote arrives.
    #[test]
    fn placeholder_matches_the_reserved_footprint_exactly() {
        for (name, w) in [("pokiDance", 4u16), ("Cat", 4), ("x", 2), ("", 3)] {
            let out = fit_exact(name, w);
            assert_eq!(
                UnicodeWidthStr::width(out.as_str()),
                w as usize,
                "{name:?} at width {w} → {out:?}"
            );
        }
    }

    #[test]
    fn placeholder_never_splits_a_wide_glyph() {
        // a 2-column glyph must not be half-emitted into a 1-column slot.
        let out = fit_exact("\u{1f525}ok", 1);
        assert_eq!(UnicodeWidthStr::width(out.as_str()), 1);
        assert_eq!(out, " "); // dropped the wide char, padded instead
    }

    #[test]
    fn placeholder_keeps_what_fits() {
        assert_eq!(fit_exact("pokiDance", 4), "poki");
        assert_eq!(fit_exact("Cat", 5), "Cat  ");
    }
}

#[cfg(test)]
mod wrap_tests {
    use super::*;

    fn msg(text: &str) -> Message {
        Message {
            platform: Platform::Twitch,
            user: "u".into(),
            text: text.into(),
            color: None,
            badges: Vec::new(),
            id: None,
            gone: None,
            reply_to: None,
            note: None,
            extra: Default::default(),
            emotes: None,
            heat: 0.0,
            uid: None,
        }
    }

    fn flat(rows: &[RowPlan]) -> Vec<String> {
        rows.iter()
            .map(|r| {
                r.line
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn a_removed_line_is_struck_gray_with_its_own_tag() {
        let set = EmoteSet::new();
        let mut m = msg("buy followers");
        m.gone = Some("timed out 10m".into());
        let rows = layout_message(&m, &set, EmoteMode::Text, 80, &Look::default());
        let spans: Vec<&Span> = rows.iter().flat_map(|r| r.line.spans.iter()).collect();
        let tag = spans.last().unwrap();
        assert_eq!(tag.content.trim(), "[timed out 10m]");
        assert_eq!(tag.style, palette::GONE_TAG);
        assert!(spans.len() > 1);
        for sp in &spans[..spans.len() - 1] {
            assert_eq!(sp.style, palette::GONE, "{:?}", sp.content);
        }
        // an untouched line keeps its colors
        let ok = layout_message(&msg("hi"), &set, EmoteMode::Text, 80, &Look::default());
        assert!(ok[0].line.spans.iter().all(|s| s.style != palette::GONE));
    }

    #[test]
    fn a_reply_gets_a_dim_context_row_cut_to_the_pane() {
        let set = EmoteSet::default();
        let mut m = msg("yes");
        m.reply_to = Some("tar".into());
        m.extra.reply_text = "a very long original message that cannot fit".into();
        let rows = layout_message(&m, &set, EmoteMode::Text, 20, &Look::default());
        let t = flat(&rows);
        assert_eq!(t.len(), 2);
        assert!(t[0].starts_with("↪@tar: a very"));
        assert!(UnicodeWidthStr::width(t[0].as_str()) <= 20);
        assert_eq!(t[1], "u: yes");
        m.extra.reply_text.clear();
        assert_eq!(
            flat(&layout_message(
                &m,
                &set,
                EmoteMode::Text,
                20,
                &Look::default()
            ))[0],
            "↪@tar"
        );
    }

    #[test]
    fn first_message_tag_action_italic_highlight_reverse_and_ping_on_reply() {
        let set = EmoteSet::default();
        let mut m = msg("hello");
        m.extra.first = true;
        m.extra.action = true;
        m.extra.highlight = true;
        let rows = layout_message(&m, &set, EmoteMode::Text, 60, &Look::default());
        assert_eq!(flat(&rows), ["u: hello ◇ first message"]);
        let body = &rows[0].line.spans[2];
        assert!(body.style.add_modifier.contains(Modifier::ITALIC));
        assert!(body.style.add_modifier.contains(Modifier::REVERSED));
        // a reply to one of my names pings even without an @.
        let me = vec!["Tar".to_string()];
        let mut r = msg("no");
        r.reply_to = Some("tar".into());
        let look = Look {
            me: &me,
            ..Look::default()
        };
        let rows = layout_message(&r, &set, EmoteMode::Text, 60, &look);
        assert!(flat(&rows)[1].starts_with("u▌ "));
    }

    #[test]
    fn stamps_prefix_only_when_on_and_known() {
        let set = EmoteSet::default();
        let mut m = msg("hi");
        m.extra.ts = 1_700_000_000;
        let on = Look {
            stamps: true,
            ..Look::default()
        };
        let t = flat(&layout_message(&m, &set, EmoteMode::Text, 40, &on));
        assert!(
            t[0].len() > "u: hi".len() && t[0].ends_with(" u: hi"),
            "{t:?}"
        );
        m.extra.ts = 0;
        assert_eq!(
            flat(&layout_message(&m, &set, EmoteMode::Text, 40, &on)),
            ["u: hi"]
        );
        m.extra.ts = 5;
        assert_eq!(
            flat(&layout_message(
                &m,
                &set,
                EmoteMode::Text,
                40,
                &Look::default()
            )),
            ["u: hi"]
        );
    }

    #[test]
    fn long_messages_wrap_onto_indented_rows() {
        let set = EmoteSet::new();
        let rows = layout_message(
            &msg("one two three four five"),
            &set,
            EmoteMode::Text,
            12,
            &Look::default(),
        );
        let got = flat(&rows);
        assert!(got.len() > 1, "wrapped: {got:?}");
        assert_eq!(got[0], "u: one two");
        assert!(
            got[1].starts_with("  three"),
            "indented continuation: {got:?}"
        );
        // nothing exceeds the width
        for r in &got {
            assert!(UnicodeWidthStr::width(r.as_str()) <= 12, "{r:?}");
        }
    }

    #[test]
    fn a_word_wider_than_the_row_hard_splits() {
        let set = EmoteSet::new();
        let rows = layout_message(
            &msg("https://example.com/really/long/url"),
            &set,
            EmoteMode::Text,
            14,
            &Look::default(),
        );
        let got = flat(&rows);
        assert!(got.len() >= 3, "{got:?}");
        for r in &got {
            assert!(UnicodeWidthStr::width(r.as_str()) <= 14, "{r:?}");
        }
        assert_eq!(
            got.join("").replace(' ', ""),
            "u:https://example.com/really/long/url".replace(' ', "")
        );
    }

    #[test]
    fn a_wall_is_capped_with_an_ellipsis() {
        let set = EmoteSet::new();
        let text = "word ".repeat(200);
        let rows = layout_message(&msg(&text), &set, EmoteMode::Text, 20, &Look::default());
        assert_eq!(rows.len(), MAX_MSG_ROWS);
        assert!(flat(&rows).last().unwrap().ends_with('…'));
    }

    #[test]
    fn merged_tabs_tag_the_platform_and_hang_under_it() {
        let set = EmoteSet::new();
        let mut m = msg(&"word ".repeat(30));
        m.platform = Platform::Kick;
        let rows = layout_message(
            &m,
            &set,
            EmoteMode::Text,
            20,
            &Look {
                tag_platform: true,
                ..Look::default()
            },
        );
        let f = flat(&rows);
        assert!(f[0].starts_with("[K] u"));
        assert!(f[1].starts_with("    ") && !f[1].starts_with("     "));
        let one = layout_message(&m, &set, EmoteMode::Text, 20, &Look::default());
        assert!(flat(&one)[0].starts_with("u"));
    }

    fn first_span(m: &Message, user: Option<&UserLook>) -> Vec<Span<'static>> {
        let set = EmoteSet::new();
        let look = Look {
            user,
            ..Look::default()
        };
        let rows = layout_message(m, &set, EmoteMode::Text, 80, &look);
        rows[0].line.spans.clone()
    }

    #[test]
    fn a_profile_color_fills_in_only_when_chat_sent_none() {
        let look = UserLook {
            paint: None,
            color: Some("#ff0000".into()),
        };
        let mut m = msg("hi");
        assert_ne!(first_span(&m, Some(&look))[0].style.fg, Some(Color::White));
        m.color = Some("#00ff00".into());
        assert_eq!(
            first_span(&m, Some(&look))[0].style.fg,
            first_span(&m, None)[0].style.fg
        );
    }

    #[test]
    fn a_paint_colors_the_name_per_character_on_truecolor_only() {
        let paint = Paint::from_heatsync(&serde_json::json!({"base": {"type": "linear",
            "angle": 90, "stops": [{"color": "#000000", "pos": 0}, {"color": "#ffffff", "pos": 100}]}}));
        let look = UserLook { paint, color: None };
        let mut m = msg("hi");
        m.user = "abcdef".into();
        let spans = first_span(&m, Some(&look));
        let name: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(name.starts_with("abcdef"), "{name}");
        if truecolor() {
            assert!(spans.len() > 3, "gradient splits the name into runs");
        } else {
            assert_eq!(spans[0].content, "abcdef");
        }
    }

    #[test]
    fn short_messages_stay_on_one_row() {
        let set = EmoteSet::new();
        let rows = layout_message(&msg("hi"), &set, EmoteMode::Text, 40, &Look::default());
        assert_eq!(flat(&rows), vec!["u: hi"]);
        assert_eq!(rows[0].h, 1);
    }
}

#[cfg(test)]
mod stack_tests {
    use super::*;
    use heatsync_core::emote::Emote;

    fn em(name: &str, zw: bool) -> Emote {
        Emote {
            name: name.into(),
            url: format!("u/{name}"),
            provider: "7tv".into(),
            id: name.into(),
            animated: false,
            zero_width: zw,
            global: false,
        }
    }

    #[test]
    fn overlay_groups_onto_base() {
        let set = EmoteSet::from_list([em("GAMBA", false), em("notL", true)]);
        let mut keys = Vec::new();
        each_stack("GAMBA notL", &set, EmoteSet::none(), |k| {
            keys.push(k.to_string())
        });
        assert_eq!(keys, vec!["u/GAMBA\nu/notL"]); // one stack, base + overlay
    }

    #[test]
    fn text_breaks_stack() {
        let set = EmoteSet::from_list([em("GAMBA", false), em("KEKW", false)]);
        let mut keys = Vec::new();
        each_stack("GAMBA lol KEKW", &set, EmoteSet::none(), |k| {
            keys.push(k.to_string())
        });
        assert_eq!(keys, vec!["u/GAMBA", "u/KEKW"]); // separate, text between
    }

    #[test]
    fn multiple_overlays_stack() {
        let set = EmoteSet::from_list([em("A", false), em("h1", true), em("h2", true)]);
        let mut keys = Vec::new();
        each_stack("A h1 h2", &set, EmoteSet::none(), |k| {
            keys.push(k.to_string())
        });
        assert_eq!(keys, vec!["u/A\nu/h1\nu/h2"]);
    }
}
