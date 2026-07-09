//! heatsync tui — heat-sorted live multichat. `heatsync [channels…]` opens the
//! grid (default demo set); `--mock` runs the offline synthetic feed; `log`,
//! `search`, `hot` are headless corpus subcommands.
//! keys: q quit · j/k scroll focused · 1-9 focus · tab cycle · space pause

#[allow(dead_code)]
mod cli;
#[allow(dead_code)]
mod emote;
mod http;
mod net;

use std::io;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use heatsync_core::emote::{tokenize, EmoteSet, Token};
use heatsync_core::heat::Tier;
use heatsync_core::{mock, Channel, Message, Platform};
use net::{ChatEvent, Sub};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame, Terminal};

const BRAND: Color = Color::Indexed(208);

/// feed source: offline synthetic, or the live relay thread.
enum Feed {
    Mock(mock::Driver),
    Live {
        rx: Receiver<ChatEvent>,
        start: Instant,
        connected: bool,
    },
}

struct App {
    channels: Vec<Channel>,
    emotes: Vec<EmoteSet>, // index-aligned with channels
    focus: usize,
    scroll: usize,
    paused: bool,
    feed: Feed,
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("log") => return cli::log(&args[1..]),
        Some("search") => return cli::search(&args[1..]),
        Some("hot") | Some("top") => return cli::hot(&args[1..]),
        Some("probe") => return cli::probe(&args[1..]),
        _ => {}
    }

    let mock_mode = args.iter().any(|a| a == "--mock");
    let chan_args: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();

    let app = if mock_mode {
        App {
            channels: mock::channels(),
            emotes: (0..mock::channels().len()).map(|_| EmoteSet::new()).collect(),
            focus: 0,
            scroll: 0,
            paused: false,
            feed: Feed::Mock(mock::Driver::new()),
        }
    } else {
        build_live(&chan_args)
    };

    let mut terminal = ratatui::init();
    let res = run(&mut terminal, app);
    ratatui::restore();
    res
}

/// parse channel args (`name`, `twitch:name`, `kick:name`), fetch emote sets,
/// and start the live feed. prints a one-line loading note before raw mode.
fn build_live(chan_args: &[&String]) -> App {
    let subs: Vec<Sub> = if chan_args.is_empty() {
        vec![
            (Platform::Twitch, "xqc".into()),
            (Platform::Twitch, "forsen".into()),
            (Platform::Twitch, "zackrawrr".into()),
            (Platform::Kick, "trainwreckstv".into()),
        ]
    } else {
        chan_args.iter().map(|a| parse_sub(a)).collect()
    };

    eprintln!("heatsync: fetching emotes + connecting to {} channels…", subs.len());
    let mut channels = Vec::new();
    let mut emotes = Vec::new();
    for (platform, name) in &subs {
        channels.push(Channel::new(name, *platform, 200));
        emotes.push(http::emote_set(name, *platform).unwrap_or_default());
    }

    let rx = net::spawn(subs);
    App {
        channels,
        emotes,
        focus: 0,
        scroll: 0,
        paused: false,
        feed: Feed::Live {
            rx,
            start: Instant::now(),
            connected: false,
        },
    }
}

fn parse_sub(tok: &str) -> Sub {
    if let Some(rest) = tok.strip_prefix("kick:") {
        (Platform::Kick, rest.to_string())
    } else if let Some(rest) = tok.strip_prefix("twitch:") {
        (Platform::Twitch, rest.to_string())
    } else {
        (Platform::Twitch, tok.to_string())
    }
}

fn run<B: ratatui::backend::Backend>(terminal: &mut Terminal<B>, mut app: App) -> io::Result<()> {
    let tick = Duration::from_millis(200);
    let mut last = Instant::now();

    loop {
        terminal.draw(|f| ui(f, &app))?;

        let wait = tick.saturating_sub(last.elapsed());
        if event::poll(wait)? {
            if let Event::Key(k) = event::read()? {
                if matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                    let order = sorted_order(&app.channels);
                    match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                            return Ok(())
                        }
                        KeyCode::Char('j') => app.scroll = app.scroll.saturating_add(1),
                        KeyCode::Char('k') => app.scroll = app.scroll.saturating_sub(1),
                        KeyCode::Char(' ') => app.paused = !app.paused,
                        KeyCode::Tab => {
                            let pos = order.iter().position(|&i| i == app.focus).unwrap_or(0);
                            app.focus = order[(pos + 1) % order.len()];
                            app.scroll = 0;
                        }
                        KeyCode::Char(c @ '1'..='9') => {
                            let idx = c as usize - '1' as usize;
                            if idx < order.len() {
                                app.focus = order[idx];
                                app.scroll = 0;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        if last.elapsed() >= tick {
            if !app.paused {
                advance(&mut app);
            }
            last = Instant::now();
        }
    }
}

/// pull new data into the channel buffers for this tick.
fn advance(app: &mut App) {
    match &mut app.feed {
        Feed::Mock(driver) => driver.tick(&mut app.channels),
        Feed::Live { rx, start, connected } => {
            let now = start.elapsed().as_millis() as u64;
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    ChatEvent::Line(l) => {
                        if let Some(ch) = app.channels.iter_mut().find(|c| {
                            c.platform == l.platform && c.name.eq_ignore_ascii_case(&l.channel)
                        }) {
                            ch.record(
                                Message {
                                    user: l.user,
                                    text: l.content,
                                    color: l.color,
                                    heat: 0.0,
                                },
                                now,
                            );
                        }
                    }
                    ChatEvent::Connected => *connected = true,
                    ChatEvent::Disconnected => *connected = false,
                }
            }
            for ch in &mut app.channels {
                ch.cool(now);
            }
        }
    }
}

fn sorted_order(channels: &[Channel]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..channels.len()).collect();
    order.sort_by(|&a, &b| {
        channels[b]
            .heat
            .partial_cmp(&channels[a].heat)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(channels[a].name.cmp(&channels[b].name))
    });
    order
}

fn ui(f: &mut Frame, app: &App) {
    let [grid, footer] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(f.area());

    let order = sorted_order(&app.channels);
    if order.is_empty() {
        return;
    }
    let cols = Layout::horizontal(order.iter().map(|_| Constraint::Fill(1)).collect::<Vec<_>>())
        .split(grid);

    for (slot, (&ch_idx, area)) in order.iter().zip(cols.iter()).enumerate() {
        let scroll = if ch_idx == app.focus { app.scroll } else { 0 };
        draw_channel(
            f,
            *area,
            &app.channels[ch_idx],
            &app.emotes[ch_idx],
            slot + 1,
            ch_idx == app.focus,
            scroll,
        );
    }

    draw_footer(f, footer, &app, order.len());
}

fn draw_channel(
    f: &mut Frame,
    area: Rect,
    ch: &Channel,
    set: &EmoteSet,
    slot: usize,
    focused: bool,
    scroll: usize,
) {
    let tier = Tier::of(ch.heat);
    let hue = Color::Indexed(tier.xterm());
    let border = if focused { BRAND } else { Color::Indexed(240) };

    let mut title = vec![
        Span::styled(format!(" {slot} "), Style::default().fg(Color::Black).bg(border)),
        Span::raw(" "),
        Span::styled(&ch.name, Style::default().fg(hue).add_modifier(Modifier::BOLD)),
        Span::styled(format!("·{}", ch.platform.tag()), Style::default().fg(Color::Indexed(240))),
        Span::raw("  "),
        Span::styled(format!("{:.0}", ch.heat), Style::default().fg(hue).add_modifier(Modifier::BOLD)),
    ];
    if !tier.marker().is_empty() {
        title.push(Span::raw(" "));
        title.push(Span::styled(
            tier.marker(),
            Style::default().fg(Color::Indexed(231)).add_modifier(Modifier::BOLD),
        ));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(Line::from(title));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let bar_area = Rect { height: 1, ..inner };
    f.render_widget(Paragraph::new(heat_bar(ch.heat, inner.width as usize)), bar_area);

    let body = Rect { y: inner.y + 1, height: inner.height - 1, ..inner };
    let cap = body.height as usize;
    let total = ch.messages.len();
    let end = total.saturating_sub(scroll);
    let start = end.saturating_sub(cap);
    let lines: Vec<Line> = ch.messages.iter().take(end).skip(start).map(|m| line_for(m, set)).collect();
    f.render_widget(Paragraph::new(lines), body);
}

/// one chat line: user (own color if sent) then text split into words + emotes.
/// emotes render as their name in brand color for now — the image layer lands next.
fn line_for(m: &Message, set: &EmoteSet) -> Line<'static> {
    let text_hue = Color::Indexed(heatsync_core::heat::color(m.heat));
    let user_color = m
        .color
        .as_deref()
        .and_then(parse_hex)
        .unwrap_or(Color::Indexed(244));
    let mut spans = vec![
        Span::styled(m.user.clone(), Style::default().fg(user_color)),
        Span::styled(": ", Style::default().fg(Color::Indexed(238))),
    ];
    for tok in tokenize(&m.text, set) {
        match tok {
            Token::Text(t) => spans.push(Span::styled(t, Style::default().fg(text_hue))),
            Token::Emote(name) => spans.push(Span::styled(
                name,
                Style::default().fg(BRAND).add_modifier(Modifier::BOLD),
            )),
        }
    }
    Line::from(spans)
}

/// `#rrggbb` → nearest-ish terminal color (truecolor if the term supports it).
fn parse_hex(hex: &str) -> Option<Color> {
    let h = hex.strip_prefix('#')?;
    if h.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&h[0..2], 16).ok()?;
    let g = u8::from_str_radix(&h[2..4], 16).ok()?;
    let b = u8::from_str_radix(&h[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

fn heat_bar(heat: f64, width: usize) -> Line<'static> {
    let width = width.max(1);
    let frac = (heat / heatsync_core::heat::MYTHIC).clamp(0.0, 1.0);
    let filled = (frac * width as f64).round() as usize;
    let hue = Color::Indexed(heatsync_core::heat::color(heat));
    Line::from(vec![
        Span::styled("\u{2588}".repeat(filled), Style::default().fg(hue)),
        Span::styled("\u{2591}".repeat(width - filled), Style::default().fg(Color::Indexed(236))),
    ])
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App, n: usize) {
    let (dot, dot_color, state) = match &app.feed {
        Feed::Mock(_) => ("\u{25cb} ", Color::Indexed(244), "mock".to_string()),
        Feed::Live { connected: true, .. } => ("\u{25cf} ", Color::Indexed(46), "live".to_string()),
        Feed::Live { connected: false, .. } => ("\u{25cf} ", Color::Indexed(214), "connecting".to_string()),
    };
    let mut spans = vec![
        Span::styled(" heatsync ", Style::default().fg(Color::Black).bg(BRAND).add_modifier(Modifier::BOLD)),
        Span::styled("  q ", Style::default().fg(BRAND)),
        Span::raw("quit  "),
        Span::styled("j/k ", Style::default().fg(BRAND)),
        Span::raw("scroll  "),
        Span::styled("1-9 ", Style::default().fg(BRAND)),
        Span::raw("focus  "),
        Span::styled("tab ", Style::default().fg(BRAND)),
        Span::raw("cycle  "),
    ];
    if app.paused {
        spans.push(Span::styled("PAUSED  ", Style::default().fg(Color::Indexed(214)).add_modifier(Modifier::BOLD)));
    }
    spans.push(Span::styled(dot, Style::default().fg(dot_color)));
    spans.push(Span::styled(format!("{state} · {n} ch"), Style::default().fg(Color::Indexed(244))));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}
