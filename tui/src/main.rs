//! heatsync tui — heat-sorted live multichat. `heatsync [channels…]` opens the
//! grid (default demo set); `--mock` runs the offline synthetic feed; `log`,
//! `search`, `hot` are headless corpus subcommands.
//! keys: q quit · j/k scroll focused · 1-9 focus · tab cycle · space pause

#[allow(dead_code)]
mod cli;
mod config;
mod emote;
mod http;
mod kick;
mod net;
mod twitch;

use std::io;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use config::TabPos;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use emote::fb::FbEmotes;
use emote::render::{key_width, EmoteStore, EMOTE_H};
use heatsync_core::emote::{segments, EmoteSet, Segment};
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
    Insert,
    Join,
    Manage,
}

/// the line editor behind Insert/Join — cursor-addressed, completion-aware.
/// `cur` is a byte index, always on a char boundary.
#[derive(Default)]
struct Composer {
    text: String,
    cur: usize,
    comp: Option<Completion>,
}

/// an active tab-completion cycle over the word at `start..end`.
struct Completion {
    items: Vec<String>,
    idx: usize,
    start: usize,
    end: usize,
}

impl Composer {
    fn clear(&mut self) {
        self.text.clear();
        self.cur = 0;
        self.comp = None;
    }

    /// take the trimmed line and reset (Enter).
    fn take(&mut self) -> String {
        let t = self.text.trim().to_string();
        self.clear();
        t
    }

    fn insert(&mut self, c: char) {
        self.text.insert(self.cur, c);
        self.cur += c.len_utf8();
        self.comp = None;
    }

    fn backspace(&mut self) {
        if let Some(c) = self.text[..self.cur].chars().next_back() {
            self.cur -= c.len_utf8();
            self.text.remove(self.cur);
        }
        self.comp = None;
    }

    fn delete(&mut self) {
        if self.cur < self.text.len() {
            self.text.remove(self.cur);
        }
        self.comp = None;
    }

    fn left(&mut self) {
        if let Some(c) = self.text[..self.cur].chars().next_back() {
            self.cur -= c.len_utf8();
        }
        self.comp = None;
    }

    fn right(&mut self) {
        if let Some(c) = self.text[self.cur..].chars().next() {
            self.cur += c.len_utf8();
        }
        self.comp = None;
    }

    fn home(&mut self) {
        self.cur = 0;
        self.comp = None;
    }

    fn end(&mut self) {
        self.cur = self.text.len();
        self.comp = None;
    }

    /// ctrl-w — delete the word (and trailing spaces) before the cursor.
    fn kill_word(&mut self) {
        let head = &self.text[..self.cur];
        let trimmed = head.trim_end();
        let cut = trimmed
            .rfind(char::is_whitespace)
            .map(|i| i + 1)
            .unwrap_or(0);
        self.text.replace_range(cut..self.cur, "");
        self.cur = cut;
        self.comp = None;
    }

    /// ctrl-u — delete everything before the cursor.
    fn kill_to_start(&mut self) {
        self.text.replace_range(..self.cur, "");
        self.cur = 0;
        self.comp = None;
    }

    /// the word the cursor sits at the end of: (byte start, the word so far).
    fn word_at_cursor(&self) -> (usize, &str) {
        let head = &self.text[..self.cur];
        let start = head.rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
        (start, &head[start..])
    }

    /// tab / shift-tab: start or cycle a completion. `candidates` is only
    /// consulted when starting a fresh cycle.
    fn complete(&mut self, back: bool, candidates: impl FnOnce(&str) -> Vec<String>) {
        match &mut self.comp {
            Some(c) => {
                c.idx = if back {
                    (c.idx + c.items.len() - 1) % c.items.len()
                } else {
                    (c.idx + 1) % c.items.len()
                };
            }
            None => {
                let (start, word) = self.word_at_cursor();
                if word.is_empty() {
                    return;
                }
                let items = candidates(word);
                if items.is_empty() {
                    return;
                }
                self.comp = Some(Completion {
                    items,
                    idx: 0,
                    start,
                    end: self.cur,
                });
            }
        }
        // apply the selected item over the current word/previous pick.
        let c = self.comp.as_mut().expect("set above");
        let pick = c.items[c.idx].clone();
        self.text.replace_range(c.start..c.end, &pick);
        c.end = c.start + pick.len();
        self.cur = c.end;
    }
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
    composer: Composer,
    status: Option<String>, // transient one-line notice (send errors, etc.)
    out: Option<net::Tx>,   // outbound channel to the live WS thread
    twitch_tx: Option<std::sync::mpsc::Sender<twitch::Send>>, // direct twitch sender
    kick_tx: Option<std::sync::mpsc::Sender<kick::Send>>, // direct kick sender
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
            emotes: (0..mock::channels().len())
                .map(|_| EmoteSet::new())
                .collect(),
            focus: 0,
            scroll: 0,
            paused: false,
            feed: Feed::Mock(mock::Driver::new()),
            store: None,
            fb: None,
            mode: InputMode::Normal,
            composer: Composer::default(),
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

    eprintln!(
        "heatsync: fetching emotes + connecting to {} channels…",
        subs.len()
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
    let mut channels = Vec::new();
    let mut emotes = Vec::new();
    for (platform, name) in &subs {
        channels.push(Channel::new(name, *platform, 200));
        emotes.push(EmoteSet::new());
        spawn_emote_fetch(&emote_tx, *platform, name.clone());
    }

    let token = std::env::var("HEATSYNC_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());
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
        composer: Composer::default(),
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
    config::save(&config::Config {
        tab_pos: app.tab_pos,
        channels,
    });
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
    // animation redraw cadence is protocol-tuned: fast + flicker-free on kitty/
    // iterm2, gentle + tear-free on sixel. text/console tiers never spin fast.
    let anim_frame = app
        .store
        .as_ref()
        .map_or(Duration::from_millis(100), EmoteStore::anim_interval);
    let mut last = Instant::now();
    // sixel/halfblocks: a freshly-loaded static emote is emitted once and foot/
    // tmux can drop that single write. when one lands we force ONE full re-emit
    // next frame via swap_buffers() (resets the diff target so every cell redraws
    // — same effect as a tab switch, but WITHOUT the screen-clear escape that
    // orphans sixels through tmux). debounced so a load storm can't thrash it.
    let mut pending_repaint = false;
    let mut last_repaint = Instant::now();
    let repaint_debounce = Duration::from_millis(300);

    loop {
        drain_emotes(&mut app);
        // per-frame emote draw budget (decoupled from the data-tick pump).
        if let Some(store) = &app.store {
            store.reset_budget();
        }
        terminal.draw(|f| ui(f, &app))?;
        if pending_repaint && last_repaint.elapsed() >= repaint_debounce {
            // discard the diff baseline so the NEXT draw re-sends every cell,
            // landing the sixel foot dropped — no clear, no flash.
            terminal.swap_buffers();
            last_repaint = Instant::now();
            pending_repaint = false;
        }
        // console tier: paint emote pixels onto the reserved cells now that the
        // text has flushed. terminal tiers draw inline during the frame above.
        if let Some(fb) = &app.fb {
            fb.blit();
        }

        let tick_left = tick.saturating_sub(last.elapsed());
        let animating = app.store.as_ref().is_some_and(EmoteStore::any_animated)
            || app.fb.as_ref().is_some_and(FbEmotes::any_animated);
        let wait = if animating {
            anim_frame.min(tick_left)
        } else {
            tick_left
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
            if !app.paused && advance(&mut app) {
                pending_repaint |= app
                    .store
                    .as_ref()
                    .is_some_and(EmoteStore::needs_load_repaint);
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
/// Join = type a channel to open. tab nav is orientation-aware: with a vertical
/// bar (left/right) j/k move between tabs; with a horizontal bar h/l do. arrows
/// always work (left/right = tabs, up/down = scroll).
fn handle_key(app: &mut App, k: crossterm::event::KeyEvent) -> Flow {
    if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
        return Flow::Quit;
    }
    match app.mode {
        InputMode::Insert => match k.code {
            KeyCode::Esc => app.mode = InputMode::Normal,
            KeyCode::Enter => send_focused(app),
            KeyCode::Tab => {
                let items = insert_candidates(app);
                app.composer
                    .complete(false, |w| filter_candidates(items, w));
            }
            KeyCode::BackTab => {
                let items = insert_candidates(app);
                app.composer.complete(true, |w| filter_candidates(items, w));
            }
            _ => edit_key(&mut app.composer, k),
        },
        InputMode::Join => match k.code {
            KeyCode::Esc => {
                app.mode = InputMode::Normal;
                app.composer.clear();
            }
            KeyCode::Enter => join_channel(app),
            KeyCode::Tab => {
                let items = join_candidates(app);
                app.composer
                    .complete(false, |w| filter_candidates(items, w));
            }
            KeyCode::BackTab => {
                let items = join_candidates(app);
                app.composer.complete(true, |w| filter_candidates(items, w));
            }
            _ => edit_key(&mut app.composer, k),
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
                        app.scroll = 0;
                        app.mode = InputMode::Normal;
                    }
                }
                KeyCode::Char('a') | KeyCode::Char('n') | KeyCode::Char('o') => {
                    app.mode = InputMode::Join;
                    app.composer.clear();
                }
                KeyCode::Char('d') | KeyCode::Char('x') => manage_delete(app),
                KeyCode::Char('K') => manage_move(app, -1),
                KeyCode::Char('J') => manage_move(app, 1),
                _ => {}
            }
        }
        InputMode::Normal => {
            let vert = app.tab_pos.is_vertical();
            match k.code {
                KeyCode::Char('q') => return Flow::Quit,
                KeyCode::Char('j') if vert => next_tab(app),
                KeyCode::Char('k') if vert => prev_tab(app),
                KeyCode::Char('j') => app.scroll = app.scroll.saturating_sub(1), // newer
                KeyCode::Char('k') => app.scroll = app.scroll.saturating_add(1), // older
                KeyCode::Char('h') if !vert => prev_tab(app),
                KeyCode::Char('l') if !vert => next_tab(app),
                KeyCode::Left => prev_tab(app),
                KeyCode::Right => next_tab(app),
                KeyCode::Tab => next_tab(app),
                KeyCode::BackTab => prev_tab(app),
                KeyCode::Char(c @ '1'..='9') => {
                    let i = c as usize - '1' as usize;
                    if i < app.channels.len() {
                        app.focus = i;
                        app.scroll = 0;
                    }
                }
                KeyCode::Down => app.scroll = app.scroll.saturating_sub(1),
                KeyCode::Up => app.scroll = app.scroll.saturating_add(1),
                KeyCode::Char('g') => app.scroll = usize::MAX / 2, // oldest loaded
                KeyCode::Char('G') => app.scroll = 0,              // newest
                KeyCode::Char('i') | KeyCode::Char('a') => {
                    app.mode = InputMode::Insert;
                    app.status = None;
                }
                KeyCode::Char('o') => {
                    app.mode = InputMode::Join;
                    app.composer.clear();
                    app.status = None;
                }
                KeyCode::Char('x') => close_channel(app),
                KeyCode::Char('m') => {
                    app.mode = InputMode::Manage;
                    app.manage_cursor = app.focus;
                }
                KeyCode::Char('T') => {
                    app.tab_pos = app.tab_pos.next();
                    save_state(app);
                }
                KeyCode::Char(' ') => app.paused = !app.paused,
                KeyCode::Esc => app.status = None,
                _ => {}
            }
        }
    }
    Flow::Continue
}

/// shared line-editor keys for Insert/Join (arrows, ctrl-w/u/a/e, chars).
fn edit_key(c: &mut Composer, k: crossterm::event::KeyEvent) {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    match k.code {
        KeyCode::Char('w') if ctrl => c.kill_word(),
        KeyCode::Char('u') if ctrl => c.kill_to_start(),
        KeyCode::Char('a') if ctrl => c.home(),
        KeyCode::Char('e') if ctrl => c.end(),
        KeyCode::Backspace => c.backspace(),
        KeyCode::Delete => c.delete(),
        KeyCode::Left => c.left(),
        KeyCode::Right => c.right(),
        KeyCode::Home => c.home(),
        KeyCode::End => c.end(),
        KeyCode::Char(ch) if !ctrl => c.insert(ch),
        _ => {}
    }
}

/// completion pools — gathered up front so the composer borrow stays clean.
#[derive(Default)]
struct Pools {
    emotes: Vec<String>,
    users: Vec<String>,
}

/// Insert-mode pools: the focused channel's emote names (+ the ffz effect
/// words), and recent chatters (newest first, deduped).
fn insert_candidates(app: &App) -> Pools {
    if app.channels.is_empty() {
        return Pools::default();
    }
    let mut emotes: Vec<String> = app.emotes[app.focus].names().map(str::to_string).collect();
    emotes.extend(
        [
            "ffzX",
            "ffzY",
            "ffzW",
            "ffzCursed",
            "ffzRainbow",
            "ffzHyper",
        ]
        .map(String::from),
    );
    let mut seen = std::collections::HashSet::new();
    let mut users = Vec::new();
    for m in app.channels[app.focus].messages.iter().rev() {
        if seen.insert(m.user.to_lowercase()) {
            users.push(m.user.clone());
            if users.len() >= 200 {
                break;
            }
        }
    }
    Pools { emotes, users }
}

/// Join-mode pool: every channel token we know (open tabs + saved config),
/// in joinable form (`name` / `kick:name`).
fn join_candidates(app: &App) -> Pools {
    let mut seen = std::collections::HashSet::new();
    let mut emotes = Vec::new(); // reused slot — these are channel tokens
    let saved = config::load().channels;
    let open = app.channels.iter().map(|c| (c.platform, c.name.clone()));
    for (platform, name) in open.chain(saved) {
        let tok = match platform {
            Platform::Twitch => name,
            Platform::Kick => format!("kick:{name}"),
        };
        if seen.insert(tok.to_lowercase()) {
            emotes.push(tok);
        }
    }
    Pools {
        emotes,
        users: Vec::new(),
    }
}

/// rank pool entries against the word under the cursor. `@word` completes
/// usernames (mention form); otherwise emotes rank prefix → substring, then
/// chatters by prefix. everything case-insensitive, capped to keep cycling sane.
fn filter_candidates(pools: Pools, word: &str) -> Vec<String> {
    const CAP: usize = 60;
    let mut out = Vec::new();
    if let Some(rest) = word.strip_prefix('@') {
        let rl = rest.to_lowercase();
        out.extend(
            pools
                .users
                .into_iter()
                .filter(|u| u.to_lowercase().starts_with(&rl))
                .map(|u| format!("@{u}")),
        );
        out.truncate(CAP);
        return out;
    }
    let wl = word.to_lowercase();
    let mut prefix: Vec<String> = Vec::new();
    let mut sub: Vec<String> = Vec::new();
    for e in pools.emotes {
        let el = e.to_lowercase();
        if el.starts_with(&wl) {
            prefix.push(e);
        } else if el.contains(&wl) {
            sub.push(e);
        }
    }
    prefix.sort_unstable_by_key(|a| a.to_lowercase());
    sub.sort_unstable_by_key(|a| a.to_lowercase());
    out.extend(prefix);
    out.extend(sub);
    out.extend(
        pools
            .users
            .into_iter()
            .filter(|u| u.to_lowercase().starts_with(&wl)),
    );
    out.truncate(CAP);
    out
}

fn next_tab(app: &mut App) {
    if app.focus + 1 < app.channels.len() {
        app.focus += 1;
        app.scroll = 0;
    }
}

fn prev_tab(app: &mut App) {
    if app.focus > 0 {
        app.focus -= 1;
        app.scroll = 0;
    }
}

/// open a new channel tab from the Join input (`name` or `kick:name`). subscribes
/// over the live WS immediately; the emote set loads off-thread (never blocks).
fn join_channel(app: &mut App) {
    let tok = app.composer.take();
    app.mode = InputMode::Normal;
    app.composer.clear();
    if tok.is_empty() {
        return;
    }
    let (platform, name) = parse_sub(&tok);
    if let Some(i) = app
        .channels
        .iter()
        .position(|c| c.platform == platform && c.name.eq_ignore_ascii_case(&name))
    {
        app.focus = i; // already open → just switch to it
        return;
    }
    if let Some(out) = &app.out {
        let _ = out.send(net::Outbound::Join {
            platform,
            channel: name.clone(),
        });
    }
    app.channels.push(Channel::new(&name, platform, 200));
    app.emotes.push(EmoteSet::new()); // populated async — see spawn_emote_fetch
    spawn_emote_fetch(&app.emote_tx, platform, name);
    app.focus = app.channels.len() - 1;
    app.scroll = 0;
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
        let _ = out.send(net::Outbound::Part {
            platform,
            channel: name,
        });
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
fn close_channel(app: &mut App) {
    if app.channels.is_empty() {
        return;
    }
    let (platform, name) = {
        let c = &app.channels[app.focus];
        (c.platform, c.name.clone())
    };
    if let Some(out) = &app.out {
        let _ = out.send(net::Outbound::Part {
            platform,
            channel: name,
        });
    }
    app.channels.remove(app.focus);
    app.emotes.remove(app.focus);
    app.focus = app.focus.min(app.channels.len().saturating_sub(1));
    app.scroll = 0;
    save_state(app);
}

/// send the current input line to the focused channel (kick only; twitch has no
/// send path). clears the input on a successful enqueue.
fn send_focused(app: &mut App) {
    let text = app.composer.text.trim().to_string();
    if text.is_empty() || app.channels.is_empty() {
        return;
    }
    let (platform, name) = {
        let c = &app.channels[app.focus];
        (c.platform, c.name.clone())
    };
    match platform {
        Platform::Twitch => match &app.twitch_tx {
            Some(tx) => {
                let _ = tx.send((name.clone(), text));
                app.composer.clear();
                app.status = Some(format!("sent → {name}"));
            }
            None => app.status = Some("no twitch token — run: heatsync login".into()),
        },
        // prefer direct kick send (own token); fall back to the ext-relay path.
        Platform::Kick => {
            if let Some(ktx) = &app.kick_tx {
                let _ = ktx.send((name.clone(), text));
                app.composer.clear();
                app.status = Some(format!("sent → {name}"));
            } else if let Some(out) = &app.out {
                let _ = out.send(net::Outbound::Chat {
                    platform: Platform::Kick,
                    channel: name.clone(),
                    text,
                });
                app.composer.clear();
                app.status = Some(format!("sent → {name} (via ext)"));
            } else {
                app.status = Some("no kick auth — run: heatsync login kick".into());
            }
        }
    }
}

/// pull new data into the channel buffers for this tick, and keep the emote
/// image cache warm for what's on screen.
fn advance(app: &mut App) -> bool {
    let App {
        channels,
        emotes,
        feed,
        store,
        fb,
        status,
        mode,
        composer,
        focus,
        ..
    } = app;
    match feed {
        Feed::Mock(driver) => driver.tick(channels),
        Feed::Live {
            rx,
            start,
            connected,
        } => {
            let now = start.elapsed().as_millis() as u64;
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    ChatEvent::Line(l) => {
                        if let Some(ch) = channels.iter_mut().find(|c| {
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
                each_stack(&m.text, set, |key| f(key));
            }
        }
        // the composer's wysiwyg preview needs its emotes loading as you type.
        if *mode == InputMode::Insert {
            if let Some(set) = emotes.get(*focus) {
                each_stack(&composer.text, set, |key| f(key));
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
    // composer chrome above the footer: a completion bar while cycling, and a
    // wysiwyg preview strip while the message contains emotes. both are 0-high
    // (invisible) otherwise — the chat area gets every spare row back.
    let editing = matches!(app.mode, InputMode::Insert | InputMode::Join);
    let comp_h = (editing && app.composer.comp.is_some()) as u16;
    let prev_h = if app.mode == InputMode::Insert
        && !app.channels.is_empty()
        && segments(&app.composer.text, &app.emotes[app.focus])
            .iter()
            .any(|s| matches!(s, Segment::Stack(_)))
    {
        EMOTE_H
    } else {
        0
    };
    let [main, comp_row, prev_row, footer] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(comp_h),
        Constraint::Length(prev_h),
        Constraint::Length(1),
    ])
    .areas(f.area());
    if comp_h > 0 {
        draw_completion(f, comp_row, app);
    }
    if prev_h > 0 {
        draw_preview(f, prev_row, app, mode);
    }

    if app.mode == InputMode::Manage {
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

/// the channel tab bar — horizontal row (top/bottom) or vertical list (left/right).
fn draw_tabs(f: &mut Frame, area: Rect, app: &App) {
    let tab_style = |i: usize, heat: f64| {
        if i == app.focus {
            Style::default()
                .fg(Color::Black)
                .bg(BRAND)
                .add_modifier(Modifier::BOLD)
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
    let [barrow, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    f.render_widget(
        Paragraph::new(heat_bar(ch.heat, barrow.width as usize)),
        barrow,
    );
    if body.height == 0 || body.width == 0 {
        return;
    }

    let cap = body.height as usize;
    // plan visible messages newest-first, honouring per-message height + scroll.
    let mut plan: Vec<(&Message, usize)> = Vec::new();
    let mut used = 0usize;
    for m in ch.messages.iter().rev().skip(app.scroll) {
        let h = if has_ready_emote(m, set, mode) {
            EMOTE_H as usize
        } else {
            1
        };
        if used + h > cap {
            break;
        }
        used += h;
        plan.push((m, h));
    }
    plan.reverse();

    let mut y = body.y + (cap - used) as u16; // bottom-anchor
    for (m, h) in plan {
        let text_row = y + (h as u16 - 1);
        let (line, places) = layout_message(m, set, mode, body.width);
        f.render_widget(
            Paragraph::new(line),
            Rect {
                x: body.x,
                y: text_row,
                width: body.width,
                height: 1,
            },
        );
        draw_places(f, body.x, y, h as u16, &places, mode);
        y += h as u16;
    }
}

/// paint the reserved emote slots of one laid-out row (whatever tier is active).
fn draw_places(f: &mut Frame, x0: u16, y: u16, h: u16, places: &[Place], mode: EmoteMode) {
    for p in places {
        let x = x0 + p.col;
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

/// the tab-completion bar: candidates in a row, the pick inverted. the window
/// slides so the pick is always visible.
fn draw_completion(f: &mut Frame, area: Rect, app: &App) {
    let Some(c) = app.composer.comp.as_ref() else {
        return;
    };
    let w = area.width as usize;
    let width_of = |s: &str| UnicodeWidthStr::width(s) + 2;
    let mut first = 0;
    while first < c.idx
        && c.items[first..=c.idx]
            .iter()
            .map(|s| width_of(s))
            .sum::<usize>()
            + 1
            > w
    {
        first += 1;
    }
    let mut spans = vec![Span::raw(" ")];
    let mut used = 1usize;
    for (i, item) in c.items.iter().enumerate().skip(first) {
        let iw = width_of(item);
        if used + iw > w {
            break;
        }
        used += iw;
        let style = if i == c.idx {
            Style::default()
                .fg(Color::Black)
                .bg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Indexed(246))
        };
        spans.push(Span::styled(item.clone(), style));
        spans.push(Span::raw("  "));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// wysiwyg preview strip: the composed message exactly as it will land in chat —
/// emote images, overlay stacks, effects, the lot.
fn draw_preview(f: &mut Frame, area: Rect, app: &App, mode: EmoteMode) {
    let set = &app.emotes[app.focus];
    let lead = vec![Span::styled(" ❯ ", Style::default().fg(BRAND))];
    let (line, places) = layout_segments(
        &app.composer.text,
        set,
        mode,
        area.width,
        lead,
        3,
        Color::Indexed(250),
    );
    let text_row = area.y + area.height.saturating_sub(1);
    f.render_widget(
        Paragraph::new(line),
        Rect {
            x: area.x,
            y: text_row,
            width: area.width,
            height: 1,
        },
    );
    draw_places(f, area.x, area.y, area.height, &places, mode);
}

/// the composer text with a real cursor: text before, the char under the cursor
/// inverted (a block when at the end), text after.
fn caret_spans(cmp: &Composer) -> Vec<Span<'static>> {
    let fg = Style::default().fg(Color::Indexed(231));
    let (head, rest) = cmp.text.split_at(cmp.cur);
    let mut v = vec![Span::styled(head.to_string(), fg)];
    let mut it = rest.chars();
    match it.next() {
        Some(c) => {
            v.push(Span::styled(
                c.to_string(),
                fg.add_modifier(Modifier::REVERSED),
            ));
            v.push(Span::styled(it.as_str().to_string(), fg));
        }
        None => v.push(Span::styled("\u{2588}", Style::default().fg(BRAND))),
    }
    v
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
                    .bg(BRAND)
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
                .bg(BRAND)
                .add_modifier(Modifier::BOLD)
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

/// walk a message's emote STACK KEYS in order (base + overlays + effects —
/// core's segment parser owns the grammar). shared by layout, prefetch, height.
fn each_stack(text: &str, set: &EmoteSet, mut f: impl FnMut(&str)) {
    for seg in segments(text, set) {
        if let Segment::Stack(s) = seg {
            f(&s.key());
        }
    }
}

/// does this message contain at least one loaded (ready-to-draw) emote stack?
fn has_ready_emote(m: &Message, set: &EmoteSet, mode: EmoteMode) -> bool {
    let mut any = false;
    each_stack(&m.text, set, |key| any |= mode.ready(key));
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

/// a reserved emote slot: column offset within the channel body, its stack key,
/// and its cell width (wide stacks take double).
struct Place {
    col: u16,
    w: u16,
    key: String,
}

/// lay message text after some lead spans: text spans (with blank cells where
/// ready emote stacks go) plus the reserved emote slots. core's segment parser
/// owns stacking + effects. an unloaded/text-mode emote falls back to its name
/// in brand color. shared by chat rows and the composer's wysiwyg preview.
fn layout_segments(
    text: &str,
    set: &EmoteSet,
    mode: EmoteMode,
    maxw: u16,
    mut spans: Vec<Span<'static>>,
    mut col: u16,
    text_hue: Color,
) -> (Line<'static>, Vec<Place>) {
    let mut places = Vec::new();
    for seg in segments(text, set) {
        if col >= maxw {
            break;
        }
        match seg {
            Segment::Text(t) => {
                col += UnicodeWidthStr::width(t.as_str()) as u16;
                spans.push(Span::styled(t, Style::default().fg(text_hue)));
            }
            Segment::Stack(s) => {
                let key = s.key();
                let w = key_width(&key);
                if col + w <= maxw && mode.ready(&key) {
                    places.push(Place { col, w, key });
                    spans.push(Span::raw(" ".repeat(w as usize)));
                    col += w;
                } else {
                    col += UnicodeWidthStr::width(s.base.as_str()) as u16;
                    spans.push(Span::styled(
                        s.base,
                        Style::default().fg(BRAND).add_modifier(Modifier::BOLD),
                    ));
                }
            }
        }
    }
    (Line::from(spans), places)
}

/// one chat row: `user: message` with emote slots.
fn layout_message(
    m: &Message,
    set: &EmoteSet,
    mode: EmoteMode,
    maxw: u16,
) -> (Line<'static>, Vec<Place>) {
    let text_hue = Color::Indexed(heatsync_core::heat::color(m.heat));
    let user_color = m
        .color
        .as_deref()
        .and_then(parse_hex)
        .unwrap_or(Color::Indexed(244));
    let spans = vec![
        Span::styled(m.user.clone(), Style::default().fg(user_color)),
        Span::styled(": ", Style::default().fg(Color::Indexed(238))),
    ];
    let col = UnicodeWidthStr::width(m.user.as_str()) as u16 + 2;
    layout_segments(&m.text, set, mode, maxw, spans, col, text_hue)
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
        Span::styled(
            "\u{2591}".repeat(width - filled),
            Style::default().fg(Color::Indexed(236)),
        ),
    ])
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App, n: usize) {
    // Manage mode → rover-style key hints.
    if app.mode == InputMode::Manage {
        let hint = |k: &'static str, d: &'static str| {
            [
                Span::styled(k, Style::default().fg(BRAND)),
                Span::styled(format!(" {d}  "), Style::default().fg(Color::Indexed(244))),
            ]
        };
        let mut spans = vec![
            Span::styled(
                " manage ",
                Style::default()
                    .fg(Color::Black)
                    .bg(BRAND)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
        ];
        for pair in [
            hint("jk", "move"),
            hint("enter", "open"),
            hint("a", "add"),
            hint("d", "leave"),
            hint("JK", "reorder"),
            hint("esc", "back"),
        ] {
            spans.extend(pair);
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }
    // Join mode → type a channel to open (tab completes known channels).
    if app.mode == InputMode::Join {
        let mut spans = vec![
            Span::styled(
                " join ",
                Style::default()
                    .fg(Color::Black)
                    .bg(BRAND)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ❯ ", Style::default().fg(BRAND)),
        ];
        spans.extend(caret_spans(&app.composer));
        spans.push(Span::styled(
            "   name or kick:name · tab complete",
            Style::default().fg(Color::Indexed(240)),
        ));
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
            Span::styled(
                prompt,
                Style::default()
                    .fg(Color::Black)
                    .bg(BRAND)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ❯ ", Style::default().fg(BRAND)),
        ];
        if readonly {
            spans.push(Span::styled(
                "read-only — no send token · esc",
                Style::default().fg(Color::Indexed(214)),
            ));
        } else {
            spans.extend(caret_spans(&app.composer));
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
    let nav = if app.tab_pos.is_vertical() {
        "jk"
    } else {
        "hl"
    };
    let mut spans = vec![
        Span::styled(
            " heatsync ",
            Style::default()
                .fg(Color::Black)
                .bg(BRAND)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  {nav} "), Style::default().fg(BRAND)),
        Span::raw("chan  "),
        Span::styled("i ", Style::default().fg(BRAND)),
        Span::raw("say  "),
        Span::styled("o ", Style::default().fg(BRAND)),
        Span::raw("join  "),
        Span::styled("m ", Style::default().fg(BRAND)),
        Span::raw("manage  "),
        Span::styled("T ", Style::default().fg(BRAND)),
        Span::raw("tabs  "),
        Span::styled("q ", Style::default().fg(BRAND)),
        Span::raw("quit  "),
    ];
    if app.paused {
        spans.push(Span::styled(
            "PAUSED  ",
            Style::default()
                .fg(Color::Indexed(214))
                .add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(s) = &app.status {
        spans.push(Span::styled(
            format!("{s}  "),
            Style::default().fg(Color::Indexed(214)),
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
mod composer_tests {
    use super::*;

    fn typed(s: &str) -> Composer {
        let mut c = Composer::default();
        for ch in s.chars() {
            c.insert(ch);
        }
        c
    }

    #[test]
    fn cursor_editing_is_utf8_safe() {
        let mut c = typed("héllo");
        c.left();
        c.left();
        c.left();
        c.backspace(); // removes é
        assert_eq!(c.text, "hllo");
        c.insert('é');
        assert_eq!(c.text, "héllo");
        c.home();
        c.delete();
        assert_eq!(c.text, "éllo");
        c.end();
        assert_eq!(c.cur, c.text.len());
    }

    #[test]
    fn kill_word_eats_word_and_spaces() {
        let mut c = typed("send this msg  ");
        c.kill_word();
        assert_eq!(c.text, "send this ");
        c.kill_word();
        assert_eq!(c.text, "send ");
    }

    #[test]
    fn kill_to_start_respects_cursor() {
        let mut c = typed("abc def");
        c.left();
        c.left();
        c.left();
        c.kill_to_start();
        assert_eq!(c.text, "def");
        assert_eq!(c.cur, 0);
    }

    #[test]
    fn completion_cycles_and_wraps() {
        let mut c = typed("hi kek");
        let items = || vec!["KEKW".to_string(), "KEKWait".to_string()];
        c.complete(false, |w| {
            assert_eq!(w, "kek");
            items()
        });
        assert_eq!(c.text, "hi KEKW");
        c.complete(false, |_| unreachable!("cycling reuses items"));
        assert_eq!(c.text, "hi KEKWait");
        c.complete(false, |_| unreachable!());
        assert_eq!(c.text, "hi KEKW"); // wrapped
        c.complete(true, |_| unreachable!());
        assert_eq!(c.text, "hi KEKWait"); // backtab
        c.insert('!');
        assert!(c.comp.is_none(), "typing ends the cycle");
        assert_eq!(c.text, "hi KEKWait!");
    }

    #[test]
    fn completion_replaces_only_the_word() {
        let mut c = typed("yo pog hype");
        c.left();
        c.left();
        c.left();
        c.left();
        c.left(); // cursor after "pog"
        c.complete(false, |w| {
            assert_eq!(w, "pog");
            vec!["POGGERS".into()]
        });
        assert_eq!(c.text, "yo POGGERS hype");
    }

    #[test]
    fn empty_word_or_no_candidates_is_a_noop() {
        let mut c = typed("x ");
        c.complete(false, |_| vec!["nope".into()]);
        assert_eq!(c.text, "x ");
        let mut c = typed("zz");
        c.complete(false, |_| Vec::new());
        assert_eq!(c.text, "zz");
        assert!(c.comp.is_none());
    }

    #[test]
    fn at_prefix_completes_users() {
        let pools = Pools {
            emotes: vec!["Clap".into()],
            users: vec!["Clara".into(), "bob".into()],
        };
        assert_eq!(filter_candidates(pools, "@cl"), vec!["@Clara"]);
    }

    #[test]
    fn ranking_prefix_then_substring_then_users() {
        let pools = Pools {
            emotes: vec!["xKEKW".into(), "KEKW".into(), "KEKWait".into()],
            users: vec!["kekuser".into()],
        };
        assert_eq!(
            filter_candidates(pools, "kek"),
            vec!["KEKW", "KEKWait", "xKEKW", "kekuser"]
        );
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
