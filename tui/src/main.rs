//! heatsync tui — heat-sorted live multichat. `heatsync [channels…]` opens the
//! grid (default demo set); `--mock` runs the offline synthetic feed; `log`,
//! `search`, `hot` are headless corpus subcommands.
//! keys: q quit · j/k scroll focused · 1-9 focus · tab cycle · space pause

#[allow(dead_code)]
mod cli;
mod composer;
mod config;
mod emote;
mod http;
mod kick;
mod net;
mod twitch;

use std::io;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use composer::{
    colon_query, emoji_pool, Badge, Candidate, Composer, LineMode, Outcome, Trigger, VKey,
};
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
use ratatui::widgets::{Clear, Paragraph};
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
    me: Option<String>,   // own username (lowercase) — drives @mention tinting
    // twitch sender feedback (auth failures, NOTICE rejections) → status line.
    twitch_notes: Option<Receiver<String>>,
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
            me: None,
            twitch_notes: None,
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
    // bound the on-disk image cache (best-effort, off the startup path).
    std::thread::spawn(http::sweep_cache);
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
    let me = auth.twitch_user.as_deref().map(str::to_lowercase);
    let (twitch_tx, twitch_notes) = match (auth.twitch_user, auth.twitch_oauth) {
        (Some(u), Some(o)) => {
            let (tx, notes) = twitch::spawn(u, o);
            (Some(tx), Some(notes))
        }
        _ => (None, None),
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
        me,
        twitch_notes,
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
    // long enough that a load storm (cold cache) coalesces into few re-emits —
    // every full re-emit re-sends every sixel on screen, which reads as flash.
    let repaint_debounce = Duration::from_millis(800);
    let sync_ok = app.store.as_ref().is_none_or(EmoteStore::sync_updates_ok);

    loop {
        drain_emotes(&mut app);
        // the focused channel is on screen — whatever it holds is now seen.
        if let Some(ch) = app.channels.get_mut(app.focus) {
            ch.seen = ch.total;
        }
        // per-frame emote draw budget (decoupled from the data-tick pump).
        if let Some(store) = &app.store {
            store.reset_budget();
        }
        // DEC 2026 synchronized update: the whole frame — text diff, image
        // escapes, fb blit — lands atomically on terminals that support it
        // (foot, kitty, wezterm, WT), killing scroll tear. unknown-mode-safe
        // everywhere else. ratatui never emits this itself.
        {
            use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
            use crossterm::ExecutableCommand;
            if sync_ok {
                let _ = io::stdout().execute(BeginSynchronizedUpdate);
            }
            terminal.draw(|f| ui(f, &app))?;
            if pending_repaint && last_repaint.elapsed() >= repaint_debounce {
                // discard the diff baseline so the NEXT draw re-sends every cell,
                // landing the sixel foot dropped — no clear, no flash.
                terminal.swap_buffers();
                last_repaint = Instant::now();
                pending_repaint = false;
            }
            // console tier: paint emote pixels onto the reserved cells now that
            // the text has flushed. terminal tiers draw inline in the frame above.
            if let Some(fb) = &app.fb {
                fb.blit();
            }
            if sync_ok {
                let _ = io::stdout().execute(EndSynchronizedUpdate);
            }
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
        InputMode::Insert => match app.composer.mode {
            LineMode::Insert => insert_mode_key(app, k),
            _ => {
                if let Some(v) = to_vkey(k) {
                    match app.composer.vi_key(v) {
                        Outcome::ExitComposer => app.mode = InputMode::Normal,
                        Outcome::Send => send_focused(app),
                        Outcome::Stay => {}
                    }
                }
            }
        },
        InputMode::Join => match k.code {
            KeyCode::Esc => {
                if app.composer.comp.is_some() {
                    app.composer.close_session();
                } else {
                    app.mode = InputMode::Normal;
                    app.composer.clear();
                }
            }
            KeyCode::Enter => {
                if app.composer.comp.is_some() {
                    app.composer.accept();
                } else {
                    join_channel(app);
                }
            }
            KeyCode::Tab | KeyCode::Down if app.composer.comp.is_some() => {
                app.composer.session_next(false)
            }
            KeyCode::BackTab | KeyCode::Up if app.composer.comp.is_some() => {
                app.composer.session_next(true)
            }
            KeyCode::Tab => open_channel_session(app),
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
                    app.composer.enter_insert();
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

/// crossterm key → the vi layer's key vocabulary.
fn to_vkey(k: crossterm::event::KeyEvent) -> Option<VKey> {
    if k.modifiers.contains(KeyModifiers::CONTROL) {
        return match k.code {
            KeyCode::Char('r') => Some(VKey::CtrlR),
            _ => None,
        };
    }
    Some(match k.code {
        KeyCode::Char(c) => VKey::Char(c),
        KeyCode::Esc => VKey::Esc,
        KeyCode::Enter => VKey::Enter,
        KeyCode::Left => VKey::Left,
        KeyCode::Right => VKey::Right,
        KeyCode::Home => VKey::Home,
        KeyCode::End => VKey::End,
        _ => return None,
    })
}

/// Insert-mode (typing) key table: session navigation when the dropdown is
/// open, else edit + the colon auto-open hook.
fn insert_mode_key(app: &mut App, k: crossterm::event::KeyEvent) {
    let open = app.composer.comp.is_some();
    match k.code {
        KeyCode::Esc if open => app.composer.close_session(),
        KeyCode::Esc => app.composer.enter_normal(),
        KeyCode::Enter if open => app.composer.accept(),
        KeyCode::Enter => send_focused(app),
        KeyCode::Tab | KeyCode::Down if open => app.composer.session_next(false),
        KeyCode::BackTab | KeyCode::Up if open => app.composer.session_next(true),
        KeyCode::Tab => open_insert_session(app),
        _ => {
            edit_key(&mut app.composer, k);
            // colon auto-open: typing landed us inside `:xy…` with no session
            if app.composer.comp.is_none() && matches!(k.code, KeyCode::Char(_)) {
                if let Some((anchor, q)) = colon_query(&app.composer.text, app.composer.cur) {
                    if q.chars().count() >= 2 {
                        let pool = colon_pool(app);
                        app.composer
                            .open_session(Trigger::Colon, anchor, pool, false);
                    }
                }
            }
        }
    }
}

/// recent chatters of the focused channel, newest first, deduped.
fn chatter_pool(app: &App, mention: bool) -> Vec<Candidate> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for m in app.channels[app.focus].messages.iter().rev() {
        if seen.insert(m.user.to_lowercase()) {
            out.push(Candidate {
                insert: if mention {
                    format!("@{}", m.user)
                } else {
                    m.user.clone()
                },
                label: m.user.clone(),
                matchkey: m.user.clone(),
                badge: Badge::User,
            });
            if out.len() >= 200 {
                break;
            }
        }
    }
    out
}

/// word pool: focused channel's emotes (+ ffz effect words) + chatters.
fn word_pool(app: &App) -> Vec<Candidate> {
    let mut pool: Vec<Candidate> = app.emotes[app.focus]
        .iter()
        .map(|e| Candidate {
            insert: e.name.clone(),
            label: e.name.clone(),
            matchkey: e.name.clone(),
            badge: Badge::from_provider(&e.provider),
        })
        .collect();
    pool.extend(
        [
            "ffzX",
            "ffzY",
            "ffzW",
            "ffzCursed",
            "ffzRainbow",
            "ffzHyper",
        ]
        .map(|w| Candidate::plain(w, Badge::Ffz)),
    );
    pool.extend(chatter_pool(app, false));
    pool
}

/// colon pool: emoji shortcodes + the channel's emotes (accepting an emote
/// from a colon session strips the `:`).
fn colon_pool(app: &App) -> Vec<Candidate> {
    let mut pool: Vec<Candidate> = emoji_pool()
        .iter()
        .map(|(code, glyph)| Candidate {
            insert: (*glyph).to_string(),
            label: format!("{glyph} :{code}:"),
            matchkey: code.clone(),
            badge: Badge::Emoji,
        })
        .collect();
    if !app.channels.is_empty() {
        pool.extend(app.emotes[app.focus].iter().map(|e| Candidate {
            insert: e.name.clone(),
            label: e.name.clone(),
            matchkey: e.name.clone(),
            badge: Badge::from_provider(&e.provider),
        }));
    }
    pool
}

/// Tab in Insert mode: pick trigger from the word's sigil and open.
fn open_insert_session(app: &mut App) {
    if app.channels.is_empty() {
        return;
    }
    let (start, word) = {
        let (s, w) = app.composer.word_at_cursor();
        (s, w.to_string())
    };
    if word.is_empty() {
        return;
    }
    let (trigger, pool) = if word.starts_with('@') {
        (Trigger::Mention, chatter_pool(app, true))
    } else if word.starts_with(':') {
        (Trigger::Colon, colon_pool(app))
    } else {
        (Trigger::Word, word_pool(app))
    };
    app.composer.open_session(trigger, start, pool, true);
}

/// Tab in Join mode: every channel token we know (open tabs + saved config).
fn open_channel_session(app: &mut App) {
    let (start, word) = {
        let (s, w) = app.composer.word_at_cursor();
        (s, w.to_string())
    };
    if word.is_empty() {
        return;
    }
    let mut seen = std::collections::HashSet::new();
    let mut pool = Vec::new();
    let saved = config::load().channels;
    let open = app.channels.iter().map(|c| (c.platform, c.name.clone()));
    for (platform, name) in open.chain(saved) {
        let tok = match platform {
            Platform::Twitch => name,
            Platform::Kick => format!("kick:{name}"),
        };
        if seen.insert(tok.to_lowercase()) {
            pool.push(Candidate::plain(tok, Badge::Chan));
        }
    }
    app.composer
        .open_session(Trigger::Channel, start, pool, true);
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
        twitch_notes,
        ..
    } = app;
    // twitch sender feedback beats the optimistic "sent →" note.
    if let Some(rx) = twitch_notes {
        while let Ok(note) = rx.try_recv() {
            *status = Some(note);
        }
    }
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
    // wysiwyg preview strip while the message contains emotes — 0-high else.
    // the completion dropdown is an OVERLAY (never a carve): the chat layout
    // is byte-identical whether it's open or not, so nothing ever jumps.
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
    let [main, prev_row, footer] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(prev_h),
        Constraint::Length(1),
    ])
    .areas(f.area());
    // the dropdown rect is computed up front: emote pixel placements that
    // intersect it must be suppressed (images ignore ratatui's cell diff).
    let popup = dropdown_rect(app, main, prev_row, footer);

    if prev_h > 0 {
        draw_preview(f, prev_row, app, mode, popup);
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
        if let Some(p) = popup {
            draw_dropdown(f, p, app);
        }
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
    draw_active(f, chat, app, mode, popup);
    draw_footer(f, footer, app, app.channels.len());
    if let Some(p) = popup {
        draw_dropdown(f, p, app);
    }
}

/// where the completion dropdown goes: anchored at the query's screen column,
/// growing upward from just above the preview strip / footer. None = closed.
fn dropdown_rect(app: &App, main: Rect, prev_row: Rect, footer: Rect) -> Option<Rect> {
    if !matches!(app.mode, InputMode::Insert | InputMode::Join) {
        return None;
    }
    let s = app.composer.comp.as_ref()?;
    if s.filtered.is_empty() {
        return None;
    }
    let rows = s.filtered.len().min(8) as u16 + 1; // +1 counter row
    let bottom = if prev_row.height > 0 {
        prev_row.y
    } else {
        footer.y
    };
    let h = rows.min(bottom.saturating_sub(main.y));
    if h == 0 {
        return None;
    }
    let y = bottom - h;
    // x = footer chrome width + rendered width of the text before the anchor
    let prefix = footer_prefix_width(app);
    let head_w = UnicodeWidthStr::width(&app.composer.text[..s.anchor]) as u16;
    let label_w = s
        .filtered
        .iter()
        .take(8)
        .map(|&i| {
            UnicodeWidthStr::width(s.pool[i].label.as_str()) + s.pool[i].badge.label().len() + 3
        })
        .max()
        .unwrap_or(0) as u16;
    let w = label_w.clamp(24, main.width.max(24));
    let x = (prefix + head_w).min(main.width.saturating_sub(w));
    Some(Rect {
        x,
        y,
        width: w,
        height: h,
    })
}

/// the channel tab bar — horizontal row (top/bottom) or vertical list (left/right).
fn draw_tabs(f: &mut Frame, area: Rect, app: &App) {
    // ext-parity tab states: current = white block, unread = white text,
    // nothing new = 808080 gray. (heat still shows as the number in the label.)
    let tab_style = |i: usize, ch: &Channel| {
        if i == app.focus {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Indexed(231))
                .add_modifier(Modifier::BOLD)
        } else if ch.total > ch.seen {
            Style::default().fg(Color::Indexed(231))
        } else {
            Style::default().fg(Color::Indexed(244))
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
                Paragraph::new(Line::from(Span::styled(label, tab_style(i, ch)))),
                row,
            );
        }
    } else {
        let mut spans = Vec::new();
        for (i, ch) in app.channels.iter().enumerate() {
            spans.push(Span::styled(
                format!(" {}·{} {:.0} ", ch.name, ch.platform.tag(), ch.heat),
                tab_style(i, ch),
            ));
            spans.push(Span::raw(" "));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}

/// the active channel: a heat bar, then its chat. messages with a ready emote
/// occupy EMOTE_H rows (text on the bottom row, emote painted across the block),
/// so emotes render big without colliding with neighbouring lines. bottom-anchored.
fn draw_active(f: &mut Frame, area: Rect, app: &App, mode: EmoteMode, mask: Option<Rect>) {
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

    let scrolled = app.scroll > 0;
    // when scrolled up, the bottom row becomes the "newer messages" bar.
    let cap = (body.height as usize).saturating_sub(scrolled as usize);
    let me = app.me.as_deref();
    // plan visible messages newest-first: full wrapped layout per message,
    // heights are the sum of its row heights. bottom-anchored.
    let mut plan: Vec<Vec<RowL>> = Vec::new();
    let mut used = 0usize;
    for m in ch.messages.iter().rev().skip(app.scroll) {
        let rows = layout_message(m, set, mode, body.width, me);
        let mh: usize = rows.iter().map(|r| r.h as usize).sum();
        if used + mh > cap {
            break;
        }
        used += mh;
        plan.push(rows);
    }
    plan.reverse();

    let mut y = body.y + (cap - used) as u16; // bottom-anchor
    for rows in plan {
        for row in rows {
            let text_row = y + (row.h - 1);
            f.render_widget(
                Paragraph::new(row.line),
                Rect {
                    x: body.x,
                    y: text_row,
                    width: body.width,
                    height: 1,
                },
            );
            draw_places(f, body.x, y, row.h, &row.places, mode, mask);
            y += row.h;
        }
    }

    if scrolled {
        let bar = Line::from(vec![
            Span::styled(
                format!(" ↓ {} newer ", app.scroll),
                Style::default()
                    .fg(Color::Indexed(214))
                    .bg(Color::Indexed(235)),
            ),
            Span::styled(
                "· G latest",
                Style::default()
                    .fg(Color::Indexed(244))
                    .bg(Color::Indexed(235)),
            ),
        ]);
        f.render_widget(
            Paragraph::new(bar).style(Style::default().bg(Color::Indexed(235))),
            Rect {
                x: body.x,
                y: body.y + body.height - 1,
                width: body.width,
                height: 1,
            },
        );
    }
}

/// paint the reserved emote slots of one laid-out row (whatever tier is
/// active). `mask` = the dropdown rect: image pixels don't respect ratatui's
/// cell diff, so any placement that would sit under the popup is suppressed
/// (the row falls back to its text-name form for the popup's lifetime).
fn draw_places(
    f: &mut Frame,
    x0: u16,
    y: u16,
    h: u16,
    places: &[Place],
    mode: EmoteMode,
    mask: Option<Rect>,
) {
    for p in places {
        let x = x0 + p.col;
        let slot = Rect {
            x,
            y,
            width: p.w,
            height: h,
        };
        if mask.is_some_and(|m| m.intersects(slot)) {
            continue;
        }
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

/// the completion dropdown: an overlay popup growing upward from the composer.
/// candidates top-to-bottom in rank order, pick inverted, counter row at the
/// bottom edge. the window slides so the pick stays visible.
fn draw_dropdown(f: &mut Frame, area: Rect, app: &App) {
    let Some(s) = app.composer.comp.as_ref() else {
        return;
    };
    f.render_widget(Clear, area);
    let visible = (area.height - 1) as usize;
    let first = s
        .sel
        .saturating_sub(visible.saturating_sub(1))
        .min(s.filtered.len().saturating_sub(visible));
    let bg = Style::default().bg(Color::Indexed(235));
    for (row, idx) in (first..s.filtered.len().min(first + visible)).enumerate() {
        let c = &s.pool[s.filtered[idx]];
        let sel = idx == s.sel;
        let (label_st, badge_st) = if sel {
            let st = Style::default()
                .fg(Color::Black)
                .bg(Color::White)
                .add_modifier(Modifier::BOLD);
            (st, st)
        } else {
            (bg.fg(Color::Indexed(250)), bg.fg(Color::Indexed(244)))
        };
        let lw = UnicodeWidthStr::width(c.label.as_str());
        let badge = c.badge.label();
        let pad = (area.width as usize).saturating_sub(lw + badge.len() + 3);
        let line = Line::from(vec![
            Span::styled(format!(" {}", c.label), label_st),
            Span::styled(" ".repeat(pad + 1), if sel { label_st } else { bg }),
            Span::styled(format!("{badge} "), badge_st),
        ]);
        f.render_widget(
            Paragraph::new(line),
            Rect {
                x: area.x,
                y: area.y + row as u16,
                width: area.width,
                height: 1,
            },
        );
    }
    // counter row hugs the composer
    let counter = format!("{}/{} ", s.sel + 1, s.filtered.len());
    let pad = (area.width as usize).saturating_sub(counter.len());
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" ".repeat(pad), bg),
            Span::styled(counter, bg.fg(Color::Indexed(244))),
        ])),
        Rect {
            x: area.x,
            y: area.y + area.height - 1,
            width: area.width,
            height: 1,
        },
    );
}

/// wysiwyg preview strip: the composed message exactly as it will land in chat —
/// emote images, overlay stacks, effects, the lot.
fn draw_preview(f: &mut Frame, area: Rect, app: &App, mode: EmoteMode, mask: Option<Rect>) {
    let set = &app.emotes[app.focus];
    let lead = vec![Span::styled(" ❯ ", Style::default().fg(BRAND))];
    // the strip is one visual row — take the first wrapped row (long drafts
    // clip here; the chat row wraps for real once sent).
    let mut rows = wrap_layout(
        &app.composer.text,
        set,
        mode,
        area.width,
        LineCtx {
            lead,
            lead_col: 3,
            hue: Color::Indexed(250),
            tint: None,
        },
    );
    let RowL { line, places, .. } = rows.remove(0);
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
    draw_places(f, area.x, area.y, area.height, &places, mode, mask);
}

/// the composer text with a real cursor. insert: brand block / inverted char.
/// normal: white-inverted char under the cursor. visual: whole selection
/// inverted, cursor end bolded.
fn caret_spans(cmp: &Composer) -> Vec<Span<'static>> {
    let fg = Style::default().fg(Color::Indexed(231));
    if let Some((a, b)) = cmp.selection() {
        let sel = Style::default().fg(Color::Black).bg(Color::White);
        return vec![
            Span::styled(cmp.text[..a].to_string(), fg),
            Span::styled(cmp.text[a..b].to_string(), sel),
            Span::styled(cmp.text[b..].to_string(), fg),
        ];
    }
    let (head, rest) = cmp.text.split_at(cmp.cur);
    let mut v = vec![Span::styled(head.to_string(), fg)];
    let mut it = rest.chars();
    let cursor_style = match cmp.mode {
        LineMode::Normal => Style::default().fg(Color::Black).bg(Color::White),
        _ => fg.add_modifier(Modifier::REVERSED),
    };
    match it.next() {
        Some(c) => {
            v.push(Span::styled(c.to_string(), cursor_style));
            v.push(Span::styled(it.as_str().to_string(), fg));
        }
        None => v.push(Span::styled("\u{2588}", Style::default().fg(BRAND))),
    }
    v
}

/// display width of the footer chrome before the composer text starts — the
/// dropdown anchors its x to this. must mirror draw_footer's span layout.
fn footer_prefix_width(app: &App) -> u16 {
    match app.mode {
        InputMode::Join => (" join ".len() + " ❯ ".chars().count()) as u16,
        _ => {
            let ch = &app.channels[app.focus.min(app.channels.len().saturating_sub(1))];
            let prompt = format!(" {}·{} ", ch.name, ch.platform.tag());
            8 + UnicodeWidthStr::width(prompt.as_str()) as u16 + 3 // chip + prompt + " ❯ "
        }
    }
}

/// the composer mode chip shown in the footer.
fn mode_chip(cmp: &Composer) -> Span<'static> {
    match cmp.mode {
        LineMode::Insert => Span::styled(
            " insert ",
            Style::default()
                .fg(Color::Black)
                .bg(BRAND)
                .add_modifier(Modifier::BOLD),
        ),
        LineMode::Normal => Span::styled(
            " normal ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        LineMode::Visual { .. } => Span::styled(
            " visual ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Indexed(226))
                .add_modifier(Modifier::BOLD),
        ),
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

/// one laid-out visual row of a message: spans, height (EMOTE_H when a stack
/// sits on it on a graphics tier), and its emote placements.
struct RowL {
    line: Line<'static>,
    h: u16,
    places: Vec<Place>,
}

/// the wrap accumulator: content fills left-to-right, breaks at `maxw` with a
/// 2-cell hanging indent, hard-breaks words wider than a row, and reserves
/// EMOTE_H the moment a KNOWN stack lands on a row (stable heights — rows never
/// jump when the image finishes loading). `tint` paints mention rows edge-to-edge.
struct Wrap {
    rows: Vec<RowL>,
    spans: Vec<Span<'static>>,
    places: Vec<Place>,
    col: u16,
    row_start: u16,
    h: u16,
    maxw: u16,
    indent: u16,
    tint: Option<Color>,
}

impl Wrap {
    fn new(lead: Vec<Span<'static>>, lead_col: u16, maxw: u16, tint: Option<Color>) -> Wrap {
        let maxw = maxw.max(4);
        Wrap {
            rows: Vec::new(),
            spans: lead,
            places: Vec::new(),
            col: lead_col,
            row_start: lead_col,
            h: 1,
            maxw,
            indent: 2.min(maxw / 4),
            tint,
        }
    }

    fn at_start(&self) -> bool {
        self.col <= self.row_start
    }

    fn fits(&self, w: u16) -> bool {
        self.col + w <= self.maxw
    }

    fn flush_row(&mut self) {
        let mut spans = std::mem::take(&mut self.spans);
        if let Some(bg) = self.tint {
            for s in &mut spans {
                s.style = s.style.bg(bg);
            }
            let pad = self.maxw.saturating_sub(self.col);
            if pad > 0 {
                spans.push(Span::styled(
                    " ".repeat(pad as usize),
                    Style::default().bg(bg),
                ));
            }
        }
        self.rows.push(RowL {
            line: Line::from(spans),
            h: self.h,
            places: std::mem::take(&mut self.places),
        });
        self.h = 1;
    }

    fn newline(&mut self) {
        self.flush_row();
        if self.indent > 0 {
            self.spans.push(Span::raw(" ".repeat(self.indent as usize)));
        }
        self.col = self.indent;
        self.row_start = self.indent;
    }

    fn push_span(&mut self, s: impl Into<String>, style: Style) {
        let s: String = s.into();
        self.col += UnicodeWidthStr::width(s.as_str()) as u16;
        self.spans.push(Span::styled(s, style));
    }

    /// a word: wraps to the next row when it doesn't fit, hard-breaks when it
    /// is wider than a whole row (copypasta with no spaces can't overflow).
    fn push_word(&mut self, word: &str, style: Style) {
        let mut rest = word;
        loop {
            let avail = self.maxw.saturating_sub(self.col);
            let ww = UnicodeWidthStr::width(rest) as u16;
            if ww <= avail {
                self.push_span(rest, style);
                return;
            }
            if !self.at_start() {
                self.newline();
                continue;
            }
            // at row start and still too wide → split at the row edge
            let mut used = 0u16;
            let mut cut = rest.len();
            for (i, c) in rest.char_indices() {
                let cw = UnicodeWidthStr::width(c.to_string().as_str()) as u16;
                if used + cw > avail {
                    cut = i;
                    break;
                }
                used += cw;
            }
            if cut == 0 {
                return; // a zero-width row — bail rather than loop forever
            }
            let (head, tail) = rest.split_at(cut);
            self.push_span(head, style);
            self.newline();
            rest = tail;
        }
    }

    fn finish(mut self) -> Vec<RowL> {
        self.flush_row();
        self.rows
    }
}

/// per-line layout context: the lead spans (username chrome), where content
/// starts, the text color, and an optional full-row mention tint.
struct LineCtx {
    lead: Vec<Span<'static>>,
    lead_col: u16,
    hue: Color,
    tint: Option<Color>,
}

/// lay message text after some lead spans into wrapped rows. core's segment
/// parser owns stacking + effects; an unloaded/text-mode emote falls back to
/// its name in brand color. shared by chat rows and the wysiwyg preview.
fn wrap_layout(text: &str, set: &EmoteSet, mode: EmoteMode, maxw: u16, ctx: LineCtx) -> Vec<RowL> {
    let mut wl = Wrap::new(ctx.lead, ctx.lead_col, maxw, ctx.tint);
    let txt = Style::default().fg(ctx.hue);
    let name_style = Style::default().fg(BRAND).add_modifier(Modifier::BOLD);
    let graphics = !matches!(mode, EmoteMode::Text);
    for seg in segments(text, set) {
        match seg {
            Segment::Text(t) => {
                for (i, word) in t.split(' ').enumerate() {
                    if i > 0 && !wl.at_start() {
                        if !wl.fits(1 + UnicodeWidthStr::width(word) as u16) {
                            wl.newline();
                        } else {
                            wl.push_span(" ", txt);
                        }
                    }
                    wl.push_word(word, txt);
                }
            }
            Segment::Stack(s) => {
                let key = s.key();
                let w = key_width(&key);
                if graphics {
                    if !wl.fits(w) && !wl.at_start() {
                        wl.newline();
                    }
                    if wl.fits(w) {
                        // known stack → the row is emote-height NOW, ready or
                        // not (stable heights). unready draws the name inside
                        // the reserved block; the image takes over seamlessly.
                        wl.h = EMOTE_H;
                        if mode.ready(&key) {
                            wl.places.push(Place {
                                col: wl.col,
                                w,
                                key,
                            });
                            wl.push_span(" ".repeat(w as usize), Style::default());
                        } else {
                            wl.push_word(&s.base, name_style);
                        }
                        continue;
                    }
                }
                wl.push_word(&s.base, name_style);
            }
        }
    }
    wl.finish()
}

/// one chat row: `user: message` with emote slots.
fn layout_message(
    m: &Message,
    set: &EmoteSet,
    mode: EmoteMode,
    maxw: u16,
    me: Option<&str>,
) -> Vec<RowL> {
    // og irc framing: <nick> in dim angle brackets, message text plain white.
    // heat stays where it belongs — the bar and the tab numbers, not the prose.
    let text_hue = Color::Indexed(231);
    let user_color = m
        .color
        .as_deref()
        .and_then(parse_hex)
        .unwrap_or(Color::Indexed(244));
    let bracket = Style::default().fg(Color::Indexed(238));
    let spans = vec![
        Span::styled("<", bracket),
        Span::styled(m.user.clone(), Style::default().fg(user_color)),
        Span::styled("> ", bracket),
    ];
    let col = UnicodeWidthStr::width(m.user.as_str()) as u16 + 3;
    // mention tint: the whole row block goes dark red when the message @'s you.
    let tint = me
        .filter(|me| {
            let hay = m.text.to_lowercase();
            hay.contains(&format!("@{me}"))
        })
        .map(|_| Color::Indexed(52));
    wrap_layout(
        &m.text,
        set,
        mode,
        maxw,
        LineCtx {
            lead: spans,
            lead_col: col,
            hue: text_hue,
            tint,
        },
    )
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
            mode_chip(&app.composer),
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
        // send feedback must be visible WHILE composing — a rejection that only
        // shows after esc is a silent failure.
        if let Some(s) = &app.status {
            spans.push(Span::styled(
                format!("   {s}"),
                Style::default().fg(Color::Indexed(214)),
            ));
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
mod wrap_tests {
    use super::*;
    use heatsync_core::emote::Emote;

    fn set_of(names: &[&str]) -> EmoteSet {
        EmoteSet::from_list(names.iter().map(|n| Emote {
            name: (*n).into(),
            url: format!("u/{n}"),
            provider: "7tv".into(),
            id: (*n).into(),
            animated: false,
            zero_width: false,
        }))
    }

    fn widths(rows: &[RowL]) -> Vec<usize> {
        rows.iter()
            .map(|r| {
                r.line
                    .spans
                    .iter()
                    .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                    .sum()
            })
            .collect()
    }

    fn lay(text: &str, maxw: u16) -> Vec<RowL> {
        wrap_layout(
            text,
            &set_of(&[]),
            EmoteMode::Text,
            maxw,
            LineCtx {
                lead: vec![Span::raw("u: ")],
                lead_col: 3,
                hue: Color::Reset,
                tint: None,
            },
        )
    }

    #[test]
    fn wraps_at_width_with_hanging_indent() {
        let rows = lay("one two three four five six seven", 12);
        assert!(rows.len() > 1);
        for w in widths(&rows) {
            assert!(w <= 12, "row overflows: {w}");
        }
        // continuation rows start with the 2-cell indent
        for r in &rows[1..] {
            assert!(r.line.spans[0].content.starts_with("  "));
        }
    }

    #[test]
    fn hard_breaks_row_width_words() {
        let long = "x".repeat(40);
        let rows = lay(&long, 12);
        assert!(rows.len() >= 4);
        for w in widths(&rows) {
            assert!(w <= 12);
        }
    }

    #[test]
    fn single_row_stays_single() {
        let rows = lay("short msg", 40);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].h, 1);
    }

    #[test]
    fn text_tier_emote_name_wraps_as_word() {
        let set = set_of(&["KEKW"]);
        let rows = wrap_layout(
            "aaaa bbbb KEKW",
            &set,
            EmoteMode::Text,
            10,
            LineCtx {
                lead: vec![],
                lead_col: 0,
                hue: Color::Reset,
                tint: None,
            },
        );
        assert!(rows.iter().all(|r| r.h == 1)); // text tier never grows rows
        assert!(rows.len() >= 2);
    }

    #[test]
    fn mention_tint_pads_row_to_full_width() {
        let rows = wrap_layout(
            "yo @me hi",
            &set_of(&[]),
            EmoteMode::Text,
            20,
            LineCtx {
                lead: vec![],
                lead_col: 0,
                hue: Color::Reset,
                tint: Some(Color::Indexed(52)),
            },
        );
        let w: usize = rows[0]
            .line
            .spans
            .iter()
            .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
            .sum();
        assert_eq!(w, 20); // padded edge-to-edge
        assert!(rows[0]
            .line
            .spans
            .iter()
            .all(|s| s.style.bg == Some(Color::Indexed(52))));
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
