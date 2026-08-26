//! heatsync tui — heat-sorted live multichat. `heatsync [channels…]` opens the
//! grid (default demo set); `--mock` runs the offline synthetic feed; `log`,
//! `search`, `hot` are headless corpus subcommands.
//! keys: j/k and h/l (or arrows, tab/shift-tab) change channel, space pauses,
//! i composes, o joins, m manages, q quits. chat always follows live — there
//! is no message cursor and no keyboard nav over messages.

#[allow(dead_code)]
mod cli;
mod config;
mod emote;
mod http;
mod key;
mod kick;
mod net;
mod twitch;

// The editing model lives in core now, so a gui can share it. Imported under
// the same names the call sites below already use.
use heatsync_core::{edit, slash};

use std::io::{self, IsTerminal};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use config::TabPos;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use emote::fb::FbEmotes;
use emote::render::{EmoteStore, EMOTE_H};
use heatsync_core::complete::Completion;
use heatsync_core::emote::{segments, EmoteSet, Segment};
use heatsync_core::heat::Tier;
use heatsync_core::{mock, Badge, Channel, Message, Platform};
use net::{ChatEvent, Sub};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal};
use ratatui_image::Image;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// chrome accent: white — active/selected is black-on-white, hint keys are
/// bright white. color in the ui comes only from semantics (heat tiers, user
/// colors, live/warn dots), never decoration.
const ACCENT: Color = Color::Indexed(231);

/// feed source: offline synthetic, or the live relay thread.
enum Feed {
    Mock(mock::Driver),
    Live {
        rx: Receiver<ChatEvent>,
        start: Instant,
        connected: bool,
    },
}

/// modes: switch channels in Normal, type a message in Insert, type a channel
/// to join in Join, or manage channels rover-style in Manage. SlotPick/SlotEdit
/// are Manage sub-modes: pick one of the t/k/y source slots, then retype it.
#[derive(Clone, Copy, PartialEq)]
enum InputMode {
    Normal,
    Insert,
    Join,
    Manage,
    /// e pressed in Manage — waiting for t/k/y to choose which slot to edit.
    SlotPick,
    /// typing a new name for one platform slot of the row under the cursor.
    SlotEdit(Platform),
}

struct App {
    channels: Vec<Channel>,
    emotes: Vec<EmoteSet>, // index-aligned with channels
    /// the pooled global set (twitch + unlocked + bttv/ffz/7tv globals),
    /// merged under every channel set — channel emotes of the same name win.
    globals: EmoteSet,
    focus: usize,
    paused: bool,
    feed: Feed,
    store: Option<EmoteStore>, // terminal graphics tier (sixel/kitty/…)
    fb: Option<FbEmotes>,      // bare-console framebuffer tier (TERM=linux)
    mode: InputMode,
    input: String,
    /// the composer: a vi line editor with its own insert/normal modes.
    line: edit::Line,
    status: Option<String>, // transient one-line notice (send errors, etc.)
    out: Option<net::Tx>,   // outbound channel to the live WS thread
    twitch_tx: Option<std::sync::mpsc::Sender<twitch::Send>>, // direct twitch sender
    kick_tx: Option<std::sync::mpsc::Sender<kick::Send>>, // direct kick sender
    tab_pos: TabPos,
    manage_cursor: usize, // cursor in the Manage view
    /// a tab-completion walk in progress; any non-tab key ends it.
    completion: Option<Completion>,
    /// own username (twitch login), for mention highlighting.
    me: Option<String>,
    // emote sets are fetched off-thread (a blocking HTTP call would freeze the
    // UI); results arrive here keyed by (platform, name) and merge in.
    emote_tx: Sender<(Platform, String, EmoteSet)>,
    emote_rx: Receiver<(Platform, String, EmoteSet)>,
    // archived scrollback arrives the same way: fetched per channel at join,
    // merged in behind whatever live lines have already landed.
    hist_tx: Sender<(Platform, String, Vec<Message>)>,
    hist_rx: Receiver<(Platform, String, Vec<Message>)>,
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

/// Printed by `--help`, and again when the TUI is asked to start without a tty.
const USAGE: &str = "\
heatsync-tui — heat-sorted live multichat in the terminal

USAGE:
    heatsync-tui [CHANNEL...]        open the chat UI on those channels
                                     (name, kick:name, yt:videoid or a youtube url)
    heatsync-tui <SUBCOMMAND>

SUBCOMMANDS:
    log        print recent messages
    search     search the chat archive
    hot, top   busiest channels right now
    status     connection + auth status
    login      link a twitch account
    login kick link a kick account
    probe      check a channel is reachable
    diag       dump diagnostics
    render-test  draw the emote/paint test pattern

OPTIONS:
    --mock     run against a synthetic feed, no network
    -h, --help
    -V, --version
";

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("log") => return cli::log(&args[1..]),
        Some("search") => return cli::search(&args[1..]),
        Some("hot") | Some("top") => return cli::hot(&args[1..]),
        Some("probe") => return cli::probe(&args[1..]),
        Some("diag") => return cli::diag(&args[1..]),
        Some("render-test") => return cli::render_test(&args[1..]),
        Some("status") => return cli::status(),
        Some("login") if args.get(1).map(String::as_str) == Some("kick") => return kick::login(),
        Some("login") => return cli::login(),
        // The two flags every installed binary is asked first. Without them
        // `heatsync-tui --version` fell through to the TUI, tried to connect to
        // three channels, and then panicked out of ratatui because there was no
        // terminal — which is what a fresh `cargo install` looks like when you
        // check what you just installed.
        Some("--version") | Some("-V") => {
            println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("--help") | Some("-h") | Some("help") => {
            print!("{USAGE}");
            return Ok(());
        }
        _ => {}
    }

    // Starting the TUI needs a terminal. Say so plainly instead of panicking
    // out of ratatui's init with `Os { code: 6 }`, which is what a pipe, a cron
    // job or a CI step would otherwise get.
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        eprintln!(
            "heatsync-tui: not a terminal — the chat UI needs an interactive tty.\n\
             Run it directly, or use a subcommand that writes to stdout:\n\
             \n\
             {USAGE}"
        );
        std::process::exit(1);
    }

    let mock_mode = args.iter().any(|a| a == "--mock");
    let chan_args: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    let cfg = config::load();

    let app = if mock_mode {
        let (emote_tx, emote_rx) = std::sync::mpsc::channel();
        let (hist_tx, hist_rx) = std::sync::mpsc::channel();
        App {
            channels: mock::channels(),
            emotes: (0..mock::channels().len())
                .map(|_| EmoteSet::new())
                .collect(),
            globals: EmoteSet::new(),
            focus: 0,
            paused: false,
            feed: Feed::Mock(mock::Driver::new()),
            store: None,
            fb: None,
            mode: InputMode::Normal,
            input: String::new(),
            line: edit::Line::default(),
            status: None,
            out: None,
            twitch_tx: None,
            kick_tx: None,
            tab_pos: cfg.tab_pos,
            manage_cursor: 0,
            completion: None,
            me: None,
            emote_tx,
            emote_rx,
            hist_tx,
            hist_rx,
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

/// parse channel args (`name`, `twitch:name`, `kick:name`, `yt:id`, and
/// `a+kick:a+yt:x` merged tabs), fetch emote sets, and start the live feed.
/// prints a one-line loading note before raw mode.
fn build_live(chan_args: &[&String], tab_pos: TabPos, saved: Vec<Vec<Sub>>) -> App {
    // precedence: explicit CLI args → persisted tabs → empty (a clean "press o to
    // join" state; no hardcoded demo channels). first run starts blank; whatever
    // you open persists, so the next launch restores it.
    let tabs: Vec<Vec<Sub>> = if !chan_args.is_empty() {
        chan_args.iter().map(|a| parse_tab(a)).collect()
    } else {
        saved
    };
    let tabs: Vec<Vec<Sub>> = tabs.into_iter().filter(|t| !t.is_empty()).collect();

    eprintln!(
        "heatsync: fetching emotes + connecting to {} channels…",
        tabs.len()
    );
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
    let (hist_tx, hist_rx) = std::sync::mpsc::channel();
    let mut channels = Vec::new();
    let mut emotes = Vec::new();
    for tab in &tabs {
        let (platform, name) = &tab[0];
        let mut ch = Channel::new(name, *platform, 200);
        ch.extra = tab[1..].to_vec();
        channels.push(ch);
        emotes.push(EmoteSet::new());
        for (p, n) in tab {
            // youtube has no emote API, and its archive is keyed by broadcaster
            // name (we only know the video id) — both fetches would be dead calls.
            if *p != Platform::Youtube {
                spawn_emote_fetch(&emote_tx, *p, n.clone());
                spawn_history(&hist_tx, *p, n.clone());
            }
        }
    }

    let token = std::env::var("HEATSYNC_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());
    let subs: Vec<Sub> = tabs.into_iter().flatten().collect();
    let (rx, out) = net::spawn(subs, token);
    // direct-to-platform sending if the user supplied their own platform tokens.
    let auth = config::load_auth();
    let me = auth.twitch_user.clone();
    let kick_tx = auth.kick_token.clone().map(kick::spawn);
    let twitch_tx = match (auth.twitch_user, auth.twitch_oauth) {
        (Some(u), Some(o)) => Some(twitch::spawn(u, o)),
        _ => None,
    };
    spawn_global_emotes(&emote_tx);
    App {
        channels,
        emotes,
        globals: EmoteSet::new(),
        focus: 0,
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
        line: edit::Line::default(),
        status: None,
        out: Some(out),
        twitch_tx,
        kick_tx,
        tab_pos,
        manage_cursor: 0,
        completion: None,
        me,
        emote_tx,
        emote_rx,
        hist_tx,
        hist_rx,
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

/// fetch the pooled global emote set once at startup, delivered over the same
/// channel as per-channel sets under an empty name (no real sub is ever "").
fn spawn_global_emotes(tx: &Sender<(Platform, String, EmoteSet)>) {
    let tx = tx.clone();
    std::thread::spawn(move || {
        let set = http::global_emotes().unwrap_or_default();
        if !set.is_empty() {
            let _ = tx.send((Platform::Twitch, String::new(), set));
        }
    });
}

/// fetch a channel's recent archive tail on a detached thread — scrollback on
/// open instead of an empty pane waiting for the feed to warm up.
fn spawn_history(tx: &Sender<(Platform, String, Vec<Message>)>, platform: Platform, name: String) {
    let tx = tx.clone();
    std::thread::spawn(move || {
        let msgs = http::recent(&name, platform, 60);
        if !msgs.is_empty() {
            let _ = tx.send((platform, name, msgs));
        }
    });
}

/// merge fetched history behind whatever live lines already landed, and start
/// image loads for the emotes it references.
fn drain_history(app: &mut App) {
    let App {
        channels,
        emotes,
        store,
        fb,
        hist_rx,
        focus,
        ..
    } = app;
    while let Ok((platform, name, msgs)) = hist_rx.try_recv() {
        if let Some(i) = channels.iter().position(|c| c.matches(platform, &name)) {
            channels[i].backfill(msgs);
            // focused channel only — background history warms up via the tick
            // sweep when it's actually looked at.
            if i == *focus {
                let from = channels[i].messages.len().saturating_sub(60);
                for m in channels[i].messages.iter().skip(from) {
                    request_stacks(store, fb, &emotes[i], &m.text);
                }
            }
        }
    }
}

/// merge any emote sets that finished fetching into their channel columns, and
/// prefetch images for the backlog those sets just made resolvable — messages
/// that arrived before the set landed were scanned against an empty set.
fn drain_emotes(app: &mut App) {
    let App {
        channels,
        emotes,
        globals,
        store,
        fb,
        emote_rx,
        focus,
        ..
    } = app;
    let mut dirty = false;
    while let Ok((platform, name, set)) = emote_rx.try_recv() {
        // the empty name is the global set (see spawn_global_emotes): keep it
        // for tabs opened later, and pool it into every open tab now.
        if name.is_empty() {
            *globals = set;
            for es in emotes.iter_mut() {
                es.merge(globals.clone());
            }
            dirty = true;
            continue;
        }
        if let Some(i) = channels.iter().position(|c| c.matches(platform, &name)) {
            // merge — a merged tab pools its sources' sets; channel emotes
            // replace any global of the same name whichever landed first.
            emotes[i].merge(set);
            // rescan only the channel on screen — a background channel's
            // backlog is warmed by the tick sweep if and when it's focused.
            if i == *focus {
                dirty = true;
            }
        }
    }
    // one rescan of the visible backlog no matter how many sets landed.
    if dirty && !channels.is_empty() {
        let i = (*focus).min(channels.len() - 1);
        let from = channels[i].messages.len().saturating_sub(60);
        for m in channels[i].messages.iter().skip(from) {
            request_stacks(store, fb, &emotes[i], &m.text);
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
        .map(|c| c.subs().map(|(p, n)| (p, n.to_string())).collect())
        .collect();
    config::save(&config::Config {
        tab_pos: app.tab_pos,
        channels,
    });
}

/// one tab spec: `+`-joined subs merge into a single interleaved tab
/// (`nl_kripp+kick:nl_kripp+yt:VIDEOID`). whitespace separates too — channel
/// names never contain spaces, so `a kick:a` typed in the join prompt means
/// the same merge.
fn parse_tab(tok: &str) -> Vec<Sub> {
    let mut subs: Vec<Sub> = Vec::new();
    for part in tok
        .split(|c: char| c == '+' || c.is_whitespace())
        .filter(|s| !s.trim().is_empty())
    {
        let s = parse_sub(part.trim());
        if !s.1.is_empty() && !subs.iter().any(|(p, n)| *p == s.0 && n.eq_ignore_ascii_case(&s.1))
        {
            subs.push(s);
        }
    }
    subs
}

fn parse_sub(tok: &str) -> Sub {
    if let Some(rest) = tok.strip_prefix("kick:") {
        (Platform::Kick, rest.to_string())
    } else if let Some(rest) = tok.strip_prefix("yt:").or_else(|| tok.strip_prefix("youtube:")) {
        (Platform::Youtube, yt_video_id(rest))
    } else if tok.contains("youtube.com") || tok.contains("youtu.be") {
        (Platform::Youtube, yt_video_id(tok))
    } else if let Some(rest) = tok.strip_prefix("twitch:") {
        (Platform::Twitch, rest.to_string())
    } else {
        (Platform::Twitch, tok.to_string())
    }
}

/// pull the live VIDEO id out of whatever the user pasted: a bare id, a
/// `watch?v=` url, `youtu.be/id`, or `/live/id`. youtube chat subscribes by
/// video id — a handle url can't be resolved client-side, so the id is the
/// canonical channel name.
fn yt_video_id(tok: &str) -> String {
    let tail = tok
        .split_once("v=")
        .map(|(_, t)| t)
        .or_else(|| tok.split_once("youtu.be/").map(|(_, t)| t))
        .or_else(|| tok.split_once("/live/").map(|(_, t)| t))
        .unwrap_or(tok);
    tail.chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect()
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
        drain_history(&mut app);
        // new chat lines and finished emote loads land on EVERY wake — at the
        // animation cadence that's within one frame of arrival, never parked
        // until the next data tick. the tick below keeps only the slow work
        // (heat decay, the mock driver, the focused-channel prefetch sweep).
        if !app.paused {
            drain_feed(&mut app);
        }
        pump_loads(&mut app);
        // composer preview: load images for emotes in the draft as it's typed
        // (idempotent per key — a cached emote costs a hash lookup).
        if app.mode == InputMode::Insert && !app.channels.is_empty() {
            let text = app.line.text();
            let focus = app.focus;
            let App {
                store, fb, emotes, ..
            } = &mut app;
            request_stacks(store, fb, &emotes[focus], &text);
        }
        // snapshot the animation clock + reset the per-draw blit budget, so every
        // emote in this frame is sampled at the same instant.
        if let Some(store) = &app.store {
            store.begin_frame();
        }
        let drew_at = Instant::now();
        // synchronized output (DECSET 2026): the terminal holds presentation
        // until the frame is complete. without it every sixel/kitty blit shows
        // as it streams — on an animation-heavy channel that reads as flicker
        // and a cursor darting to each emote. foot/kitty/wezterm and tmux ≥3.4
        // honour it; terminals that don't simply ignore the mode.
        crossterm::execute!(io::stdout(), crossterm::terminal::BeginSynchronizedUpdate)?;
        let drew = terminal.draw(|f| ui(f, &app));
        crossterm::execute!(io::stdout(), crossterm::terminal::EndSynchronizedUpdate)?;
        drew?;
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
                tick_advance(&mut app);
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
/// Join = type a channel to open, Search = type a `/` pattern. h/l always change
/// channel; j/k are cursor motion, except on a vertical tab bar where (in
/// normal mode) they walk the channel list the eye is looking at.
fn handle_key(app: &mut App, k: crossterm::event::KeyEvent) -> Flow {
    if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
        return Flow::Quit;
    }
    match app.mode {
        // the composer is a vi line editor: it owns insert vs normal, and only
        // tells us when the line should be sent or put away.
        InputMode::Insert => {
            // tab walks emote/username completion; any other key ends the walk.
            match k.code {
                KeyCode::Tab => {
                    complete_step(app, 1);
                    return Flow::Continue;
                }
                KeyCode::BackTab => {
                    complete_step(app, -1);
                    return Flow::Continue;
                }
                _ => app.completion = None,
            }
            if !matches!(k.code, KeyCode::Esc | KeyCode::Enter) {
                app.status = None; // typing dismisses the last command's reply
            }
            // Keys core has no vocabulary for (F-keys, Insert) are dropped
            // rather than widening the editing model to carry them.
            let Some(ck) = key::to_core(k) else {
                return Flow::Continue;
            };
            match app.line.key(ck) {
                edit::Act::Continue => {}
                edit::Act::Send => return send_focused(app),
                edit::Act::Leave => app.mode = InputMode::Normal,
            }
        }
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
        // rover-style channel manager: j/k move, enter open, a add, x leave,
        // e edit slots, K/J reorder, esc back.
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
                        app.mode = InputMode::Normal;
                    }
                }
                // o joins here too — same key as normal mode.
                KeyCode::Char('a') | KeyCode::Char('o') => {
                    app.mode = InputMode::Join;
                    app.input.clear();
                }
                KeyCode::Char('x') => manage_delete(app),
                KeyCode::Char('e') => {
                    if !app.channels.is_empty() {
                        app.mode = InputMode::SlotPick;
                        app.status = None;
                    }
                }
                KeyCode::Char('K') => manage_move(app, -1),
                KeyCode::Char('J') => manage_move(app, 1),
                _ => {}
            }
        }
        // which of the row's t/k/y source slots to edit.
        InputMode::SlotPick => match k.code {
            KeyCode::Esc | KeyCode::Char('q') => app.mode = InputMode::Manage,
            KeyCode::Char('t') => slot_open(app, Platform::Twitch),
            KeyCode::Char('k') => slot_open(app, Platform::Kick),
            KeyCode::Char('y') => slot_open(app, Platform::Youtube),
            _ => {}
        },
        // retype one slot's name; enter applies, empty clears, esc backs out.
        InputMode::SlotEdit(p) => match k.code {
            KeyCode::Esc => {
                app.mode = InputMode::SlotPick;
                app.input.clear();
            }
            KeyCode::Enter => slot_apply(app, p),
            KeyCode::Backspace => {
                app.input.pop();
            }
            KeyCode::Char(c) => app.input.push(c),
            _ => {}
        },
        InputMode::Normal => return normal_key(app, k),
    }
    Flow::Continue
}

/// Normal mode: switch channels and enter the other modes. chat has no message
/// cursor — j/k and h/l (and arrows, tab/shift-tab) all move between tabs.
fn normal_key(app: &mut App, k: crossterm::event::KeyEvent) -> Flow {
    match k.code {
        KeyCode::Char('q') => Flow::Quit,
        // j/l/down/right/tab → next channel; k/h/up/left/shift-tab → previous.
        KeyCode::Char('j') | KeyCode::Char('l') | KeyCode::Down | KeyCode::Right | KeyCode::Tab => {
            tab_and_continue(app, 1)
        }
        KeyCode::Char('k') | KeyCode::Char('h') | KeyCode::Up | KeyCode::Left | KeyCode::BackTab => {
            tab_and_continue(app, -1)
        }
        KeyCode::Char('i') | KeyCode::Char('a') => {
            app.mode = InputMode::Insert;
            app.line.focus();
            app.status = None;
            Flow::Continue
        }
        KeyCode::Char('o') => {
            app.mode = InputMode::Join;
            app.input.clear();
            app.status = None;
            Flow::Continue
        }
        KeyCode::Char('x') => {
            close_channel(app);
            Flow::Continue
        }
        KeyCode::Char('m') => {
            app.mode = InputMode::Manage;
            app.manage_cursor = app.focus;
            Flow::Continue
        }
        KeyCode::Char('T') => {
            app.tab_pos = app.tab_pos.next();
            save_state(app);
            Flow::Continue
        }
        KeyCode::Char(' ') => {
            app.paused = !app.paused;
            Flow::Continue
        }
        KeyCode::Esc => {
            app.status = None;
            Flow::Continue
        }
        _ => Flow::Continue,
    }
}

fn tab_and_continue(app: &mut App, delta: i32) -> Flow {
    if delta > 0 {
        next_tab(app)
    } else {
        prev_tab(app)
    }
    Flow::Continue
}

fn next_tab(app: &mut App) {
    if app.focus + 1 < app.channels.len() {
        app.focus += 1;
    }
}

fn prev_tab(app: &mut App) {
    if app.focus > 0 {
        app.focus -= 1;
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

/// open (or focus, if already open) a tab by `name` / `kick:name` / `yt:id`,
/// or a `+`-joined merge of several.
fn open_channel(app: &mut App, tok: &str) {
    let subs = parse_tab(tok);
    let Some((platform, name)) = subs.first().cloned() else {
        return;
    };
    // a tab already carrying the primary sub upgrades in place: any sources it
    // doesn't have yet merge into it — `kick:a` open, then `kick:a+yt:x`
    // typed, and the existing tab becomes the merge instead of a dead focus.
    if let Some(i) = app
        .channels
        .iter()
        .position(|c| c.matches(platform, &name))
    {
        let fresh: Vec<Sub> = subs
            .into_iter()
            .filter(|(p, n)| !app.channels[i].matches(*p, n))
            .collect();
        for (p, n) in &fresh {
            start_sub(app, *p, n);
        }
        app.channels[i].extra.extend(fresh);
        app.focus = i;
        save_state(app);
        return;
    }
    let mut ch = Channel::new(&name, platform, 200);
    ch.extra = subs[1..].to_vec();
    for (p, n) in ch.subs() {
        start_sub(app, p, n);
    }
    app.channels.push(ch);
    // seeded with globals now; channel sets merge in async (spawn_emote_fetch).
    app.emotes.push(app.globals.clone());
    app.focus = app.channels.len() - 1;
    save_state(app);
}

/// subscribe one source over the live WS and start its emote + history fetches
/// (shared by open, merge-upgrade, and slot edits).
fn start_sub(app: &App, p: Platform, n: &str) {
    if let Some(out) = &app.out {
        let _ = out.send(net::Outbound::Join {
            platform: p,
            channel: n.to_string(),
        });
    }
    if p != Platform::Youtube {
        spawn_emote_fetch(&app.emote_tx, p, n.to_string());
        spawn_history(&app.hist_tx, p, n.to_string());
    }
}

/// the first source of platform `p` feeding `ch` — what the t/k/y slot shows.
fn slot_of(ch: &Channel, p: Platform) -> Option<String> {
    ch.subs().find(|(q, _)| *q == p).map(|(_, n)| n.to_string())
}

/// begin editing one platform slot of the row under the Manage cursor,
/// prefilled with its current name so a tweak doesn't mean retyping.
fn slot_open(app: &mut App, p: Platform) {
    if app.channels.is_empty() {
        app.mode = InputMode::Manage;
        return;
    }
    let i = app.manage_cursor.min(app.channels.len() - 1);
    app.input = slot_of(&app.channels[i], p).unwrap_or_default();
    app.mode = InputMode::SlotEdit(p);
}

/// apply a slot edit: empty input clears the slot, a new name swaps the
/// subscription in place, a name on an empty slot merges it in. the last
/// remaining source can't be cleared — that's what x (leave) is for.
fn slot_apply(app: &mut App, p: Platform) {
    let raw = app.input.trim().to_string();
    app.input.clear();
    app.mode = InputMode::Manage;
    if app.channels.is_empty() {
        return;
    }
    // a pasted youtube url still means its video id, same as the join prompt.
    let new = match p {
        Platform::Youtube if !raw.is_empty() => yt_video_id(&raw),
        _ => raw,
    };
    let i = app.manage_cursor.min(app.channels.len() - 1);
    let old = slot_of(&app.channels[i], p);
    if old
        .as_deref()
        .is_some_and(|o| o.eq_ignore_ascii_case(&new))
    {
        return; // unchanged
    }
    if new.is_empty() {
        let Some(old) = old else { return };
        let ch = &mut app.channels[i];
        if ch.extra.is_empty() {
            app.status = Some("last source — x leaves the tab".to_string());
            return;
        }
        if ch.platform == p {
            // clearing the primary promotes the next source to lead the tab.
            let (np, nn) = ch.extra.remove(0);
            ch.platform = np;
            ch.name = nn;
        } else if let Some(j) = ch.extra.iter().position(|(q, _)| *q == p) {
            ch.extra.remove(j);
        }
        if let Some(out) = &app.out {
            let _ = out.send(net::Outbound::Part {
                platform: p,
                channel: old,
            });
        }
        save_state(app);
        return;
    }
    // duplicate guards: same source twice in one tab, or open in another tab
    // (inbound lines route to the first matching tab — a dupe would shadow it).
    if app.channels[i].matches(p, &new) {
        app.status = Some(format!("{}:{new} already in this tab", p.tag()));
        return;
    }
    if app
        .channels
        .iter()
        .enumerate()
        .any(|(j, c)| j != i && c.matches(p, &new))
    {
        app.status = Some(format!("{}:{new} is open in another tab", p.tag()));
        return;
    }
    match old {
        Some(old) => {
            if let Some(out) = &app.out {
                let _ = out.send(net::Outbound::Part {
                    platform: p,
                    channel: old,
                });
            }
            let ch = &mut app.channels[i];
            if ch.platform == p {
                ch.name = new.clone();
            } else if let Some(j) = ch.extra.iter().position(|(q, _)| *q == p) {
                ch.extra[j].1 = new.clone();
            }
        }
        None => app.channels[i].extra.push((p, new.clone())),
    }
    start_sub(app, p, &new);
    save_state(app);
}

/// leave the channel under the Manage cursor.
fn manage_delete(app: &mut App) {
    if app.channels.is_empty() {
        return;
    }
    let i = app.manage_cursor.min(app.channels.len() - 1);
    part_subs(&app.out, &app.channels[i]);
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

/// unsubscribe every source a tab merges.
fn part_subs(out: &Option<net::Tx>, ch: &Channel) {
    if let Some(out) = out {
        for (p, n) in ch.subs() {
            let _ = out.send(net::Outbound::Part {
                platform: p,
                channel: n.to_string(),
            });
        }
    }
}

fn close_channel(app: &mut App) {
    if app.channels.is_empty() {
        return;
    }
    part_subs(&app.out, &app.channels[app.focus]);
    app.channels.remove(app.focus);
    app.emotes.remove(app.focus);
    app.focus = app.focus.min(app.channels.len().saturating_sub(1));
    save_state(app);
}

/// send the current input line to the focused channel (kick only; twitch has no
/// send path). clears the input on a successful enqueue.
/// enter in the composer. a leading `/` is a command — ours are handled here,
/// everything else (mod actions, /me) goes to the platform verbatim.
fn send_focused(app: &mut App) -> Flow {
    let text = match slash::parse(&app.line.text()) {
        slash::Cmd::Join(ch) => {
            app.line.accept();
            open_channel(app, &ch);
            return Flow::Continue;
        }
        slash::Cmd::Part(which) => {
            app.line.accept();
            part_channel(app, which.as_deref());
            return Flow::Continue;
        }
        slash::Cmd::Quit => return Flow::Quit,
        slash::Cmd::Usage(u) => {
            // keep the draft so the channel can just be appended
            app.status = Some(u.into());
            return Flow::Continue;
        }
        slash::Cmd::Send(t) => t,
    };
    if text.is_empty() || app.channels.is_empty() {
        return Flow::Continue;
    }
    // offline feed: echo the line locally — the composer (completion, preview,
    // history) works end-to-end with no network and nothing leaves the machine.
    if matches!(app.feed, Feed::Mock(_)) {
        let ch = &mut app.channels[app.focus];
        let now = ch.last_ms;
        ch.record(
            Message {
                platform: ch.platform,
                user: "you".into(),
                text,
                color: Some("#ff8700".into()),
                badges: Vec::new(),
                reply_to: None,
                note: None,
                heat: 0.0,
            },
            now,
        );
        app.line.accept();
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
                app.line.accept();
                app.status = Some(format!("sent → {name}"));
            }
            None => app.status = Some("no twitch token — run: heatsync login".into()),
        },
        // prefer direct kick send (own token); fall back to the ext-relay path.
        Platform::Kick => {
            if let Some(ktx) = &app.kick_tx {
                let _ = ktx.send((name.clone(), text));
                app.line.accept();
                app.status = Some(format!("sent → {name}"));
            } else if let Some(out) = &app.out {
                let _ = out.send(net::Outbound::Chat {
                    platform: Platform::Kick,
                    channel: name.clone(),
                    text,
                });
                app.line.accept();
                app.status = Some(format!("sent → {name} (via ext)"));
            } else {
                app.status = Some("no kick auth — run: heatsync login kick".into());
            }
        }
        // youtube sends only via the authenticated relay (extension path).
        Platform::Youtube => match &app.out {
            Some(out) => {
                let _ = out.send(net::Outbound::Chat {
                    platform: Platform::Youtube,
                    channel: name.clone(),
                    text,
                });
                app.line.accept();
                app.status = Some(format!("sent → {name} (via ext)"));
            }
            None => app.status = Some("no send path — set HEATSYNC_TOKEN".into()),
        },
    }
    Flow::Continue
}

/// one tab press: start or continue a completion walk over the focused
/// channel's emote names and recent chatters. `dir` is 1 (tab) or -1 (s-tab).
fn complete_step(app: &mut App, dir: i32) {
    // completion is a typing gesture — vi-normal keeps tab inert.
    if app.line.mode() != edit::Mode::Insert {
        return;
    }
    if app.completion.is_none() {
        let Some(ch) = app.channels.get(app.focus) else {
            return;
        };
        // recent chatters, newest first, deduped — the people you'd reply to.
        let mut users: Vec<&str> = Vec::new();
        for m in ch.messages.iter().rev() {
            // actorless event lines (live/offline/raid) have no user to offer.
            if !m.user.is_empty() && !users.iter().any(|u| u.eq_ignore_ascii_case(&m.user)) {
                users.push(&m.user);
            }
            if users.len() >= 50 {
                break;
            }
        }
        let text = app.line.text();
        app.completion = Completion::build(
            &text,
            app.line.cursor(),
            app.emotes[app.focus].names(),
            users.into_iter(),
        );
    }
    if let Some(c) = &mut app.completion {
        let (lo, hi, s) = c.advance(dir);
        let s = s.to_string();
        app.line.replace_range(lo, hi, &s);
    }
}

/// kick off image loads for every emote stack in `text` (idempotent — a key
/// already cached or in flight is a no-op). the prefetch model: each incoming
/// line is scanned ONCE as it arrives, plus a cheap focused-channel sweep on
/// the tick — nothing rescans every channel's backlog anymore.
fn request_stacks(store: &mut Option<EmoteStore>, fb: &Option<FbEmotes>, set: &EmoteSet, text: &str) {
    if set.is_empty() {
        return;
    }
    if let Some(s) = store {
        each_stack(text, set, |k| s.request(k));
    } else if let Some(f) = fb {
        each_stack(text, set, |k| f.request(k));
    }
}

/// drain the live feed into the channel buffers. runs on every event-loop wake,
/// so a chat line shows on the very next frame instead of the next data tick.
fn drain_feed(app: &mut App) {
    let App {
        channels,
        emotes,
        feed,
        store,
        fb,
        status,
        focus,
        ..
    } = app;
    let Feed::Live {
        rx,
        start,
        connected,
    } = feed
    else {
        return; // mock advances on the tick — it's a synthetic cadence
    };
    let now = start.elapsed().as_millis() as u64;
    while let Ok(ev) = rx.try_recv() {
        match ev {
            ChatEvent::Line(l) => {
                if let Some(i) = channels
                    .iter()
                    .position(|c| c.matches(l.platform, &l.channel))
                {
                    // fetch starts the instant the line lands — but only for
                    // the channel on screen. prefetching every background
                    // channel's emotes burned network + decode for images
                    // that might never be looked at; a switched-to channel is
                    // filled by the tick sweep within 200ms instead.
                    // the relay double-broadcasts some event frames (usernotice
                    // arrives twice with the same id) — drop a note identical
                    // to one already in the recent tail.
                    // 16 covers a spam-speed channel filling the gap between
                    // the two broadcasts (~200ms apart).
                    if l.note.is_some()
                        && channels[i].messages.iter().rev().take(16).any(|e| {
                            e.platform == l.platform
                                && e.user == l.user
                                && e.text == l.content
                                && e.note.as_ref() == l.note.as_ref()
                        })
                    {
                        continue;
                    }
                    if i == *focus {
                        request_stacks(store, fb, &emotes[i], &l.content);
                    }
                    channels[i].record(
                        Message {
                            platform: l.platform,
                            user: l.user,
                            text: l.content,
                            color: l.color,
                            badges: l.badges,
                            reply_to: l.reply_to,
                            note: l.note,
                            heat: 0.0,
                        },
                        now,
                    );
                }
            }
            ChatEvent::Connected => *connected = true,
            ChatEvent::Disconnected => *connected = false,
            ChatEvent::Auth(ok) => {
                *status = Some(if ok {
                    "authenticated".into()
                } else {
                    "auth failed — check HEATSYNC_TOKEN".into()
                });
            }
            ChatEvent::SendResult { ok, error } => {
                if !ok {
                    *status = Some(format!("send failed: {}", error.unwrap_or_default()));
                }
            }
        }
    }
}

/// merge finished emote loads into the cache (every wake — a built emote is on
/// screen the next frame, never parked until the tick).
fn pump_loads(app: &mut App) {
    if let Some(store) = &mut app.store {
        store.pump();
    } else if let Some(fb) = &mut app.fb {
        fb.pump();
    }
}

/// the slow-cadence work: heat decay, the mock driver, and a prefetch sweep of
/// the focused channel only (covers focus switches and cache-evicted emotes;
/// arrival-time scans in drain_feed cover everything else).
fn tick_advance(app: &mut App) {
    let App {
        channels,
        emotes,
        feed,
        store,
        fb,
        focus,
        ..
    } = app;
    match feed {
        Feed::Mock(driver) => {
            driver.tick(channels);
        }
        Feed::Live { start, .. } => {
            let now = start.elapsed().as_millis() as u64;
            for ch in channels.iter_mut() {
                ch.cool(now);
            }
        }
    }
    if let Some(ch) = channels.get(*focus) {
        let from = ch.messages.len().saturating_sub(60);
        for m in ch.messages.iter().skip(from) {
            request_stacks(store, fb, &emotes[*focus], &m.text);
        }
    }
}

/// width of the tab column when the bar is vertical (left/right).
const TAB_COL_W: u16 = 16;

fn emote_mode(app: &App) -> EmoteMode<'_> {
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

fn ui(f: &mut Frame, app: &App) {
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
                Span::styled(
                    "  no channels — press ",
                    Style::default().fg(Color::Indexed(244)),
                ),
                Span::styled("o", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
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
        .any(|s| matches!(s, Segment::Stack(_))) { EMOTE_H } else { 0 }
}

/// the draft as it will render — same layout path as a real chat row (first
/// visual row; the preview strip is one emote-row tall).
fn draw_preview(f: &mut Frame, area: Rect, app: &App, mode: EmoteMode) {
    let set = &app.emotes[app.focus];
    let mut b = Rows::new(area.width);
    b.prefix(Span::styled(" ❯ ", Style::default().fg(ACCENT)), 3);
    let text = app.line.text();
    layout_text(&mut b, &text, set, mode);
    if let Some(row) = b.finish(None).into_iter().next() {
        draw_planned(f, mode, area, area.y, row.line, &row.places, EMOTE_H);
    }
}

/// the channel tab bar — horizontal row (top/bottom) or vertical list (left/right).
fn draw_tabs(f: &mut Frame, area: Rect, app: &App) {
    let tab_style = |i: usize, heat: f64| {
        if i == app.focus {
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Indexed(Tier::of(heat).xterm()))
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
            let label = fit(format!(" {}{} {:.0} ", ch.name, plus(ch), ch.heat), area.width);
            let row = Rect {
                x: area.x,
                y: area.y + i as u16,
                width: area.width,
                height: 1,
            };
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(label, tab_style(i, ch.heat)))),
                row,
            );
        }
    } else {
        let mut spans = Vec::new();
        for (i, ch) in app.channels.iter().enumerate() {
            spans.push(Span::styled(
                format!(
                    " {}·{}{} {:.0} ",
                    ch.name,
                    ch.platform.tag(),
                    plus(ch),
                    ch.heat
                ),
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
    let [barrow, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    f.render_widget(
        Paragraph::new(heat_bar(ch.heat, barrow.width as usize)),
        barrow,
    );
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
    for m in ch.messages.iter().rev() {
        let mut rows = layout_message(m, set, mode, body.width, app.me.as_deref(), ch.merged());
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
                    f.render_widget(
                        Image::new(proto),
                        Rect {
                            x,
                            y,
                            width: p.w,
                            height: h,
                        },
                    );
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

/// rover-style channel manager: a single-pane list, cursor row highlighted, the
/// active channel marked. reorder/leave/open from here.
fn draw_manage(f: &mut Frame, area: Rect, app: &App) {
    let [head, list] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " channels ",
                Style::default()
                    .fg(Color::Black)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {} open", app.channels.len()),
                Style::default().fg(Color::Indexed(244)),
            ),
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
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(hue)
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

/// walk a message's emote STACKS in order, yielding each stack's cache key.
/// core's [`segments`] owns the grammar — zero-width overlays, `w!`-style BTTV
/// prefixes, `z!` forcing, FFZ effect words. shared by layout and prefetch.
fn each_stack(text: &str, set: &EmoteSet, mut f: impl FnMut(&str)) {
    for seg in segments(text, set) {
        if let Segment::Stack(s) = seg {
            f(&s.key());
        }
    }
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
struct Place {
    col: u16,
    /// this emote's own width in cells — square emotes and wide emotes get
    /// different footprints instead of one fixed block.
    w: u16,
    key: String,
}

/// does `text` address `me` — as a word, with or without a leading `@` and
/// trailing punctuation ("@Mellen," pings mellen).
fn mentions(text: &str, me: &str) -> bool {
    text.split_whitespace().any(|w| {
        w.trim_start_matches('@')
            .trim_end_matches(|c: char| !c.is_alphanumeric() && c != '_')
            .eq_ignore_ascii_case(me)
    })
}

/// a platform's brand hue in the 256 palette — the merged-tab line marker.
fn platform_color(p: Platform) -> Color {
    Color::Indexed(match p {
        Platform::Twitch => 99,   // twitch purple
        Platform::Kick => 82,     // kick green
        Platform::Youtube => 196, // youtube red
    })
}

/// one-cell role badge: black glyph on the role's color. same black-on-color
/// scheme as the active tab — square, dense, no brackets.
fn badge_span(b: Badge) -> Span<'static> {
    let bg = match b {
        Badge::Broadcaster => 196, // red
        Badge::Moderator => 40,    // green
        Badge::Vip => 213,         // pink
        Badge::Subscriber => 99,   // purple
        Badge::Founder => 208,     // orange
        Badge::Staff => 129,       // violet
        Badge::Verified => 45,     // cyan
        Badge::Og => 51,           // teal
    };
    Span::styled(
        b.glyph().to_string(),
        Style::default()
            .fg(Color::Black)
            .bg(Color::Indexed(bg))
            .add_modifier(Modifier::BOLD),
    )
}

/// continuation rows hang under the message body by this many columns.
const WRAP_INDENT: u16 = 2;
/// visual rows one message may occupy — bounds an emote/text wall so a single
/// line can never eat the whole pane. twitch/kick cap text at 500 chars, which
/// wraps well inside this on any sane width.
const MAX_MSG_ROWS: usize = 8;

/// one visual row of a laid-out message: its text line, its reserved emote
/// slots, and its height (EMOTE_H when it holds an image, else 1).
struct RowPlan {
    line: Line<'static>,
    places: Vec<Place>,
    h: u16,
}

/// the wrapping layouter: words and emote stacks flow left to right and wrap
/// onto indented continuation rows. shared by chat rows and the composer
/// preview.
struct Rows {
    maxw: u16,
    rows: Vec<RowPlan>,
    spans: Vec<Span<'static>>,
    places: Vec<Place>,
    col: u16,
    row_start: u16, // col where content on this row begins (prefix or indent)
    has_stack: bool,
    full: bool, // hit MAX_MSG_ROWS — stop consuming
}

impl Rows {
    fn new(maxw: u16) -> Rows {
        Rows {
            maxw: maxw.max(WRAP_INDENT + 1),
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
    fn prefix(&mut self, span: Span<'static>, w: u16) {
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
        self.col = WRAP_INDENT;
        self.row_start = WRAP_INDENT;
        self.spans.push(Span::raw(" ".repeat(WRAP_INDENT as usize)));
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
            return self.word(
                &s.base,
                Style::default()
                    .fg(Color::Indexed(231))
                    .add_modifier(Modifier::BOLD),
            );
        };
        if self.col + w > self.maxw && !self.at_row_start() {
            self.newline();
            if self.full {
                return;
            }
        }
        if self.col + w > self.maxw {
            return self.word(&s.base, Style::default().fg(Color::Indexed(231)));
        }
        self.has_stack = true;
        if ready {
            self.places.push(Place { col: self.col, w, key });
            self.spans.push(Span::raw(" ".repeat(w as usize)));
        } else {
            // loading: hold the exact footprint and show what fits of the
            // name, so the image swaps in place instead of shoving the line.
            self.spans.push(Span::styled(
                fit_exact(&s.base, w),
                Style::default().fg(Color::Indexed(231)),
            ));
        }
        self.col += w;
    }

    /// close out; `bg` (mention slab) applies to every row.
    fn finish(mut self, bg: Option<Style>) -> Vec<RowPlan> {
        if self.full {
            // truncated: drop the empty continuation, mark the last real row.
            if let Some(last) = self.rows.last_mut() {
                last.line
                    .spans
                    .push(Span::styled("…", Style::default().fg(Color::Indexed(244))));
            }
        } else if !self.spans.is_empty() || self.rows.is_empty() {
            let h = if self.has_stack { EMOTE_H } else { 1 };
            self.rows.push(RowPlan {
                line: Line::from(std::mem::take(&mut self.spans)),
                places: std::mem::take(&mut self.places),
                h,
            });
        }
        if let Some(bg) = bg {
            for r in &mut self.rows {
                r.line = std::mem::take(&mut r.line).style(bg);
            }
        }
        self.rows
    }
}

/// flow `text`'s segments (words + emote stacks) into the layouter.
fn layout_text(b: &mut Rows, text: &str, set: &EmoteSet, mode: EmoteMode) {
    // message text is plain white — heat lives in the bar and tab numbers,
    // never in the reading surface.
    let text_hue = Style::default().fg(Color::Indexed(231));
    for seg in segments(text, set) {
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

/// lay a message out as wrapped visual rows: badges + user prefix on the
/// first, indented continuations after, emote cells reserved wherever their
/// stack lands.
fn layout_message(
    m: &Message,
    set: &EmoteSet,
    mode: EmoteMode,
    maxw: u16,
    me: Option<&str>,
    tag_platform: bool,
) -> Vec<RowPlan> {
    let user_color = m
        .color
        .as_deref()
        .and_then(parse_hex)
        .unwrap_or(Color::Indexed(244));
    let mut b = Rows::new(maxw);
    // merged tab: a one-cell bar in the platform's hue marks each line's origin.
    if tag_platform {
        b.prefix(
            Span::styled("▎", Style::default().fg(platform_color(m.platform))),
            1,
        );
    }
    // an event line: glyph + actor + headline in the event's hue, then any
    // attached chat text (resub message, kicks message) laid out like chat.
    if let Some(n) = &m.note {
        let (glyph, idx) = note_style(n.kind);
        let hue = Style::default().fg(Color::Indexed(idx));
        b.prefix(Span::styled(glyph, hue), 1);
        b.prefix(Span::raw(" "), 1);
        if !m.user.is_empty() {
            let uw = UnicodeWidthStr::width(m.user.as_str()) as u16;
            b.prefix(
                Span::styled(m.user.clone(), Style::default().fg(user_color)),
                uw,
            );
            b.prefix(Span::raw(" "), 1);
        }
        for w in n.what.split(' ') {
            b.word(w, hue);
        }
        if !m.text.is_empty() {
            b.word("·", Style::default().fg(Color::Indexed(244)));
            layout_text(&mut b, &m.text, set, mode);
        }
        let bg = me
            .is_some_and(|me| mentions(&m.text, me))
            .then(|| Style::default().bg(Color::Indexed(236)));
        return b.finish(bg);
    }
    // role badges, capped — a badge wall must not eat the line.
    for &bd in m.badges.iter().take(3) {
        b.prefix(badge_span(bd), 1);
    }
    if !m.badges.is_empty() {
        b.prefix(Span::raw(" "), 1);
    }
    let uw = UnicodeWidthStr::width(m.user.as_str()) as u16;
    b.prefix(
        Span::styled(m.user.clone(), Style::default().fg(user_color)),
        uw,
    );
    // reply thread marker: who this message answers, dim, before the content.
    if let Some(r) = &m.reply_to {
        let tag = format!(" ↳{r}");
        let tw = UnicodeWidthStr::width(tag.as_str()) as u16;
        b.prefix(Span::styled(tag, Style::default().fg(Color::Indexed(244))), tw);
    }
    b.prefix(Span::styled(": ", Style::default().fg(Color::Indexed(244))), 2);
    layout_text(&mut b, &m.text, set, mode);
    // a line that pings you gets a quiet slab under it — semantic, not decor.
    let bg = me
        .is_some_and(|me| mentions(&m.text, me))
        .then(|| Style::default().bg(Color::Indexed(236)));
    b.finish(bg)
}

/// one-cell glyph + xterm-256 hue for each event kind. semantic: green=live,
/// red=mod/danger, orange=money+hype (brand), dim=gone.
fn note_style(k: heatsync_core::NoteKind) -> (&'static str, u8) {
    use heatsync_core::NoteKind as K;
    match k {
        K::Sub => ("★", 220),
        K::Gift => ("✦", 213),
        K::Cheer => ("◆", 208),
        K::Raid => ("⚑", 201),
        K::Redeem => ("◇", 39),
        K::Live => ("●", 40),
        K::Offline => ("○", 244),
        K::Category => ("→", 75),
        K::Notice => ("»", 75),
        K::Spike => ("▲", 208),
        K::Mod => ("×", 196),
    }
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
    static TRUECOLOR: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let tc = *TRUECOLOR.get_or_init(|| {
        std::env::var("COLORTERM")
            .map(|v| v.contains("truecolor") || v.contains("24bit"))
            .unwrap_or(false)
    });
    Some(if tc {
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

fn heat_bar(heat: f64, width: usize) -> Line<'static> {
    let width = width.max(1);
    let frac = (heat / heatsync_core::heat::MYTHIC).clamp(0.0, 1.0);
    let filled = (frac * width as f64).round() as usize;
    let hue = Color::Indexed(heatsync_core::heat::color(heat));
    Line::from(vec![
        Span::styled("\u{2588}".repeat(filled), Style::default().fg(hue)),
        Span::styled(
            "\u{2591}".repeat(width - filled),
            Style::default().fg(Color::Indexed(236)),
        ),
    ])
}

/// one `key label` footer hint: brand-colored key, dim label. every hint bar
/// shows exactly ONE key per action — aliases stay out of the footer.
fn hint(k: &'static str, d: &'static str) -> [Span<'static>; 2] {
    [
        Span::styled(k, Style::default().fg(ACCENT)),
        Span::styled(format!(" {d}  "), Style::default().fg(Color::Indexed(244))),
    ]
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App, n: usize) {
    let tag = Style::default()
        .fg(Color::Black)
        .bg(ACCENT)
        .add_modifier(Modifier::BOLD);
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
            spans.push(Span::styled(
                format!(" {msg}"),
                Style::default().fg(Color::Indexed(214)),
            ));
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
                spans.push(Span::styled(key, Style::default().fg(ACCENT)));
                spans.push(Span::styled(
                    format!(
                        " {}:{}  ",
                        p.tag(),
                        slot_of(ch, p).unwrap_or_else(|| "—".to_string())
                    ),
                    Style::default().fg(Color::Indexed(244)),
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
            Span::styled(" ❯ ", Style::default().fg(ACCENT)),
            Span::styled(app.input.clone(), Style::default().fg(Color::Indexed(231))),
            Span::styled("\u{2588}", Style::default().fg(ACCENT)),
            Span::styled(
                "   enter apply · empty clears · esc back",
                Style::default().fg(Color::Indexed(244)),
            ),
        ];
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }
    // Join mode → type a channel to open.
    if app.mode == InputMode::Join {
        let spans = vec![
            Span::styled(
                " join ",
                Style::default()
                    .fg(Color::Black)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ❯ ", Style::default().fg(ACCENT)),
            Span::styled(app.input.clone(), Style::default().fg(Color::Indexed(231))),
            Span::styled("\u{2588}", Style::default().fg(ACCENT)),
            Span::styled(
                "   name · kick:name · yt:video",
                Style::default().fg(Color::Indexed(244)),
            ),
        ];
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }
    // Insert mode → the message composer for the focused channel.
    if app.mode == InputMode::Insert && !app.channels.is_empty() {
        let ch = &app.channels[app.focus];
        // read-only only when there's genuinely no send path for this platform:
        // twitch needs the user's own token; kick can also relay via the ws.
        // the mock feed echoes locally, so it always composes.
        let readonly = match (&app.feed, ch.platform) {
            (Feed::Mock(_), _) => false,
            (_, Platform::Twitch) => app.twitch_tx.is_none(),
            (_, Platform::Kick) => app.kick_tx.is_none() && app.out.is_none(),
            (_, Platform::Youtube) => app.out.is_none(),
        };
        // the tag names the mode as well as the target, so `esc` never leaves
        // you guessing whether a keystroke will type or command.
        let normal = app.line.mode() == edit::Mode::Normal;
        let prompt = if normal {
            format!(" {}·{} NORMAL ", ch.name, ch.platform.tag())
        } else {
            format!(" {}·{} ", ch.name, ch.platform.tag())
        };
        let tag = Style::default()
            .fg(Color::Black)
            .bg(ACCENT)
            .add_modifier(Modifier::BOLD);
        let mut spans = vec![
            Span::styled(prompt, tag),
            Span::styled(" ❯ ", Style::default().fg(ACCENT)),
        ];
        if readonly {
            spans.push(Span::styled(
                "read-only — no send token · esc",
                Style::default().fg(Color::Indexed(214)),
            ));
        } else {
            // draw the line with a block cursor sitting ON a character in normal
            // mode and between characters in insert — the same shape vi has, so
            // the mode is readable without looking at the tag.
            let chars: Vec<char> = app.line.text().chars().collect();
            let at = app.line.cursor();
            let body = Style::default().fg(Color::Indexed(231));
            let block = Style::default().fg(Color::Black).bg(Color::White);
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
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ));
            }
            // a command's reply (usage, "not open: x") has to be visible from
            // the composer — that is where the command was typed.
            if let Some(msg) = &app.status {
                spans.push(Span::styled(
                    format!("   {msg}"),
                    Style::default().fg(Color::Indexed(214)),
                ));
            } else if app.line.is_empty() && !normal {
                spans.push(Span::styled(
                    "   tab completes emotes/@users  ·  /join /part /quit  ·  text goes to chat",
                    Style::default().fg(Color::Indexed(244)),
                ));
            } else if normal {
                spans.push(Span::styled(
                    "   kj history  esc leave",
                    Style::default().fg(Color::Indexed(244)),
                ));
            }
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }

    let (dot, dot_color, state) = match &app.feed {
        Feed::Mock(_) => ("\u{25cb} ", Color::Indexed(244), "mock".to_string()),
        Feed::Live {
            connected: true, ..
        } => ("\u{25cf} ", Color::Indexed(46), "live".to_string()),
        Feed::Live {
            connected: false, ..
        } => ("\u{25cf} ", Color::Indexed(214), "connecting".to_string()),
    };
    let mut spans = vec![Span::styled(
        " heatsync ",
        Style::default()
            .fg(Color::Black)
            .bg(ACCENT)
            .add_modifier(Modifier::BOLD),
    )];
    // a message (search miss, send error) takes the front of the line the way
    // vi's command line does — appended after the key hints it was simply
    // truncated away on anything narrower than ~110 columns.
    if let Some(msg) = &app.status {
        spans.push(Span::styled(
            format!("  {msg}"),
            Style::default().fg(Color::Indexed(214)),
        ));
    }
    // essentials only, one key per action — the full set fits a phone-width
    // terminal.
    spans.push(Span::raw("  "));
    for pair in [
        hint("jk", "chan"),
        hint("i", "say"),
        hint("o", "join"),
        hint("m", "manage"),
        hint("q", "quit"),
    ] {
        spans.extend(pair);
    }
    if app.paused {
        spans.push(Span::styled(
            "PAUSED  ",
            Style::default()
                .fg(Color::Indexed(214))
                .add_modifier(Modifier::BOLD),
        ));
    }
    spans.push(Span::styled(dot, Style::default().fg(dot_color)));
    spans.push(Span::styled(
        format!("{state} · {n} ch"),
        Style::default().fg(Color::Indexed(244)),
    ));
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
mod spec_tests {
    use super::*;

    #[test]
    fn plus_and_whitespace_both_merge() {
        let want = vec![
            (Platform::Twitch, "nl_kripp".to_string()),
            (Platform::Kick, "nl_kripp".to_string()),
            (Platform::Youtube, "4tDC0sKhTnA".to_string()),
        ];
        assert_eq!(parse_tab("nl_kripp+kick:nl_kripp+yt:4tDC0sKhTnA"), want);
        assert_eq!(parse_tab("twitch:nl_kripp kick:nl_kripp yt:4tDC0sKhTnA"), want);
        assert_eq!(parse_tab("  nl_kripp  +  kick:nl_kripp "), want[..2].to_vec());
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
mod wrap_tests {
    use super::*;

    fn msg(text: &str) -> Message {
        Message {
            platform: Platform::Twitch,
            user: "u".into(),
            text: text.into(),
            color: None,
            badges: Vec::new(),
            reply_to: None,
            note: None,
            heat: 0.0,
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
    fn long_messages_wrap_onto_indented_rows() {
        let set = EmoteSet::new();
        let rows = layout_message(&msg("one two three four five"), &set, EmoteMode::Text, 12, None, false);
        let got = flat(&rows);
        assert!(got.len() > 1, "wrapped: {got:?}");
        assert_eq!(got[0], "u: one two");
        assert!(got[1].starts_with("  three"), "indented continuation: {got:?}");
        // nothing exceeds the width
        for r in &got {
            assert!(UnicodeWidthStr::width(r.as_str()) <= 12, "{r:?}");
        }
    }

    #[test]
    fn a_word_wider_than_the_row_hard_splits() {
        let set = EmoteSet::new();
        let rows = layout_message(&msg("https://example.com/really/long/url"), &set, EmoteMode::Text, 14, None, false);
        let got = flat(&rows);
        assert!(got.len() >= 3, "{got:?}");
        for r in &got {
            assert!(UnicodeWidthStr::width(r.as_str()) <= 14, "{r:?}");
        }
        assert_eq!(got.join("").replace(' ', ""), "u:https://example.com/really/long/url".replace(' ', ""));
    }

    #[test]
    fn a_wall_is_capped_with_an_ellipsis() {
        let set = EmoteSet::new();
        let text = "word ".repeat(200);
        let rows = layout_message(&msg(&text), &set, EmoteMode::Text, 20, None, false);
        assert_eq!(rows.len(), MAX_MSG_ROWS);
        assert!(flat(&rows).last().unwrap().ends_with('…'));
    }

    #[test]
    fn short_messages_stay_on_one_row() {
        let set = EmoteSet::new();
        let rows = layout_message(&msg("hi"), &set, EmoteMode::Text, 40, None, false);
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
