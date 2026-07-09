//! heatsync tui — heat-sorted live multichat. `heatsync [channels…]` opens the
//! grid (default demo set); `--mock` runs the offline synthetic feed; `log`,
//! `search`, `hot` are headless corpus subcommands.
//! keys: q quit · j/k scroll focused · 1-9 focus · tab cycle · space pause

#[allow(dead_code)]
mod cli;
mod emote;
mod http;
mod net;

use std::io;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use emote::fb::FbEmotes;
use emote::render::{EmoteStore, EMOTE_H, EMOTE_W};
use heatsync_core::emote::{tokenize, EmoteSet, Token};
use heatsync_core::heat::Tier;
use heatsync_core::{mock, Channel, Message, Platform};
use net::{ChatEvent, Sub};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame, Terminal};
use ratatui_image::Image;
use unicode_width::UnicodeWidthStr;

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

/// vim-style modes: navigate in Normal, type a message in Insert.
#[derive(Clone, Copy, PartialEq)]
enum InputMode {
    Normal,
    Insert,
}

struct App {
    channels: Vec<Channel>,
    emotes: Vec<EmoteSet>, // index-aligned with channels
    focus: usize,
    scroll: usize,
    paused: bool,
    feed: Feed,
    store: Option<EmoteStore>, // terminal graphics tier (sixel/kitty/…)
    fb: Option<FbEmotes>,      // bare-console framebuffer tier (TERM=linux)
    mode: InputMode,
    input: String,
    status: Option<String>,          // transient one-line notice (send errors, etc.)
    out: Option<net::Tx>,            // outbound channel to the live WS thread
}

/// which emote backend a channel column should draw with this frame.
#[derive(Clone, Copy)]
enum EmoteMode<'a> {
    Term(&'a EmoteStore),
    Fb(&'a FbEmotes),
    Text,
}

impl EmoteMode<'_> {
    /// is this emote loaded + ready to draw? (drives reserve-cells vs show-name)
    fn ready(&self, url: &str) -> bool {
        match self {
            EmoteMode::Term(s) => s.is_ready(url),
            EmoteMode::Fb(f) => f.is_ready(url),
            EmoteMode::Text => false,
        }
    }
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
            store: None,
            fb: None,
            mode: InputMode::Normal,
            input: String::new(),
            status: None,
            out: None,
        }
    } else {
        build_live(&chan_args)
    };

    // must probe the terminal for graphics BEFORE raw mode / alt screen.
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
    // pick the emote tier now, while we're still a normal terminal:
    //   1. a graphics-capable emulator (sixel/kitty) → EmoteStore
    //   2. else a bare Linux console with /dev/fb0 → framebuffer tier
    //   3. else text-only
    let store = EmoteStore::new();
    let fb = if store.is_none() {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        FbEmotes::open(cols, rows)
    } else {
        None
    };
    let mut channels = Vec::new();
    let mut emotes = Vec::new();
    for (platform, name) in &subs {
        channels.push(Channel::new(name, *platform, 200));
        emotes.push(http::emote_set(name, *platform).unwrap_or_default());
    }

    let token = std::env::var("HEATSYNC_TOKEN").ok().filter(|t| !t.is_empty());
    let (rx, out) = net::spawn(subs, token);
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
        store,
        fb,
        mode: InputMode::Normal,
        input: String::new(),
        status: None,
        out: Some(out),
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
        // console tier: paint emote pixels onto the reserved cells now that the
        // text has flushed. terminal tiers draw inline during the frame above.
        if let Some(fb) = &app.fb {
            fb.blit();
        }

        let wait = tick.saturating_sub(last.elapsed());
        if event::poll(wait)? {
            if let Event::Key(k) = event::read()? {
                if matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                    && handle_key(&mut app, k) == Flow::Quit
                {
                    return Ok(());
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

#[derive(PartialEq)]
enum Flow {
    Continue,
    Quit,
}

/// dispatch a keypress by mode. Normal = hjkl navigation; Insert = type a line.
fn handle_key(app: &mut App, k: crossterm::event::KeyEvent) -> Flow {
    if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
        return Flow::Quit;
    }
    match app.mode {
        InputMode::Insert => match k.code {
            KeyCode::Esc => app.mode = InputMode::Normal,
            KeyCode::Enter => send_focused(app),
            KeyCode::Backspace => {
                app.input.pop();
            }
            KeyCode::Char(c) => app.input.push(c),
            _ => {}
        },
        InputMode::Normal => {
            let order = sorted_order(&app.channels);
            let pos = order.iter().position(|&i| i == app.focus).unwrap_or(0);
            match k.code {
                KeyCode::Char('q') => return Flow::Quit,
                // h/l move focus between columns (left/right), j/k scroll.
                KeyCode::Char('h') => {
                    if pos > 0 {
                        app.focus = order[pos - 1];
                        app.scroll = 0;
                    }
                }
                KeyCode::Char('l') => {
                    if pos + 1 < order.len() {
                        app.focus = order[pos + 1];
                        app.scroll = 0;
                    }
                }
                KeyCode::Char('j') => app.scroll = app.scroll.saturating_sub(1), // toward newest
                KeyCode::Char('k') => app.scroll = app.scroll.saturating_add(1), // toward older
                KeyCode::Char('g') => app.scroll = usize::MAX / 2,               // jump to oldest loaded
                KeyCode::Char('G') => app.scroll = 0,                            // jump to newest
                KeyCode::Char('i') | KeyCode::Char('a') => {
                    app.mode = InputMode::Insert;
                    app.status = None;
                }
                KeyCode::Char(' ') => app.paused = !app.paused,
                KeyCode::Esc => app.status = None,
                _ => {}
            }
        }
    }
    Flow::Continue
}

/// send the current input line to the focused channel (kick only; twitch has no
/// send path). clears the input on a successful enqueue.
fn send_focused(app: &mut App) {
    let text = app.input.trim().to_string();
    if text.is_empty() {
        return;
    }
    let (platform, name) = {
        let c = &app.channels[app.focus];
        (c.platform, c.name.clone())
    };
    match platform {
        Platform::Twitch => {
            app.status = Some("twitch is read-only — no send path".into());
        }
        Platform::Kick => match &app.out {
            Some(out) => {
                let _ = out.send(net::Outbound::Chat {
                    platform: Platform::Kick,
                    channel: name.clone(),
                    text,
                });
                app.input.clear();
                app.status = Some(format!("sent → {name}"));
            }
            None => app.status = Some("not connected".into()),
        },
    }
}

/// pull new data into the channel buffers for this tick, and keep the emote
/// image cache warm for what's on screen.
fn advance(app: &mut App) {
    let App { channels, emotes, feed, store, fb, status, .. } = app;
    match feed {
        Feed::Mock(driver) => driver.tick(channels),
        Feed::Live { rx, start, connected } => {
            let now = start.elapsed().as_millis() as u64;
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    ChatEvent::Line(l) => {
                        if let Some(ch) = channels.iter_mut().find(|c| {
                            c.platform == l.platform && c.name.eq_ignore_ascii_case(&l.channel)
                        }) {
                            ch.record(
                                Message { user: l.user, text: l.content, color: l.color, heat: 0.0 },
                                now,
                            );
                        }
                    }
                    ChatEvent::Connected => *connected = true,
                    ChatEvent::Disconnected => *connected = false,
                    ChatEvent::Auth(ok) => {
                        *status = Some(if ok { "authenticated".into() } else { "auth failed — check HEATSYNC_TOKEN".into() });
                    }
                    ChatEvent::SendResult { ok, error } => {
                        if !ok {
                            *status = Some(format!("send failed: {}", error.unwrap_or_default()));
                        }
                    }
                }
            }
            for ch in channels.iter_mut() {
                ch.cool(now);
            }
        }
    }

    // request loads for emotes near the bottom of each channel (whichever tier
    // is active), then drain finished loads. request() is idempotent.
    let want_urls = |f: &mut dyn FnMut(&str)| {
        for (ci, ch) in channels.iter().enumerate() {
            let set = &emotes[ci];
            if set.is_empty() {
                continue;
            }
            let start = ch.messages.len().saturating_sub(60);
            for m in ch.messages.iter().skip(start) {
                for tok in tokenize(&m.text, set) {
                    if let Token::Emote(name) = tok {
                        if let Some(e) = set.get(&name) {
                            f(&e.url);
                        }
                    }
                }
            }
        }
    };
    if let Some(store) = store {
        want_urls(&mut |u| store.request(u));
        store.pump();
    } else if let Some(fb) = fb {
        want_urls(&mut |u| fb.request(u));
        fb.pump();
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

    let mode = if let Some(s) = &app.store {
        EmoteMode::Term(s)
    } else if let Some(f) = &app.fb {
        EmoteMode::Fb(f)
    } else {
        EmoteMode::Text
    };

    for (slot, (&ch_idx, area)) in order.iter().zip(cols.iter()).enumerate() {
        let scroll = if ch_idx == app.focus { app.scroll } else { 0 };
        draw_channel(
            f,
            *area,
            &app.channels[ch_idx],
            &app.emotes[ch_idx],
            mode,
            slot + 1,
            ch_idx == app.focus,
            scroll,
        );
    }

    draw_footer(f, footer, app, order.len());
}

#[allow(clippy::too_many_arguments)]
fn draw_channel(
    f: &mut Frame,
    area: Rect,
    ch: &Channel,
    set: &EmoteSet,
    mode: EmoteMode,
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

    // one message per row; text via paragraph. ready emotes get blank cells
    // reserved, then filled by the active tier (terminal image widget inline, or
    // framebuffer pixels after the frame). unloaded emotes show their name.
    for (r, m) in ch.messages.iter().take(end).skip(start).enumerate() {
        let y = body.y + r as u16;
        let (line, places) = layout_message(m, set, mode, body.width);
        f.render_widget(Paragraph::new(line), Rect { x: body.x, y, width: body.width, height: 1 });
        for p in &places {
            let x = body.x + p.col;
            match mode {
                EmoteMode::Term(store) => {
                    if let Some(proto) = store.frame(&p.url) {
                        f.render_widget(Image::new(proto), Rect { x, y, width: EMOTE_W, height: EMOTE_H });
                    }
                }
                EmoteMode::Fb(fb) => fb.push(x, y, &p.url),
                EmoteMode::Text => {}
            }
        }
    }
}

/// a reserved emote slot: column offset within the channel body + its url.
struct Place {
    col: u16,
    url: String,
}

/// lay a message onto one row: the text line (with blank cells where ready
/// emotes go) plus the reserved emote slots. an emote that isn't loaded yet (or
/// in text mode) falls back to its name in brand color.
fn layout_message(m: &Message, set: &EmoteSet, mode: EmoteMode, maxw: u16) -> (Line<'static>, Vec<Place>) {
    let text_hue = Color::Indexed(heatsync_core::heat::color(m.heat));
    let user_color = m.color.as_deref().and_then(parse_hex).unwrap_or(Color::Indexed(244));
    let mut spans = vec![
        Span::styled(m.user.clone(), Style::default().fg(user_color)),
        Span::styled(": ", Style::default().fg(Color::Indexed(238))),
    ];
    let mut col = UnicodeWidthStr::width(m.user.as_str()) as u16 + 2;
    let mut places = Vec::new();

    for tok in tokenize(&m.text, set) {
        if col >= maxw {
            break;
        }
        match tok {
            Token::Text(t) => {
                col += UnicodeWidthStr::width(t.as_str()) as u16;
                spans.push(Span::styled(t, Style::default().fg(text_hue)));
            }
            Token::Emote(name) => {
                let url = set.get(&name).map(|e| e.url.as_str());
                let ready = col + EMOTE_W <= maxw && url.map_or(false, |u| mode.ready(u));
                if let (true, Some(u)) = (ready, url) {
                    places.push(Place { col, url: u.to_string() });
                    spans.push(Span::raw(" ".repeat(EMOTE_W as usize)));
                    col += EMOTE_W;
                } else {
                    col += UnicodeWidthStr::width(name.as_str()) as u16;
                    spans.push(Span::styled(name, Style::default().fg(BRAND).add_modifier(Modifier::BOLD)));
                }
            }
        }
    }
    (Line::from(spans), places)
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
    // Insert mode → the message composer for the focused channel.
    if app.mode == InputMode::Insert {
        let ch = &app.channels[app.focus];
        let readonly = ch.platform == Platform::Twitch;
        let prompt = format!(" {}·{} ", ch.name, ch.platform.tag());
        let mut spans = vec![
            Span::styled(prompt, Style::default().fg(Color::Black).bg(BRAND).add_modifier(Modifier::BOLD)),
            Span::styled(" ❯ ", Style::default().fg(BRAND)),
        ];
        if readonly {
            spans.push(Span::styled("twitch is read-only — esc", Style::default().fg(Color::Indexed(214))));
        } else {
            spans.push(Span::styled(app.input.clone(), Style::default().fg(Color::Indexed(231))));
            spans.push(Span::styled("\u{2588}", Style::default().fg(BRAND))); // cursor
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }

    let (dot, dot_color, state) = match &app.feed {
        Feed::Mock(_) => ("\u{25cb} ", Color::Indexed(244), "mock".to_string()),
        Feed::Live { connected: true, .. } => ("\u{25cf} ", Color::Indexed(46), "live".to_string()),
        Feed::Live { connected: false, .. } => ("\u{25cf} ", Color::Indexed(214), "connecting".to_string()),
    };
    let mut spans = vec![
        Span::styled(" heatsync ", Style::default().fg(Color::Black).bg(BRAND).add_modifier(Modifier::BOLD)),
        Span::styled("  hl ", Style::default().fg(BRAND)),
        Span::raw("chan  "),
        Span::styled("jk ", Style::default().fg(BRAND)),
        Span::raw("scroll  "),
        Span::styled("i ", Style::default().fg(BRAND)),
        Span::raw("say  "),
        Span::styled("q ", Style::default().fg(BRAND)),
        Span::raw("quit  "),
    ];
    if app.paused {
        spans.push(Span::styled("PAUSED  ", Style::default().fg(Color::Indexed(214)).add_modifier(Modifier::BOLD)));
    }
    if let Some(s) = &app.status {
        spans.push(Span::styled(format!("{s}  "), Style::default().fg(Color::Indexed(214))));
    }
    spans.push(Span::styled(dot, Style::default().fg(dot_color)));
    spans.push(Span::styled(format!("{state} · {n} ch"), Style::default().fg(Color::Indexed(244))));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}
