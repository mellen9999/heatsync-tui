//! heatsync tui — heat-sorted live multichat. `heatsync [channels…]` opens the
//! grid (default demo set); `--mock` runs the offline synthetic feed; `log`,
//! `search`, `hot` are headless corpus subcommands.
//! keys are vi: j/k move the cursor, h/l (or gt/gT) change channel, gg/G ends,
//! ctrl-d/u/f/b page, counts work (5j), v/V select, y yanks, / and ? search with
//! n/N, i composes, o joins, m manages, q quits.

#[allow(dead_code)]
mod cli;
mod clip;
mod config;
mod emote;
mod http;
mod kick;
mod net;
mod slash;
mod twitch;
mod vi;

use std::io;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use config::TabPos;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use emote::fb::FbEmotes;
use emote::render::{EmoteStore, EMOTE_H};
use heatsync_core::emote::{tokenize, EmoteSet, Token};
use heatsync_core::heat::Tier;
use heatsync_core::{mock, Channel, Message, Platform};
use net::{ChatEvent, Sub};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
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

/// vim-style modes: navigate in Normal, type a message in Insert, type a channel
/// to join in Join, or manage channels rover-style in Manage.
#[derive(Clone, Copy, PartialEq)]
enum InputMode {
    Normal,
    Visual,
    Insert,
    Join,
    Manage,
    /// typing a `/` or `?` pattern; `vi.dir` holds which way it will run.
    Search,
}

struct App {
    channels: Vec<Channel>,
    emotes: Vec<EmoteSet>, // index-aligned with channels
    focus: usize,
    /// cursor, selection, counts and search over the focused channel's ring.
    vi: vi::Vi,
    paused: bool,
    feed: Feed,
    store: Option<EmoteStore>, // terminal graphics tier (sixel/kitty/…)
    fb: Option<FbEmotes>,      // bare-console framebuffer tier (TERM=linux)
    mode: InputMode,
    input: String,
    status: Option<String>,          // transient one-line notice (send errors, etc.)
    out: Option<net::Tx>,            // outbound channel to the live WS thread
    twitch_tx: Option<std::sync::mpsc::Sender<twitch::Send>>, // direct twitch sender
    kick_tx: Option<std::sync::mpsc::Sender<kick::Send>>,     // direct kick sender
    tab_pos: TabPos,
    manage_cursor: usize, // cursor in the Manage view
    // emote sets are fetched off-thread (a blocking HTTP call would freeze the
    // UI); results arrive here keyed by (platform, name) and merge in.
    emote_tx: Sender<(Platform, String, EmoteSet)>,
    emote_rx: Receiver<(Platform, String, EmoteSet)>,
}

/// which emote backend a channel column should draw with this frame.
#[derive(Clone, Copy)]
enum EmoteMode<'a> {
    Term(&'a EmoteStore),
    Fb(&'a FbEmotes),
    Text,
}

impl EmoteMode<'_> {
    /// this emote's width in cells once it's loaded, else None. width is
    /// per-emote (derived from its real aspect and the terminal's cell aspect),
    /// so a square emote reads square and a wide one isn't crushed. `None`
    /// doubles as "not ready" — the layout falls back to the emote's name.
    fn cells(&self, url: &str) -> Option<u16> {
        match self {
            EmoteMode::Term(s) => s.cells(url),
            EmoteMode::Fb(f) => f.cells(url),
            EmoteMode::Text => None,
        }
    }

    /// the footprint a not-yet-loaded emote is laid out at. `None` in text mode,
    /// where emote names are just words on the line.
    fn square_cells(&self) -> Option<u16> {
        match self {
            EmoteMode::Term(s) => Some(s.square_cells()),
            EmoteMode::Fb(f) => Some(f.square_cells()),
            EmoteMode::Text => None,
        }
    }

    /// how long until any loaded emote flips to its next frame.
    fn next_flip_in(&self) -> Option<Duration> {
        match self {
            EmoteMode::Term(s) => s.next_flip_in(),
            EmoteMode::Fb(f) => f.next_flip_in(),
            EmoteMode::Text => None,
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
        Some("diag") => return cli::diag(&args[1..]),
        Some("render-test") => return cli::render_test(&args[1..]),
        Some("login") if args.get(1).map(String::as_str) == Some("kick") => return kick::login(),
        Some("login") => return cli::login(),
        _ => {}
    }

    let mock_mode = args.iter().any(|a| a == "--mock");
    let chan_args: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    let cfg = config::load();

    let app = if mock_mode {
        let (emote_tx, emote_rx) = std::sync::mpsc::channel();
        App {
            channels: mock::channels(),
            emotes: (0..mock::channels().len()).map(|_| EmoteSet::new()).collect(),
            focus: 0,
            vi: vi::Vi::default(),
            paused: false,
            feed: Feed::Mock(mock::Driver::new()),
            store: None,
            fb: None,
            mode: InputMode::Normal,
            input: String::new(),
            status: None,
            out: None,
            twitch_tx: None,
            kick_tx: None,
            tab_pos: cfg.tab_pos,
            manage_cursor: 0,
            emote_tx,
            emote_rx,
        }
    } else {
        build_live(&chan_args, cfg.tab_pos, cfg.channels)
    };

    // must probe the terminal for graphics BEFORE raw mode / alt screen.
    let had_fb = app.fb.is_some();
    let mut terminal = ratatui::init();
    let res = run(&mut terminal, app);
    ratatui::restore();
    if had_fb {
        // the bare console has no alternate screen to pop back to, so the dead
        // TUI text AND our framebuffer emote pixels would linger behind the
        // shell prompt. a real clear repaints every cell (wiping both).
        use std::io::Write;
        print!("\x1b[2J\x1b[H");
        let _ = io::stdout().flush();
    }
    res
}

/// parse channel args (`name`, `twitch:name`, `kick:name`), fetch emote sets,
/// and start the live feed. prints a one-line loading note before raw mode.
fn build_live(chan_args: &[&String], tab_pos: TabPos, saved: Vec<Sub>) -> App {
    // precedence: explicit CLI args → persisted tabs → empty (a clean "press o to
    // join" state; no hardcoded demo channels). first run starts blank; whatever
    // you open persists, so the next launch restores it.
    let subs: Vec<Sub> = if !chan_args.is_empty() {
        chan_args.iter().map(|a| parse_sub(a)).collect()
    } else {
        saved
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
    let tier = match (&store, &fb) {
        (Some(s), _) => s.tier_label(),
        (None, Some(_)) => "framebuffer",
        _ => "text (no terminal graphics)",
    };
    eprintln!("heatsync: emotes = {tier}");
    if store.is_none() && fb.is_none() {
        eprintln!(
            "  → for image emotes, run in a graphics terminal: foot (linux), \
             windows terminal (win), wezterm (any) — else names show as text"
        );
    }
    // channels open immediately with empty emote sets; each set is fetched on a
    // background thread and merged in via emote_rx once it lands (no UI stall).
    let (emote_tx, emote_rx) = std::sync::mpsc::channel();
    let mut channels = Vec::new();
    let mut emotes = Vec::new();
    for (platform, name) in &subs {
        channels.push(Channel::new(name, *platform, 200));
        emotes.push(EmoteSet::new());
        spawn_emote_fetch(&emote_tx, *platform, name.clone());
    }

    let token = std::env::var("HEATSYNC_TOKEN").ok().filter(|t| !t.is_empty());
    let (rx, out) = net::spawn(subs, token);
    // direct-to-platform sending if the user supplied their own platform tokens.
    let auth = config::load_auth();
    let kick_tx = auth.kick_token.clone().map(kick::spawn);
    let twitch_tx = match (auth.twitch_user, auth.twitch_oauth) {
        (Some(u), Some(o)) => Some(twitch::spawn(u, o)),
        _ => None,
    };
    App {
        channels,
        emotes,
        focus: 0,
        vi: vi::Vi::default(),
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
        twitch_tx,
        kick_tx,
        tab_pos,
        manage_cursor: 0,
        emote_tx,
        emote_rx,
    }
}

/// fetch a channel's emote set on a detached thread and hand it back over `tx`.
/// keeps the blocking HTTP call off the UI thread so joins never freeze.
fn spawn_emote_fetch(tx: &Sender<(Platform, String, EmoteSet)>, platform: Platform, name: String) {
    let tx = tx.clone();
    std::thread::spawn(move || {
        let set = http::emote_set(&name, platform).unwrap_or_default();
        let _ = tx.send((platform, name, set));
    });
}

/// merge any emote sets that finished fetching into their channel columns.
fn drain_emotes(app: &mut App) {
    while let Ok((platform, name, set)) = app.emote_rx.try_recv() {
        if let Some(i) = app
            .channels
            .iter()
            .position(|c| c.platform == platform && c.name.eq_ignore_ascii_case(&name))
        {
            app.emotes[i] = set;
        }
    }
}

/// persist the current tab set + bar position (live mode only — mock is ephemeral).
fn save_state(app: &App) {
    if !matches!(app.feed, Feed::Live { .. }) {
        return;
    }
    let channels = app
        .channels
        .iter()
        .map(|c| (c.platform, c.name.clone()))
        .collect();
    config::save(&config::Config { tab_pos: app.tab_pos, channels });
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
    // data cadence: pull chat + decay heat every `tick`. draw cadence: while an
    // animated emote is on the store, redraw every `anim_frame` (~20fps) so the
    // animation is smooth — ratatui diffs the buffer, so an idle frame re-emits
    // nothing and only the changed emote cells cost anything. text-only / static
    // views stay lazy (redraw only on the tick or a keypress).
    let tick = Duration::from_millis(200);
    let mut last = Instant::now();
    // smoothed cost of one draw. animations run at their authored rate, but never
    // faster than the terminal can actually paint — if a draw starts costing real
    // time (a slow pty, a screenful of sixels), the cadence stretches to match
    // instead of queueing writes the terminal renders as tearing. self-tuning, so
    // no protocol gets a hardcoded fps penalty.
    let mut draw_cost = Duration::ZERO;
    loop {
        drain_emotes(&mut app);
        // snapshot the animation clock + reset the per-draw blit budget, so every
        // emote in this frame is sampled at the same instant.
        if let Some(store) = &app.store {
            store.begin_frame();
        }
        let drew_at = Instant::now();
        terminal.draw(|f| ui(f, &app))?;
        // exponential moving average — one slow frame shouldn't throttle us, a
        // sustained slow terminal should.
        draw_cost = (draw_cost * 3 + drew_at.elapsed()) / 4;
        // console tier: paint emote pixels onto the reserved cells now that the
        // text has flushed. terminal tiers draw inline during the frame above.
        if let Some(fb) = &app.fb {
            fb.blit();
        }

        // sleep until whichever comes first: the data tick, or the exact instant
        // the soonest emote flips to its next frame. that's what makes animations
        // play at their authored fps instead of a fixed cadence — and it means a
        // text-only or all-static view never wakes up early at all.
        let tick_left = tick.saturating_sub(last.elapsed());
        let wait = match emote_mode(&app).next_flip_in() {
            // keep the pty at most ~half busy: never schedule the next frame
            // sooner than twice what the last draws actually cost.
            Some(flip) => flip.max(draw_cost * 2).min(tick_left),
            None => tick_left,
        };
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

/// dispatch a keypress by mode. Normal = navigation; Insert = compose a message;
/// Join = type a channel to open, Search = type a `/` pattern. j/k are always
/// cursor motion and h/l always change channel, whatever the tab bar's position
/// — the same keys shouldn't mean different things depending on a layout option.
fn handle_key(app: &mut App, k: crossterm::event::KeyEvent) -> Flow {
    if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
        return Flow::Quit;
    }
    match app.mode {
        InputMode::Insert => match k.code {
            KeyCode::Esc => app.mode = InputMode::Normal,
            KeyCode::Enter => return send_focused(app),
            KeyCode::Backspace => {
                app.input.pop();
            }
            KeyCode::Char(c) => {
                app.status = None; // typing dismisses the last command's reply
                app.input.push(c)
            }
            _ => {}
        },
        InputMode::Join => match k.code {
            KeyCode::Esc => {
                app.mode = InputMode::Normal;
                app.input.clear();
            }
            KeyCode::Enter => join_channel(app),
            KeyCode::Backspace => {
                app.input.pop();
            }
            KeyCode::Char(c) => app.input.push(c),
            _ => {}
        },
        // rover-style channel manager: j/k move, enter open, a add, d leave,
        // K/J reorder, esc back.
        InputMode::Manage => {
            let last = app.channels.len().saturating_sub(1);
            match k.code {
                KeyCode::Esc | KeyCode::Char('q') => app.mode = InputMode::Normal,
                KeyCode::Char('j') | KeyCode::Down => {
                    app.manage_cursor = (app.manage_cursor + 1).min(last)
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    app.manage_cursor = app.manage_cursor.saturating_sub(1)
                }
                KeyCode::Char('g') | KeyCode::Home => app.manage_cursor = 0,
                KeyCode::Char('G') | KeyCode::End => app.manage_cursor = last,
                KeyCode::Enter | KeyCode::Char('l') => {
                    if !app.channels.is_empty() {
                        app.focus = app.manage_cursor.min(last);
                        app.vi.reset();
                        app.mode = InputMode::Normal;
                    }
                }
                KeyCode::Char('a') | KeyCode::Char('n') | KeyCode::Char('o') => {
                    app.mode = InputMode::Join;
                    app.input.clear();
                }
                KeyCode::Char('d') | KeyCode::Char('x') => manage_delete(app),
                KeyCode::Char('K') => manage_move(app, -1),
                KeyCode::Char('J') => manage_move(app, 1),
                _ => {}
            }
        }
        InputMode::Normal | InputMode::Visual => return normal_key(app, k),
        InputMode::Search => match k.code {
            KeyCode::Esc => {
                app.mode = if app.vi.visual.is_some() { InputMode::Visual } else { InputMode::Normal };
                app.input.clear();
            }
            KeyCode::Enter => run_search(app),
            KeyCode::Backspace => {
                app.input.pop();
            }
            KeyCode::Char(c) => app.input.push(c),
            _ => {}
        },
    }
    Flow::Continue
}

/// Normal and Visual share every motion — the only difference is that Visual
/// keeps an anchor, so the selection grows as the cursor moves. this is also
/// where counts and the `g` prefix are resolved.
fn normal_key(app: &mut App, k: crossterm::event::KeyEvent) -> Flow {
    let last = app.channels.get(app.focus).map_or(0, |c| c.messages.len().saturating_sub(1));
    let page = app.vi.view.get().count.max(1);
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);

    // `g` is a prefix: gg, gt, gT. anything else after it cancels.
    if app.vi.g_pending {
        app.vi.g_pending = false;
        match k.code {
            KeyCode::Char('g') => app.vi.to_oldest(last),
            KeyCode::Char('t') => return tab_and_continue(app, 1),
            KeyCode::Char('T') => return tab_and_continue(app, -1),
            _ => return Flow::Continue,
        }
        app.vi.keep_visible();
        return Flow::Continue;
    }

    if let KeyCode::Char(c) = k.code {
        if !ctrl {
            if let Some(d) = c.to_digit(10) {
                if app.vi.push_digit(d) {
                    return Flow::Continue;
                }
            }
        }
    }

    if ctrl {
        match k.code {
            KeyCode::Char('d') => {
                let n = app.vi.take_count() * page.div_ceil(2);
                app.vi.down(n)
            }
            KeyCode::Char('u') => {
                let n = app.vi.take_count() * page.div_ceil(2);
                app.vi.up(n, last)
            }
            KeyCode::Char('f') => {
                let n = app.vi.take_count() * page;
                app.vi.down(n)
            }
            KeyCode::Char('b') => {
                let n = app.vi.take_count() * page;
                app.vi.up(n, last)
            }
            _ => return Flow::Continue,
        }
        app.vi.keep_visible();
        return Flow::Continue;
    }

    match k.code {
        KeyCode::Char('q') => return Flow::Quit,
        // motions — j is down the pane (newer), k is up it (older)
        KeyCode::Char('j') | KeyCode::Down => {
            let n = app.vi.take_count();
            app.vi.down(n)
        }
        KeyCode::Char('k') | KeyCode::Up => {
            let n = app.vi.take_count();
            app.vi.up(n, last)
        }
        KeyCode::Char('G') => app.vi.to_newest(),
        KeyCode::Char('g') => {
            app.vi.g_pending = true;
            return Flow::Continue;
        }
        // channel switching is h/l (and gt/gT) in every tab-bar orientation —
        // j/k are cursor motion, the way they are everywhere else in vi.
        KeyCode::Char('h') | KeyCode::Left | KeyCode::BackTab => return tab_and_continue(app, -1),
        KeyCode::Char('l') | KeyCode::Right | KeyCode::Tab => return tab_and_continue(app, 1),
        // visual selection
        KeyCode::Char('v') | KeyCode::Char('V') => {
            if app.vi.visual.is_some() {
                app.vi.visual = None;
                app.mode = InputMode::Normal;
            } else {
                app.vi.visual = Some(app.vi.cursor);
                app.mode = InputMode::Visual;
            }
            return Flow::Continue;
        }
        KeyCode::Char('y') => {
            yank(app);
            return Flow::Continue;
        }
        KeyCode::Char('/') => return search_prompt(app, vi::Dir::Fwd),
        KeyCode::Char('?') => return search_prompt(app, vi::Dir::Back),
        KeyCode::Char('n') => return repeat_search(app, false),
        KeyCode::Char('N') => return repeat_search(app, true),
        KeyCode::Esc => {
            // one escape drops the selection, the next returns to live chat.
            if app.vi.visual.is_some() || app.mode == InputMode::Visual {
                app.vi.visual = None;
                app.mode = InputMode::Normal;
            } else if !app.vi.following() {
                app.vi.reset();
            } else {
                app.status = None;
            }
            return Flow::Continue;
        }
        KeyCode::Char('i') | KeyCode::Char('a') => {
            app.mode = InputMode::Insert;
            app.status = None;
            return Flow::Continue;
        }
        KeyCode::Char('o') => {
            app.mode = InputMode::Join;
            app.input.clear();
            app.status = None;
            return Flow::Continue;
        }
        KeyCode::Char('x') => {
            close_channel(app);
            return Flow::Continue;
        }
        KeyCode::Char('m') => {
            app.mode = InputMode::Manage;
            app.manage_cursor = app.focus;
            return Flow::Continue;
        }
        KeyCode::Char('T') => {
            app.tab_pos = app.tab_pos.next();
            save_state(app);
            return Flow::Continue;
        }
        KeyCode::Char(' ') => {
            app.paused = !app.paused;
            return Flow::Continue;
        }
        _ => return Flow::Continue,
    }
    app.vi.keep_visible();
    Flow::Continue
}

fn tab_and_continue(app: &mut App, delta: i32) -> Flow {
    if delta > 0 {
        next_tab(app)
    } else {
        prev_tab(app)
    }
    Flow::Continue
}

/// open the `/` or `?` prompt.
fn search_prompt(app: &mut App, dir: vi::Dir) -> Flow {
    app.vi.dir = Some(dir);
    app.mode = InputMode::Search;
    app.input.clear();
    Flow::Continue
}

/// does this message match `pat`? case-insensitive over the sender and the text,
/// so `/nymn` finds both who said it and who was mentioned.
fn matches(m: &Message, pat: &str) -> bool {
    let pat = pat.to_lowercase();
    m.text.to_lowercase().contains(&pat) || m.user.to_lowercase().contains(&pat)
}

/// the message at index-back-from-newest `i` in the focused channel.
fn message_at(app: &App, i: usize) -> Option<&Message> {
    let ch = app.channels.get(app.focus)?;
    let n = ch.messages.len();
    (i < n).then(|| &ch.messages[n - 1 - i])
}

fn run_search(app: &mut App) {
    let pat = app.input.trim().to_string();
    app.input.clear();
    app.mode = if app.vi.visual.is_some() { InputMode::Visual } else { InputMode::Normal };
    if pat.is_empty() {
        return;
    }
    app.vi.pattern = pat;
    jump_to_match(app, app.vi.dir.unwrap_or(vi::Dir::Fwd));
}

fn repeat_search(app: &mut App, flip: bool) -> Flow {
    if app.vi.pattern.is_empty() {
        app.status = Some("no previous search".into());
        return Flow::Continue;
    }
    let dir = app.vi.dir.unwrap_or(vi::Dir::Fwd);
    jump_to_match(app, if flip { dir.flip() } else { dir });
    Flow::Continue
}

fn jump_to_match(app: &mut App, dir: vi::Dir) {
    let len = app.channels.get(app.focus).map_or(0, |c| c.messages.len());
    let pat = app.vi.pattern.clone();
    let found = vi::search(len, app.vi.cursor, dir, |i| {
        message_at(app, i).is_some_and(|m| matches(m, &pat))
    });
    match found {
        Some(i) => {
            app.vi.cursor = i;
            app.vi.keep_visible();
            app.status = None;
        }
        None => app.status = Some(format!("no match: {pat}")),
    }
}

/// copy the selection (or the cursor message) as plain `user: text` lines,
/// oldest first — the same shape `heatsync log` prints, so a yank pastes
/// straight into a grep or an issue.
fn yank(app: &mut App) {
    let (lo, hi) = app.vi.selection().unwrap_or((app.vi.cursor, app.vi.cursor));
    let mut lines: Vec<String> = Vec::new();
    for i in (lo..=hi).rev() {
        if let Some(m) = message_at(app, i) {
            lines.push(format!("{}: {}", m.user, m.text));
        }
    }
    if lines.is_empty() {
        app.status = Some("nothing to yank".into());
        return;
    }
    let n = lines.len();
    let body = lines.join("\n") + "\n";
    app.status = Some(match clip::copy(&body) {
        Ok(via) => format!("yanked {n} line{} → {via}", if n == 1 { "" } else { "s" }),
        Err(e) => format!("yank failed: {e}"),
    });
    app.vi.visual = None;
    app.mode = InputMode::Normal;
}

fn next_tab(app: &mut App) {
    if app.focus + 1 < app.channels.len() {
        app.focus += 1;
        app.vi.reset();
    }
}

fn prev_tab(app: &mut App) {
    if app.focus > 0 {
        app.focus -= 1;
        app.vi.reset();
    }
}

/// open a new channel tab from the Join input (`name` or `kick:name`). subscribes
/// over the live WS immediately; the emote set loads off-thread (never blocks).
fn join_channel(app: &mut App) {
    let tok = app.input.trim().to_string();
    app.mode = InputMode::Normal;
    app.input.clear();
    open_channel(app, &tok);
}

/// open (or focus, if already open) a channel by `name` / `kick:name`.
fn open_channel(app: &mut App, tok: &str) {
    if tok.is_empty() {
        return;
    }
    let (platform, name) = parse_sub(tok);
    if let Some(i) = app
        .channels
        .iter()
        .position(|c| c.platform == platform && c.name.eq_ignore_ascii_case(&name))
    {
        app.focus = i; // already open → just switch to it
        return;
    }
    if let Some(out) = &app.out {
        let _ = out.send(net::Outbound::Join { platform, channel: name.clone() });
    }
    app.channels.push(Channel::new(&name, platform, 200));
    app.emotes.push(EmoteSet::new()); // populated async — see spawn_emote_fetch
    spawn_emote_fetch(&app.emote_tx, platform, name);
    app.focus = app.channels.len() - 1;
    app.vi.reset();
    save_state(app);
}

/// leave the channel under the Manage cursor.
fn manage_delete(app: &mut App) {
    if app.channels.is_empty() {
        return;
    }
    let i = app.manage_cursor.min(app.channels.len() - 1);
    let (platform, name) = {
        let c = &app.channels[i];
        (c.platform, c.name.clone())
    };
    if let Some(out) = &app.out {
        let _ = out.send(net::Outbound::Part { platform, channel: name });
    }
    app.channels.remove(i);
    app.emotes.remove(i);
    let last = app.channels.len().saturating_sub(1);
    app.manage_cursor = i.min(last);
    app.focus = app.focus.min(last);
    save_state(app);
}

/// reorder the channel under the cursor by `delta` (-1 up, +1 down), keeping the
/// active channel and cursor pointed at the same items.
fn manage_move(app: &mut App, delta: isize) {
    let n = app.channels.len();
    if n < 2 {
        return;
    }
    let i = app.manage_cursor.min(n - 1);
    let j = i as isize + delta;
    if j < 0 || j >= n as isize {
        return;
    }
    let j = j as usize;
    app.channels.swap(i, j);
    app.emotes.swap(i, j);
    if app.focus == i {
        app.focus = j;
    } else if app.focus == j {
        app.focus = i;
    }
    app.manage_cursor = j;
    save_state(app);
}

/// leave the active channel tab.
/// leave a channel by name, or the focused one when `which` is None.
fn part_channel(app: &mut App, which: Option<&str>) {
    if let Some(tok) = which {
        let (platform, name) = parse_sub(tok);
        match app
            .channels
            .iter()
            .position(|c| c.platform == platform && c.name.eq_ignore_ascii_case(&name))
        {
            Some(i) => app.focus = i,
            None => {
                app.status = Some(format!("not open: {name}"));
                return;
            }
        }
    }
    close_channel(app);
}

fn close_channel(app: &mut App) {
    if app.channels.is_empty() {
        return;
    }
    let (platform, name) = {
        let c = &app.channels[app.focus];
        (c.platform, c.name.clone())
    };
    if let Some(out) = &app.out {
        let _ = out.send(net::Outbound::Part { platform, channel: name });
    }
    app.channels.remove(app.focus);
    app.emotes.remove(app.focus);
    app.focus = app.focus.min(app.channels.len().saturating_sub(1));
    app.vi.reset();
    save_state(app);
}

/// send the current input line to the focused channel (kick only; twitch has no
/// send path). clears the input on a successful enqueue.
/// enter in the composer. a leading `/` is a command — ours are handled here,
/// everything else (mod actions, /me) goes to the platform verbatim.
fn send_focused(app: &mut App) -> Flow {
    let text = match slash::parse(&app.input) {
        slash::Cmd::Join(ch) => {
            app.input.clear();
            open_channel(app, &ch);
            return Flow::Continue;
        }
        slash::Cmd::Part(which) => {
            app.input.clear();
            part_channel(app, which.as_deref());
            return Flow::Continue;
        }
        slash::Cmd::Quit => return Flow::Quit,
        slash::Cmd::Usage(u) => {
            app.status = Some(u.into());
            return Flow::Continue;
        }
        slash::Cmd::Send(t) => t,
    };
    if text.is_empty() || app.channels.is_empty() {
        return Flow::Continue;
    }
    let (platform, name) = {
        let c = &app.channels[app.focus];
        (c.platform, c.name.clone())
    };
    match platform {
        Platform::Twitch => match &app.twitch_tx {
            Some(tx) => {
                let _ = tx.send((name.clone(), text));
                app.input.clear();
                app.status = Some(format!("sent → {name}"));
            }
            None => app.status = Some("no twitch token — run: heatsync login".into()),
        },
        // prefer direct kick send (own token); fall back to the ext-relay path.
        Platform::Kick => {
            if let Some(ktx) = &app.kick_tx {
                let _ = ktx.send((name.clone(), text));
                app.input.clear();
                app.status = Some(format!("sent → {name}"));
            } else if let Some(out) = &app.out {
                let _ = out.send(net::Outbound::Chat {
                    platform: Platform::Kick,
                    channel: name.clone(),
                    text,
                });
                app.input.clear();
                app.status = Some(format!("sent → {name} (via ext)"));
            } else {
                app.status = Some("no kick auth — run: heatsync login kick".into());
            }
        }
    }
    Flow::Continue
}

/// pull new data into the channel buffers for this tick, and keep the emote
/// image cache warm for what's on screen.
fn advance(app: &mut App) -> bool {
    let focus = app.focus;
    let App { channels, emotes, feed, store, fb, status, vi, .. } = app;
    // how many messages landed in the channel being read. the vi layer needs it
    // to hold scrollback still — indices count back from the newest message, so
    // every arrival shifts them and the cursor would otherwise slide off the
    // message it was parked on.
    let mut added = 0usize;
    match feed {
        Feed::Mock(driver) => {
            let before = channels.get(focus).map_or(0, |c| c.messages.len());
            driver.tick(channels);
            added = channels.get(focus).map_or(0, |c| c.messages.len()).saturating_sub(before);
        }
        Feed::Live { rx, start, connected } => {
            let now = start.elapsed().as_millis() as u64;
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    ChatEvent::Line(l) => {
                        if let Some(i) = channels.iter().position(|c| {
                            c.platform == l.platform && c.name.eq_ignore_ascii_case(&l.channel)
                        }) {
                            channels[i].record(
                                Message { user: l.user, text: l.content, color: l.color, heat: 0.0 },
                                now,
                            );
                            if i == focus {
                                added += 1;
                            }
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
    let last = channels.get(focus).map_or(0, |c| c.messages.len().saturating_sub(1));
    vi.absorb_new(added, last);
    vi.clamp(last);

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
                each_stack(&m.text, set, |key| f(key));
            }
        }
    };
    if let Some(store) = store {
        want_urls(&mut |u| store.request(u));
        store.pump()
    } else if let Some(fb) = fb {
        want_urls(&mut |u| fb.request(u));
        fb.pump();
        false
    } else {
        false
    }
}

/// width of the tab column when the bar is vertical (left/right).
const TAB_COL_W: u16 = 16;

fn emote_mode(app: &App) -> EmoteMode<'_> {
    if let Some(s) = &app.store {
        EmoteMode::Term(s)
    } else if let Some(f) = &app.fb {
        EmoteMode::Fb(f)
    } else {
        EmoteMode::Text
    }
}

fn ui(f: &mut Frame, app: &App) {
    let mode = emote_mode(app);
    let [main, footer] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(f.area());

    if app.mode == InputMode::Manage {
        draw_manage(f, main, app);
        draw_footer(f, footer, app, app.channels.len());
        return;
    }

    if app.channels.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("  no channels — press ", Style::default().fg(Color::Indexed(244))),
                Span::styled("o", Style::default().fg(BRAND).add_modifier(Modifier::BOLD)),
                Span::styled(" to join one", Style::default().fg(Color::Indexed(244))),
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
            let a = Layout::horizontal([Constraint::Length(TAB_COL_W), Constraint::Min(0)]).split(main);
            (a[0], a[1])
        }
        TabPos::Right => {
            let a = Layout::horizontal([Constraint::Min(0), Constraint::Length(TAB_COL_W)]).split(main);
            (a[1], a[0])
        }
    };

    draw_tabs(f, tabs, app);
    draw_active(f, chat, app, mode);
    draw_footer(f, footer, app, app.channels.len());
}

/// the channel tab bar — horizontal row (top/bottom) or vertical list (left/right).
fn draw_tabs(f: &mut Frame, area: Rect, app: &App) {
    let tab_style = |i: usize, heat: f64| {
        if i == app.focus {
            Style::default().fg(Color::Black).bg(BRAND).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Indexed(Tier::of(heat).xterm()))
        }
    };

    if app.tab_pos.is_vertical() {
        for (i, ch) in app.channels.iter().enumerate() {
            if i as u16 >= area.height {
                break;
            }
            let label = fit(format!(" {} {:.0} ", ch.name, ch.heat), area.width);
            let row = Rect { x: area.x, y: area.y + i as u16, width: area.width, height: 1 };
            f.render_widget(Paragraph::new(Line::from(Span::styled(label, tab_style(i, ch.heat)))), row);
        }
    } else {
        let mut spans = Vec::new();
        for (i, ch) in app.channels.iter().enumerate() {
            spans.push(Span::styled(
                format!(" {}·{} {:.0} ", ch.name, ch.platform.tag(), ch.heat),
                tab_style(i, ch.heat),
            ));
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
    let [barrow, body] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    f.render_widget(Paragraph::new(heat_bar(ch.heat, barrow.width as usize)), barrow);
    if body.height == 0 || body.width == 0 {
        return;
    }

    let cap = body.height as usize;
    // plan visible messages newest-first, honouring per-message height + scroll.
    // the index travels with each one so the cursor and selection can be matched
    // without re-deriving positions.
    let mut plan: Vec<(&Message, usize, usize)> = Vec::new();
    let mut used = 0usize;
    for (i, m) in ch.messages.iter().rev().skip(app.vi.scroll).enumerate() {
        let h = if has_emote(m, set, mode) { EMOTE_H as usize } else { 1 };
        if used + h > cap {
            break;
        }
        used += h;
        plan.push((m, h, app.vi.scroll + i));
    }
    // hand the row count back to the vi layer — paging and keeping the cursor on
    // screen need it, and only this loop knows how many variable-height messages
    // actually fit.
    app.vi.view.set(vi::View { count: plan.len() });
    plan.reverse();

    // the cursor bar stays hidden while chat is simply following live, so the
    // pane reads clean until you actually start navigating.
    let show_cursor = !app.vi.following();

    let mut y = body.y + (cap - used) as u16; // bottom-anchor
    for (m, h, idx) in plan {
        let text_row = y + (h as u16 - 1);
        let (mut line, places) = layout_message(m, set, mode, body.width);
        // black-on-white for the cursor row and every selected row — the same
        // treatment hover and active get everywhere else in the product. the
        // Paragraph style carries the bar across the unwritten cells too, so it
        // reads as one solid row rather than stopping at the last character.
        let marked = app.vi.selected(idx) || (show_cursor && idx == app.vi.cursor);
        let para = if marked {
            let hl = Style::default().bg(Color::White).fg(Color::Black);
            // patch every span, not just the paragraph — the per-user colours
            // and heat hues would otherwise stay bright on a white bar.
            for span in line.spans.iter_mut() {
                span.style = span.style.patch(hl);
            }
            Paragraph::new(line).style(hl)
        } else {
            Paragraph::new(line)
        };
        f.render_widget(para, Rect { x: body.x, y: text_row, width: body.width, height: 1 });
        for p in &places {
            let x = body.x + p.col;
            match mode {
                EmoteMode::Term(store) => {
                    if let Some(proto) = store.frame(&p.key) {
                        f.render_widget(Image::new(proto), Rect { x, y, width: p.w, height: h as u16 });
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
                    let pad = "\u{a0}".repeat(p.w as usize);
                    let lines: Vec<Line> = (0..h).map(|_| Line::raw(pad.clone())).collect();
                    f.render_widget(
                        Paragraph::new(lines),
                        Rect { x, y, width: p.w, height: h as u16 },
                    );
                    fb.push(x, y, &p.key);
                }
                EmoteMode::Text => {}
            }
        }
        y += h as u16;
    }
}

/// rover-style channel manager: a single-pane list, cursor row highlighted, the
/// active channel marked. reorder/leave/open from here.
fn draw_manage(f: &mut Frame, area: Rect, app: &App) {
    let [head, list] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" channels ", Style::default().fg(Color::Black).bg(BRAND).add_modifier(Modifier::BOLD)),
            Span::styled(format!("  {} open", app.channels.len()), Style::default().fg(Color::Indexed(244))),
        ])),
        head,
    );

    if app.channels.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  empty — press a to add a channel",
                Style::default().fg(Color::Indexed(244)),
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
        let hue = Color::Indexed(Tier::of(ch.heat).xterm());
        let style = if sel {
            Style::default().fg(Color::Black).bg(BRAND).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(hue)
        };
        let cursor = if sel { "❯" } else { " " };
        let active = if i == app.focus { "●" } else { " " };
        let label = format!(
            " {cursor} {active} {}·{}   {:>6.0}   {} msg",
            ch.name,
            ch.platform.tag(),
            ch.heat,
            ch.messages.len(),
        );
        let row = Rect { x: list.x, y: list.y + i as u16, width: list.width, height: 1 };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(fit(label, list.width), style))),
            row,
        );
    }
}

/// walk a message's emote STACKS in order. a stack is a base emote plus any
/// immediately-following zero-width (overlay) emotes; its key is the layer urls
/// joined by '\n'. text breaks a stack. shared by layout, prefetch, and height.
fn each_stack(text: &str, set: &EmoteSet, mut f: impl FnMut(&str)) {
    let mut urls: Vec<String> = Vec::new();
    let flush = |urls: &mut Vec<String>, f: &mut dyn FnMut(&str)| {
        if !urls.is_empty() {
            f(&urls.join("\n"));
            urls.clear();
        }
    };
    for tok in tokenize(text, set) {
        match tok {
            Token::Emote(name) => {
                if let Some(e) = set.get(&name) {
                    if e.zero_width && !urls.is_empty() {
                        urls.push(e.url.clone()); // overlay onto the current base
                    } else {
                        flush(&mut urls, &mut f);
                        urls.push(e.url.clone()); // new base
                    }
                }
            }
            Token::Text(_) => flush(&mut urls, &mut f),
        }
    }
    flush(&mut urls, &mut f);
}

/// does this message contain at least one loaded (ready-to-draw) emote stack?
/// does this message contain an emote we will draw as an image? true as soon as
/// the NAME resolves in the set — before the image has finished loading — so the
/// row is already EMOTE_H tall when the picture arrives. reserving the space up
/// front is what stops the whole chat from re-laying-out (and every sixel below
/// from being re-emitted) each time an emote lands.
fn has_emote(m: &Message, set: &EmoteSet, mode: EmoteMode) -> bool {
    if mode.square_cells().is_none() {
        return false; // text tier: emote names are just words
    }
    let mut any = false;
    each_stack(&m.text, set, |_| any = true);
    any
}

/// truncate a label to `w` display columns (approximate; ASCII-dominant labels).
fn fit(s: String, w: u16) -> String {
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
    let mut out = String::new();
    let mut used = 0usize;
    for c in s.chars() {
        let cw = UnicodeWidthStr::width(c.to_string().as_str());
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
struct Place {
    col: u16,
    /// this emote's own width in cells — square emotes and wide emotes get
    /// different footprints instead of one fixed block.
    w: u16,
    key: String,
}

/// lay a message onto one row: the text line (with blank cells where ready emote
/// stacks go) plus the reserved emote slots. a base emote absorbs any following
/// zero-width (overlay) emotes into one stack. an unloaded/text-mode emote falls
/// back to its name in brand color.
fn layout_message(m: &Message, set: &EmoteSet, mode: EmoteMode, maxw: u16) -> (Line<'static>, Vec<Place>) {
    let text_hue = Color::Indexed(heatsync_core::heat::color(m.heat));
    let user_color = m.color.as_deref().and_then(parse_hex).unwrap_or(Color::Indexed(244));
    let mut spans = vec![
        Span::styled(m.user.clone(), Style::default().fg(user_color)),
        Span::styled(": ", Style::default().fg(Color::Indexed(244))),
    ];
    let mut col = UnicodeWidthStr::width(m.user.as_str()) as u16 + 2;
    let mut places = Vec::new();

    let toks = tokenize(&m.text, set);
    let mut i = 0;
    while i < toks.len() {
        if col >= maxw {
            break;
        }
        match &toks[i] {
            Token::Text(t) => {
                col += UnicodeWidthStr::width(t.as_str()) as u16;
                spans.push(Span::styled(t.clone(), Style::default().fg(text_hue)));
                i += 1;
            }
            Token::Emote(name) => {
                // gather the base emote + any trailing zero-width overlays.
                let base_name = name.clone();
                let mut urls: Vec<String> =
                    set.get(name).map(|e| e.url.clone()).into_iter().collect();
                i += 1;
                while let Some(Token::Emote(n2)) = toks.get(i) {
                    match set.get(n2) {
                        Some(e2) if e2.zero_width => {
                            urls.push(e2.url.clone());
                            i += 1;
                        }
                        _ => break,
                    }
                }
                let key = urls.join("\n");
                // an emote that has finished loading knows its own width; one
                // still loading is laid out at the provisional square footprint,
                // so the line does not shift when its image arrives.
                let w = match mode.cells(&key) {
                    Some(w) => Some((w, true)),
                    None => mode.square_cells().map(|w| (w, false)),
                }
                .filter(|(w, _)| col + w <= maxw && !urls.is_empty());
                if let Some((w, ready)) = w {
                    if ready {
                        places.push(Place { col, w, key });
                        spans.push(Span::raw(" ".repeat(w as usize)));
                    } else {
                        // still loading: hold the exact same footprint and show
                        // as much of the name as fits, so the image swaps in
                        // place instead of shoving the line around.
                        spans.push(Span::styled(
                            fit_exact(&base_name, w),
                            Style::default().fg(BRAND),
                        ));
                    }
                    col += w;
                } else {
                    col += UnicodeWidthStr::width(base_name.as_str()) as u16;
                    spans.push(Span::styled(base_name, Style::default().fg(BRAND).add_modifier(Modifier::BOLD)));
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
    // Manage mode → rover-style key hints.
    if app.mode == InputMode::Manage {
        let hint = |k: &'static str, d: &'static str| {
            [Span::styled(k, Style::default().fg(BRAND)), Span::styled(format!(" {d}  "), Style::default().fg(Color::Indexed(244)))]
        };
        let mut spans = vec![Span::styled(" manage ", Style::default().fg(Color::Black).bg(BRAND).add_modifier(Modifier::BOLD)), Span::raw("  ")];
        for pair in [hint("jk", "move"), hint("enter", "open"), hint("a", "add"), hint("d", "leave"), hint("JK", "reorder"), hint("esc", "back")] {
            spans.extend(pair);
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }
    // Join mode → type a channel to open.
    if app.mode == InputMode::Join {
        let spans = vec![
            Span::styled(" join ", Style::default().fg(Color::Black).bg(BRAND).add_modifier(Modifier::BOLD)),
            Span::styled(" ❯ ", Style::default().fg(BRAND)),
            Span::styled(app.input.clone(), Style::default().fg(Color::Indexed(231))),
            Span::styled("\u{2588}", Style::default().fg(BRAND)),
            Span::styled("   name or kick:name", Style::default().fg(Color::Indexed(244))),
        ];
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }
    // Insert mode → the message composer for the focused channel.
    if app.mode == InputMode::Insert && !app.channels.is_empty() {
        let ch = &app.channels[app.focus];
        // read-only only when there's genuinely no send path for this platform:
        // twitch needs the user's own token; kick can also relay via the ws.
        let readonly = match ch.platform {
            Platform::Twitch => app.twitch_tx.is_none(),
            Platform::Kick => app.kick_tx.is_none() && app.out.is_none(),
        };
        let prompt = format!(" {}·{} ", ch.name, ch.platform.tag());
        let mut spans = vec![
            Span::styled(prompt, Style::default().fg(Color::Black).bg(BRAND).add_modifier(Modifier::BOLD)),
            Span::styled(" ❯ ", Style::default().fg(BRAND)),
        ];
        if readonly {
            spans.push(Span::styled("read-only — no send token · esc", Style::default().fg(Color::Indexed(214))));
        } else {
            spans.push(Span::styled(app.input.clone(), Style::default().fg(Color::Indexed(231))));
            spans.push(Span::styled("\u{2588}", Style::default().fg(BRAND))); // cursor
            // a command's reply (usage, "not open: x") has to be visible from
            // the composer — that is where the command was typed.
            if let Some(msg) = &app.status {
                spans.push(Span::styled(format!("   {msg}"), Style::default().fg(Color::Indexed(214))));
            } else if app.input.is_empty() {
                spans.push(Span::styled(
                    "   /join <chan>  /part  /quit  — anything else goes to chat",
                    Style::default().fg(Color::Indexed(244)),
                ));
            }
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }

    // Search mode → the `/` or `?` prompt, vi-style at the bottom.
    if app.mode == InputMode::Search {
        let lead = if app.vi.dir == Some(vi::Dir::Back) { "?" } else { "/" };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!(" {lead} "), Style::default().fg(Color::Black).bg(BRAND).add_modifier(Modifier::BOLD)),
                Span::styled(format!(" {}", app.input), Style::default().fg(Color::Indexed(231))),
                Span::styled("\u{2588}", Style::default().fg(BRAND)),
            ])),
            area,
        );
        return;
    }
    // Visual mode → selection size + what you can do with it.
    if app.mode == InputMode::Visual {
        let n = app.vi.selection().map_or(1, |(lo, hi)| hi - lo + 1);
        let hint = |k: &'static str, d: &'static str| {
            [Span::styled(k, Style::default().fg(BRAND)), Span::styled(format!(" {d}  "), Style::default().fg(Color::Indexed(244)))]
        };
        let mut spans = vec![
            Span::styled(" visual ", Style::default().fg(Color::Black).bg(BRAND).add_modifier(Modifier::BOLD)),
            Span::styled(format!("  {n} line{}  ", if n == 1 { "" } else { "s" }), Style::default().fg(Color::Indexed(231))),
        ];
        for pair in [hint("jk", "extend"), hint("y", "yank"), hint("/", "search"), hint("esc", "cancel")] {
            spans.extend(pair);
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
    ];
    // a message (yank confirmation, search miss, send error) takes the front of
    // the line the way vi's command line does — appended after the key hints it
    // was simply truncated away on anything narrower than ~110 columns.
    if let Some(msg) = &app.status {
        spans.push(Span::styled(format!("  {msg}"), Style::default().fg(Color::Indexed(214))));
    }
    spans.extend([
        Span::styled("  hl ", Style::default().fg(BRAND)),
        Span::raw("chan  "),
        Span::styled("jk ", Style::default().fg(BRAND)),
        Span::raw("move  "),
        Span::styled("v ", Style::default().fg(BRAND)),
        Span::raw("select  "),
        Span::styled("y ", Style::default().fg(BRAND)),
        Span::raw("yank  "),
        Span::styled("/ ", Style::default().fg(BRAND)),
        Span::raw("find  "),
        Span::styled("i ", Style::default().fg(BRAND)),
        Span::raw("say  "),
        Span::styled("o ", Style::default().fg(BRAND)),
        Span::raw("join  "),
        Span::styled("m ", Style::default().fg(BRAND)),
        Span::raw("manage  "),
        Span::styled("q ", Style::default().fg(BRAND)),
        Span::raw("quit  "),
    ]);
    // a pending count or `g` echoes at the bottom, the way vi shows what it is
    // still waiting on.
    let pending = match (app.vi.count, app.vi.g_pending) {
        (0, false) => String::new(),
        (0, true) => "g".into(),
        (n, g) => format!("{n}{}", if g { "g" } else { "" }),
    };
    if !pending.is_empty() {
        spans.push(Span::styled(format!("{pending}  "), Style::default().fg(Color::Indexed(231)).add_modifier(Modifier::BOLD)));
    }
    if !app.vi.following() {
        spans.push(Span::styled("SCROLLBACK  ", Style::default().fg(Color::Indexed(214)).add_modifier(Modifier::BOLD)));
    }
    if app.paused {
        spans.push(Span::styled("PAUSED  ", Style::default().fg(Color::Indexed(214)).add_modifier(Modifier::BOLD)));
    }
    spans.push(Span::styled(dot, Style::default().fg(dot_color)));
    spans.push(Span::styled(format!("{state} · {n} ch"), Style::default().fg(Color::Indexed(244))));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
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
        }
    }

    #[test]
    fn overlay_groups_onto_base() {
        let set = EmoteSet::from_list([em("GAMBA", false), em("notL", true)]);
        let mut keys = Vec::new();
        each_stack("GAMBA notL", &set, |k| keys.push(k.to_string()));
        assert_eq!(keys, vec!["u/GAMBA\nu/notL"]); // one stack, base + overlay
    }

    #[test]
    fn text_breaks_stack() {
        let set = EmoteSet::from_list([em("GAMBA", false), em("KEKW", false)]);
        let mut keys = Vec::new();
        each_stack("GAMBA lol KEKW", &set, |k| keys.push(k.to_string()));
        assert_eq!(keys, vec!["u/GAMBA", "u/KEKW"]); // separate, text between
    }

    #[test]
    fn multiple_overlays_stack() {
        let set = EmoteSet::from_list([em("A", false), em("h1", true), em("h2", true)]);
        let mut keys = Vec::new();
        each_stack("A h1 h2", &set, |k| keys.push(k.to_string()));
        assert_eq!(keys, vec!["u/A\nu/h1\nu/h2"]);
    }
}
