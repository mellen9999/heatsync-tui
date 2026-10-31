use crate::app::{App, Feed, InputMode};
use crate::config::TabPos;
use crate::emote::render::EMOTE_H;
use crate::input::slot_of;
use crate::layout::{fit, layout_message, layout_text, EmoteMode, Look, Place, RowPlan, Rows};
use crate::palette;
use heatsync_core::edit;
use heatsync_core::emote::{segments, EmoteSet, Segment};
use heatsync_core::heat::Tier;
use heatsync_core::scroll;
use heatsync_core::TabKind;
use heatsync_core::{Channel, Platform};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use ratatui_image::Image;

/// width of the tab column when the bar is vertical (left/right).
const TAB_COL_W: u16 = 16;

pub(crate) fn emote_mode(app: &App) -> EmoteMode<'_> {
    match (&app.store, &app.fb) {
        // a tmux session outlives any one client: reattached over mosh or from
        // termux, the graphics escapes probed at startup would just vanish and
        // every emote would be a blank hole. usable() re-checks the attached
        // client on a TTL, so those frames draw emote names instead — and the
        // images come straight back when a graphics terminal reattaches.
        (Some(s), _) if s.usable() => EmoteMode::Term(s),
        (None, Some(f)) => EmoteMode::Fb(f),
        _ => EmoteMode::Text,
    }
}

pub(crate) fn ui(f: &mut Frame, app: &App) {
    app.chat_body.set(Rect::default());
    let mode = emote_mode(app);
    // wysiwyg strip: while composing, a draft that resolves an emote gets a
    // live preview row above the footer — the message as it will actually
    // render, images included. zero rows (and zero cost) otherwise.
    let preview_h = preview_height(app, mode);
    let [main, preview, footer] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(preview_h),
        Constraint::Length(1),
    ])
    .areas(f.area());
    if preview_h > 0 {
        draw_preview(f, preview, app, mode);
    }

    if let (InputMode::Pick, Some(p)) = (app.mode, &app.picker) {
        f.render_widget(Paragraph::new(p.lines(main.width, main.height)), main);
        draw_footer(f, footer, app, app.channels.len());
        return;
    }

    if matches!(
        app.mode,
        InputMode::Manage | InputMode::SlotPick | InputMode::SlotEdit(_)
    ) {
        draw_manage(f, main, app);
        draw_footer(f, footer, app, app.channels.len());
        return;
    }

    if app.channels.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("  no channels — press ", palette::DIM),
                Span::styled("o", palette::HEAD),
                Span::styled(" to join one", palette::DIM),
            ])),
            main,
        );
        draw_footer(f, footer, app, 0);
        return;
    }

    // carve the tab bar out of `main` according to the configured position.
    let (tabs, chat) = match app.tab_pos {
        TabPos::Top => {
            let a = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(main);
            (a[0], a[1])
        }
        TabPos::Bottom => {
            let a = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(main);
            (a[1], a[0])
        }
        TabPos::Left => {
            let a =
                Layout::horizontal([Constraint::Length(TAB_COL_W), Constraint::Min(0)]).split(main);
            (a[0], a[1])
        }
        TabPos::Right => {
            let a =
                Layout::horizontal([Constraint::Min(0), Constraint::Length(TAB_COL_W)]).split(main);
            (a[1], a[0])
        }
    };

    draw_tabs(f, tabs, app);
    draw_active(f, chat, app, mode);
    draw_footer(f, footer, app, app.channels.len());
}

/// rows the composer preview needs: EMOTE_H when the draft resolves at least
/// one emote on a graphics tier, else 0 (plain text needs no preview — the
/// composer already IS the text).
fn preview_height(app: &App, mode: EmoteMode) -> u16 {
    if app.mode != InputMode::Insert || app.channels.is_empty() || mode.square_cells().is_none() {
        return 0;
    }
    let text = app.line.text();
    if text.is_empty() {
        return 0;
    }
    let set = &app.emotes[app.focus];
    if segments(&text, set)
        .iter()
        .any(|s| matches!(s, Segment::Stack(_)))
    {
        EMOTE_H
    } else {
        0
    }
}

/// the draft as it will render — same layout path as a real chat row (first
/// visual row; the preview strip is one emote-row tall).
fn draw_preview(f: &mut Frame, area: Rect, app: &App, mode: EmoteMode) {
    let set = &app.emotes[app.focus];
    let mut b = Rows::new(area.width);
    b.prefix(Span::styled(" ❯ ", palette::TEXT), 3);
    let text = app.line.text();
    layout_text(&mut b, &text, set, EmoteSet::none(), mode);
    if let Some(row) = b.finish().into_iter().next() {
        draw_planned(f, mode, area, area.y, row.line, &row.places, EMOTE_H);
    }
}

/// the channel tab bar — horizontal row (top/bottom) or vertical list (left/right).
fn draw_tabs(f: &mut Frame, area: Rect, app: &App) {
    let tab_style = |i: usize, ch: &Channel| {
        if i == app.focus {
            palette::TAG
        } else {
            palette::tab(ch.unread())
        }
    };

    // merged tabs read `name+2` — the sources are one channel to the eye.
    let plus = |ch: &Channel| {
        if ch.merged() {
            format!("+{}", ch.extra.len())
        } else {
            String::new()
        }
    };
    if app.tab_pos.is_vertical() {
        for (i, ch) in app.channels.iter().enumerate() {
            if i as u16 >= area.height {
                break;
            }
            let label = fit(
                if ch.kind.is_special() {
                    format!(" {} ", ch.name)
                } else {
                    format!(" {}{} {:.0} ", ch.name, plus(ch), ch.heat)
                },
                area.width,
            );
            let row = Rect {
                x: area.x,
                y: area.y + i as u16,
                width: area.width,
                height: 1,
            };
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(label, tab_style(i, ch)))),
                row,
            );
        }
    } else {
        let mut spans = Vec::new();
        for (i, ch) in app.channels.iter().enumerate() {
            let label = if ch.kind.is_special() {
                format!(" {} ", ch.name)
            } else {
                format!(
                    " {}·{}{} {:.0} ",
                    ch.name,
                    ch.platform.tag(),
                    plus(ch),
                    ch.heat
                )
            };
            spans.push(Span::styled(label, tab_style(i, ch)));
            spans.push(Span::raw(" "));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}

/// the active channel: a heat bar, then its chat. messages with a ready emote
/// occupy EMOTE_H rows (text on the bottom row, emote painted across the block),
/// so emotes render big without colliding with neighbouring lines. bottom-anchored.
fn draw_active(f: &mut Frame, area: Rect, app: &App, mode: EmoteMode) {
    let ch = &app.channels[app.focus];
    let set = &app.emotes[app.focus];
    let pin = ch.visible_pin();
    let [barrow, pinrow, body] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(u16::from(pin.is_some())),
        Constraint::Min(0),
    ])
    .areas(area);
    f.render_widget(
        Paragraph::new(heat_bar(ch.heat, barrow.width as usize)),
        barrow,
    );
    if let Some(p) = pin {
        let line = fit(format!("◉ {}: {}", p.user, p.text), pinrow.width);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(line, palette::DIM_TAG))),
            pinrow,
        );
    }
    if body.height == 0 || body.width == 0 {
        return;
    }

    let cap = body.height as usize;
    // plan visible messages newest-first, honouring per-message height (wrapped
    // rows + emote rows). chat always follows live — no scrollback, no message
    // cursor. layout stops the moment the pane is full: nothing outside the
    // viewport is ever laid out or drawn.
    let mut plan: Vec<Vec<RowPlan>> = Vec::new();
    let mut used = 0usize;
    // live: the newest line sits at the bottom. with the message cursor up, the
    // cursor's line does — only the lines from there up are ever laid out.
    app.view_h.set(body.height);
    app.chat_body.set(body);
    let cursor_on = app.mode == InputMode::Cursor;
    let end = if cursor_on {
        scroll::index_of(ch.seq, ch.messages.len(), app.cursor).map_or(0, |i| i + 1)
    } else {
        ch.messages.len()
    };
    for (n, m) in ch.messages.range(..end).rev().enumerate() {
        let user = app
            .paints
            .look_for(m.uid.as_deref(), &m.user, m.color.is_some());
        let mut rows = layout_message(
            m,
            set,
            mode,
            body.width,
            &Look {
                me: &app.me,
                tag_platform: ch.merged(),
                stamps: app.stamps,
                user: user.as_deref(),
            },
        );
        if cursor_on && n == 0 {
            for r in rows.iter_mut() {
                for sp in r.line.spans.iter_mut() {
                    sp.style = sp.style.add_modifier(Modifier::REVERSED);
                }
            }
        }
        let h: usize = rows.iter().map(|r| r.h as usize).sum();
        if used + h > cap {
            // the newest message alone can exceed the pane — keep its tail.
            if used == 0 {
                let mut acc = 0usize;
                let keep_from = rows
                    .iter()
                    .rposition(|r| {
                        acc += r.h as usize;
                        acc > cap
                    })
                    .map(|i| i + 1)
                    .unwrap_or(0);
                rows.drain(..keep_from);
                used = rows.iter().map(|r| r.h as usize).sum();
                plan.push(rows);
            }
            break;
        }
        used += h;
        plan.push(rows);
    }
    plan.reverse();

    let mut y = body.y + (cap - used) as u16; // bottom-anchor
    for rows in plan {
        for row in rows {
            draw_planned(f, mode, body, y, row.line, &row.places, row.h);
            y += row.h;
        }
    }
}

/// draw one planned row: the text line on the bottom row of its block, then the
/// reserved emote cells (image inline, or NBSP + framebuffer queue). shared by
/// the chat body and the composer preview.
fn draw_planned(
    f: &mut Frame,
    mode: EmoteMode,
    area: Rect,
    y: u16,
    line: Line,
    places: &[Place],
    h: u16,
) {
    f.render_widget(
        Paragraph::new(line),
        Rect {
            x: area.x,
            y: y + h - 1,
            width: area.width,
            height: 1,
        },
    );
    for p in places {
        let x = area.x + p.col;
        match mode {
            EmoteMode::Term(store) => {
                if let Some(proto) = store.frame(&p.key) {
                    let rect = Rect {
                        x,
                        y,
                        width: p.w,
                        height: h,
                    };
                    if store.is_sixel(&p.key) {
                        // NBSP cells diff like text; the sixel itself is queued
                        // and written once, after the text (see emote/blit.rs).
                        fill_nbsp(f, rect);
                        store.queue_blit(rect, &p.key);
                    } else {
                        f.render_widget(Image::new(proto), rect);
                    }
                }
            }
            EmoteMode::Fb(fb) => {
                // fill the reserved cells with NBSP — renders blank like a
                // space, but DIFFERS from the space that scrolls in behind
                // a departing emote. ratatui's diff then re-emits exactly
                // those cells and the console itself erases the stale
                // pixels before we blit. plain spaces diff as "unchanged",
                // the console never repaints them, and old emote pixels
                // smear across the screen during fast scroll.
                fill_nbsp(
                    f,
                    Rect {
                        x,
                        y,
                        width: p.w,
                        height: h,
                    },
                );
                fb.push(x, y, &p.key);
            }
            EmoteMode::Text => {}
        }
    }
}

/// fill `rect` with NBSP: blank like a space, but different from the space that
/// scrolls in behind a departing emote, so ratatui's diff rewrites exactly those
/// cells and the terminal erases the stale pixels.
fn fill_nbsp(f: &mut Frame, rect: Rect) {
    let pad = "\u{a0}".repeat(rect.width as usize);
    let lines: Vec<Line> = (0..rect.height).map(|_| Line::raw(pad.clone())).collect();
    f.render_widget(Paragraph::new(lines), rect);
}

/// rover-style channel manager: a single-pane list, cursor row highlighted, the
/// active channel marked. reorder/leave/open from here.
fn draw_manage(f: &mut Frame, area: Rect, app: &App) {
    let [head, list] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" channels ", palette::TAG),
            Span::styled(format!("  {} open", app.channels.len()), palette::DIM),
        ])),
        head,
    );

    if app.channels.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  empty — press a to add a channel",
                palette::DIM,
            ))),
            list,
        );
        return;
    }

    for (i, ch) in app.channels.iter().enumerate() {
        if i as u16 >= list.height {
            break;
        }
        let sel = i == app.manage_cursor;
        let style = if sel {
            palette::TAG
        } else {
            palette::heat(Tier::of(ch.heat))
        };
        let cursor = if sel { "❯" } else { " " };
        let active = if i == app.focus { "●" } else { " " };
        let tags = ch
            .subs()
            .map(|(p, _)| p.tag())
            .collect::<Vec<_>>()
            .join("+");
        let label = format!(
            " {cursor} {active} {}·{}   {:>6.0}   {} msg",
            ch.name,
            tags,
            ch.heat,
            ch.messages.len(),
        );
        let row = Rect {
            x: list.x,
            y: list.y + i as u16,
            width: list.width,
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(fit(label, list.width), style))),
            row,
        );
    }
}

fn heat_bar(heat: f64, width: usize) -> Line<'static> {
    let width = width.max(1);
    let frac = (heat / heatsync_core::heat::MYTHIC).clamp(0.0, 1.0);
    let filled = (frac * width as f64).round() as usize;
    let hue = palette::heat_fill(Tier::of(heat));
    Line::from(vec![
        Span::styled("\u{2588}".repeat(filled), hue),
        Span::styled("\u{2591}".repeat(width - filled), palette::TRACK),
    ])
}

/// one `key label` footer hint: brand-colored key, dim label. every hint bar
/// shows exactly ONE key per action — aliases stay out of the footer.
fn hint(k: &'static str, d: &'static str) -> [Span<'static>; 2] {
    [
        Span::styled(k, palette::HEAD),
        Span::styled(format!(" {d}  "), palette::DIM),
    ]
}

/// live feed, no heatsync login: sends and mod tools won't work. the demo feed
/// never needs one.
fn logged_out_live(app: &App) -> bool {
    app.hs.is_none() && matches!(app.feed, Feed::Live { .. })
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App, n: usize) {
    let tag = palette::TAG;
    // Manage mode → rover-style key hints.
    if app.mode == InputMode::Manage {
        let mut spans = vec![Span::styled(" manage ", tag), Span::raw("  ")];
        for pair in [
            hint("jk", "move"),
            hint("enter", "open"),
            hint("a", "add"),
            hint("e", "edit"),
            hint("x", "leave"),
            hint("JK", "reorder"),
            hint("esc", "back"),
        ] {
            spans.extend(pair);
        }
        if let Some(msg) = &app.status {
            spans.push(Span::styled(format!(" {msg}"), palette::WARN));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }
    // SlotPick → show the cursor row's three slots, pick one with t/k/y.
    if app.mode == InputMode::SlotPick {
        let mut spans = vec![Span::styled(" edit ", tag), Span::raw("  ")];
        if let Some(ch) = app.channels.get(app.manage_cursor) {
            for (key, p) in [
                ("t", Platform::Twitch),
                ("k", Platform::Kick),
                ("y", Platform::Youtube),
            ] {
                spans.push(Span::styled(key, palette::HEAD));
                spans.push(Span::styled(
                    format!(
                        " {}:{}  ",
                        p.tag(),
                        slot_of(ch, p).unwrap_or_else(|| "—".to_string())
                    ),
                    palette::DIM,
                ));
            }
        }
        spans.extend(hint("esc", "back"));
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }
    // SlotEdit → type the slot's new name, join-prompt style.
    if let InputMode::SlotEdit(p) = app.mode {
        let spans = vec![
            Span::styled(format!(" edit {} ", p.tag()), tag),
            Span::styled(" ❯ ", palette::TEXT),
            Span::styled(app.input.clone(), palette::TEXT),
            Span::styled("\u{2588}", palette::TEXT),
            Span::styled("   enter apply · empty clears · esc back", palette::DIM),
        ];
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }
    if app.mode == InputMode::Pick {
        let mut spans = vec![Span::styled(" pick ", tag), Span::raw("  ")];
        for pair in [
            hint("type", "filter"),
            hint("↑↓", "move"),
            hint("enter", "join"),
            hint("+", "merge"),
            hint("esc", "close"),
        ] {
            spans.extend(pair);
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }
    // Insert mode → the message composer for the focused channel.
    if app.mode == InputMode::Insert && !app.channels.is_empty() {
        let ch = &app.channels[app.focus];
        // read-only only when there's genuinely no send path — the same order
        // send_line tries: heatsync login, then the user's own token, then the
        // ws relay. the mock feed echoes locally, so it always composes.
        let hs = app.hs.is_some();
        let readonly = match (&app.feed, ch.kind, ch.platform) {
            (Feed::Mock(_), ..) => false,
            // mentions still takes slash commands.
            (_, TabKind::Mentions, _) => false,
            (_, TabKind::Feed | TabKind::Whispers, _) => !hs,
            (_, _, Platform::Twitch) => !hs && app.twitch_tx.is_none(),
            (_, _, Platform::Kick) => !hs && app.kick_tx.is_none() && app.out.is_none(),
            (_, _, Platform::Youtube) => app.out.is_none() || app.special.ws_cli,
        };
        // the tag names the mode as well as the target, so `esc` never leaves
        // you guessing whether a keystroke will type or command.
        let normal = app.line.mode() == edit::Mode::Normal;
        // a virtual tab has no platform, so it carries no ·tw.
        let target = if ch.kind.is_special() {
            ch.name.clone()
        } else {
            format!("{}·{}", ch.name, ch.platform.tag())
        };
        let prompt = if normal {
            format!(" {target} NORMAL ")
        } else {
            format!(" {target} ")
        };
        let tag = palette::TAG;
        let mut spans = vec![
            Span::styled(prompt, tag),
            Span::styled(" ❯ ", palette::TEXT),
        ];
        // replying: a dim chip names who, and goes with the message.
        if let Some((_, user)) = &app.reply {
            spans.push(Span::styled(format!("↪@{user} "), palette::DIM_TAG));
        }
        // the warning stands in for an empty line only — whatever is typed
        // (a /join, a /w) always stays visible.
        if readonly && app.line.text().is_empty() {
            spans.push(Span::styled(
                "read-only — no send token · esc",
                palette::WARN,
            ));
        } else {
            // draw the line with a block cursor sitting ON a character in normal
            // mode and between characters in insert — the same shape vi has, so
            // the mode is readable without looking at the tag.
            let chars: Vec<char> = app.line.text().chars().collect();
            let at = app.line.cursor();
            let body = palette::TEXT;
            let block = palette::SEL;
            let take = |r: std::ops::Range<usize>| -> String { chars[r].iter().collect() };
            spans.push(Span::styled(take(0..at.min(chars.len())), body));
            if at < chars.len() {
                spans.push(Span::styled(take(at..at + 1), block));
                spans.push(Span::styled(take(at + 1..chars.len()), body));
            } else {
                spans.push(Span::styled(" ", block));
            }
            if !app.line.pending().is_empty() {
                spans.push(Span::styled(
                    format!("  {}", app.line.pending()),
                    palette::HEAD,
                ));
            }
            // a command's reply (usage, "not open: x") has to be visible from
            // the composer — that is where the command was typed.
            if let Some(msg) = &app.status {
                spans.push(Span::styled(format!("   {msg}"), palette::WARN));
            } else if app.line.is_empty() && !normal {
                let hint = match ch.kind {
                    TabKind::Feed => "   text posts to the feed  ·  /op /re  ·  r on a row replies",
                    TabKind::Whispers => {
                        "   /w <user> <msg>  ·  /dm <user> <msg>  ·  r on a row replies"
                    }
                    TabKind::Mentions => {
                        "   commands only  ·  /join /w /dm  ·  esc, enter jumps to the source"
                    }
                    TabKind::Channel => {
                        "   tab completes emotes/@users  ·  /join /part /quit  ·  text goes to chat"
                    }
                };
                spans.push(Span::styled(hint, palette::DIM));
            } else if normal {
                spans.push(Span::styled("   kj history  esc leave", palette::DIM));
            }
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }

    let (dot, dot_color, state) = match &app.feed {
        Feed::Mock(_) => ("\u{25cb} ", palette::DIM, "mock".to_string()),
        Feed::Live {
            connected: true, ..
        } => ("\u{25cf} ", palette::LIVE, "live".to_string()),
        Feed::Live {
            connected: false, ..
        } => ("\u{25cf} ", palette::WARN, "connecting".to_string()),
    };
    let mut spans = vec![Span::styled(" heatsync ", palette::TAG)];
    // message cursor: how much arrived while parked, and the keys that work here.
    if app.mode == InputMode::Cursor {
        if let Some(ch) = app.channels.get(app.focus) {
            let newer = scroll::newer(ch.seq, ch.messages.len(), app.cursor);
            spans.push(Span::styled(
                format!(" -- {newer} new --  "),
                palette::DIM_TAG,
            ));
        }
        for pair in [
            hint("jk", "move"),
            hint("r", "reply"),
            hint("y", "yank"),
            hint("d", "delete"),
            hint("G", "live"),
        ] {
            spans.extend(pair);
        }
        if let Some(msg) = &app.status {
            spans.push(Span::styled(format!("  {msg}"), palette::WARN));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }
    // a message (search miss, send error) takes the front of the line the way
    // vi's command line does — appended after the key hints it was simply
    // truncated away on anything narrower than ~110 columns.
    if let Some(msg) = &app.status {
        spans.push(Span::styled(format!("  {msg}"), palette::WARN));
    } else if logged_out_live(app) {
        spans.push(Span::styled(
            "  not logged in — q then `hs login`",
            palette::DIM,
        ));
    }
    // essentials only, one key per action — the full set fits a phone-width
    // terminal.
    spans.push(Span::raw("  "));
    for pair in [
        hint("jk", "chan"),
        hint("i", "say"),
        hint("o", "pick"),
        hint("m", "manage"),
        hint("q", "quit"),
    ] {
        spans.extend(pair);
    }
    if app.paused {
        spans.push(Span::styled("PAUSED  ", palette::WARN_BOLD));
    }
    spans.push(Span::styled(dot, dot_color));
    spans.push(Span::styled(format!("{state} · {n} ch"), palette::DIM));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[cfg(test)]
mod fb_reserve_tests {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    /// the framebuffer tier's stale-pixel defense: reserved emote cells hold
    /// NBSP, so when an emote departs and ordinary spaces scroll in, ratatui's
    /// diff re-emits those cells and the console erases the old pixels. if the
    /// reservation were plain spaces the diff would be empty and pixels would
    /// smear during fast scroll — this pins both halves of that contract.
    #[test]
    fn nbsp_reservation_diffs_against_space() {
        let area = Rect::new(0, 0, 4, 1);
        let reserved = Buffer::with_lines(["\u{a0}\u{a0}\u{a0} "]);
        let scrolled = Buffer::filled(area, ratatui::buffer::Cell::EMPTY);
        // emote leaves → every reserved cell re-emits (console erases pixels)
        assert_eq!(reserved.diff(&scrolled).len(), 3);
        // plain spaces would be invisible to the diff (the smear bug)
        assert!(scrolled.diff(&scrolled).is_empty());
    }
}
