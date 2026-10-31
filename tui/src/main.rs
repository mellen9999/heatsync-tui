//! heatsync tui — heat-sorted live multichat. `heatsync [channels…]` opens the
//! grid (default demo set); `--mock` runs the offline synthetic feed; `log`,
//! `search`, `hot` are headless corpus subcommands.
//! keys: j/k and h/l (or arrows, tab/shift-tab) change channel, space pauses,
//! i composes, o joins, m manages, q quits. chat always follows live — there
//! is no message cursor and no keyboard nav over messages.

mod app;
#[allow(dead_code)]
mod cli;
mod config;
mod draw;
mod drawlog;
mod emote;
mod feed;
mod hsauth;
mod http;
mod input;
mod intr;
mod key;
mod kick;
mod layout;
mod modcmd;
mod net;
mod paints;
mod palette;
mod picker;
mod sched;
mod twitch;

use std::io::{self, IsTerminal};
use std::time::{Duration, Instant};

use app::{App, Feed, InputMode};
use config::TabPos;
use crossterm::event::{self, Event, KeyEventKind};
use draw::{emote_mode, ui};
use emote::fb::FbEmotes;
use emote::render::EmoteStore;
use feed::{
    drain_emotes, drain_feed, drain_history, drain_hs, drain_twitch, pump_loads, request_stacks,
    spawn_emote_fetch, spawn_global_emotes, spawn_history, tick_advance,
};
use heatsync_core::edit;
use heatsync_core::emote::EmoteSet;
use heatsync_core::{mock, Channel, Platform};
use input::{drain_picker, handle_key, Flow};
use net::Sub;
use ratatui::Terminal;

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
    login      log in with heatsync (chat + mod, twitch and kick)
    login kick same login, with how to link kick
    logout     log out and forget the login
    status     who is logged in and what they can do
    probe      check a channel is reachable
    diag       dump diagnostics
    render-test  draw the emote/paint test pattern

OPTIONS:
    --mock     run against a synthetic feed, no network
    -h, --help
    -V, --version
";

fn main() -> io::Result<()> {
    intr::install();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("log") => return cli::log(&args[1..]),
        Some("search") => return cli::search(&args[1..]),
        Some("hot") | Some("top") => return cli::hot(&args[1..]),
        Some("probe") => return cli::probe(&args[1..]),
        Some("diag") => return cli::diag(&args[1..]),
        Some("render-test") => return cli::render_test(&args[1..]),
        Some("status") => return cli::status(),
        Some("login") => {
            intr::login_phase();
            // an interactive login drops you into the chat, same as plain `hs`
            let opened = cli::login(args.get(1).map(String::as_str) == Some("kick"))?;
            intr::plain_phase();
            if !opened {
                return Ok(());
            }
            return chat(&[]);
        }
        Some("logout") => return cli::logout(),
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
    chat(&args)
}

/// the chat ui, for plain `hs` and for the end of an interactive `hs login`.
fn chat(args: &[String]) -> io::Result<()> {
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
        let (hs_tx, hs_rx) = std::sync::mpsc::channel::<hsauth::Note>();
        let (emote_tx, emote_rx) = std::sync::mpsc::channel();
        let (hist_tx, hist_rx) = std::sync::mpsc::channel();
        let (pick_tx, pick_rx) = std::sync::mpsc::channel();
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
            twitch_notes: None,
            kick_tx: None,
            hs: None,
            hs_tx,
            hs_notes: hs_rx,
            tab_pos: cfg.tab_pos,
            manage_cursor: 0,
            completion: None,
            me: None,
            emote_tx,
            emote_rx,
            hist_tx,
            hist_rx,
            picker: None,
            pick_cache: picker::Cache(None),
            pick_tx,
            pick_rx,
            pick_inflight: false,
            paints: paints::Paints::new(),
        }
    } else {
        build_live(&chan_args, cfg.tab_pos, cfg.channels)
    };

    // must probe the terminal for graphics BEFORE raw mode / alt screen.
    let had_fb = app.fb.is_some();
    let mut terminal = ratatui::init();
    intr::tui_phase();
    let res = run(&mut terminal, app);
    ratatui::restore();
    intr::plain_phase();
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
    let (twitch_tx, twitch_notes) = match (auth.twitch_user, auth.twitch_oauth) {
        (Some(u), Some(o)) => {
            let (tx, notes) = twitch::spawn(u, o);
            (Some(tx), Some(notes))
        }
        _ => (None, None),
    };
    spawn_global_emotes(&emote_tx);
    let (hs_tx, hs_rx) = std::sync::mpsc::channel::<hsauth::Note>();
    let (pick_tx, pick_rx) = std::sync::mpsc::channel();
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
        twitch_notes,
        kick_tx,
        hs: hsauth::load_client(),
        hs_tx,
        hs_notes: hs_rx,
        tab_pos,
        manage_cursor: 0,
        completion: None,
        me,
        emote_tx,
        emote_rx,
        hist_tx,
        hist_rx,
        picker: None,
        pick_cache: picker::Cache(None),
        pick_tx,
        pick_rx,
        pick_inflight: false,
        paints: paints::Paints::new(),
    }
}

/// persist the current tab set + bar position (live mode only — mock is ephemeral).
pub(crate) fn save_state(app: &App) {
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
pub(crate) fn parse_tab(tok: &str) -> Vec<Sub> {
    let mut subs: Vec<Sub> = Vec::new();
    for part in tok
        .split(|c: char| c == '+' || c.is_whitespace())
        .filter(|s| !s.trim().is_empty())
    {
        let s = parse_sub(part.trim());
        if !s.1.is_empty()
            && !subs
                .iter()
                .any(|(p, n)| *p == s.0 && n.eq_ignore_ascii_case(&s.1))
        {
            subs.push(s);
        }
    }
    subs
}

pub(crate) fn parse_sub(tok: &str) -> Sub {
    if let Some(rest) = tok.strip_prefix("kick:") {
        (Platform::Kick, rest.to_string())
    } else if let Some(rest) = tok
        .strip_prefix("yt:")
        .or_else(|| tok.strip_prefix("youtube:"))
    {
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
pub(crate) fn yt_video_id(tok: &str) -> String {
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
    // see the frame loop below: synchronized output is a win everywhere EXCEPT
    // inside tmux, where it costs us every emote on screen.
    let sync = std::env::var_os("TMUX").is_none();
    // smoothed cost of one draw. animations run at their authored rate, but never
    // faster than the terminal can actually paint — if a draw starts costing real
    // time (a slow pty, a screenful of sixels), the cadence stretches to match
    // instead of queueing writes the terminal renders as tearing. self-tuning, so
    // no protocol gets a hardcoded fps penalty.
    let mut draw_cost = Duration::ZERO;
    let mut dlog = drawlog::DrawLog::from_env();
    let mut gate = sched::ChatGate::default();
    // true when something visible may have changed since the last draw: a key,
    // the data tick, or a batch of chat. the 50ms look-for-chat wakes in between
    // draw nothing, so they cost no bytes on the wire.
    let mut woke = true;
    loop {
        drain_emotes(&mut app);
        drain_history(&mut app);
        drain_twitch(&mut app);
        drain_hs(&mut app);
        drain_picker(&mut app);
        // the focused tab is on screen — whatever it holds is now seen.
        if let Some(ch) = app.channels.get_mut(app.focus) {
            ch.mark_seen();
        }
        // new chat lines and finished emote loads land on EVERY wake — at the
        // animation cadence that's within one frame of arrival, never parked
        // until the next data tick. the tick below keeps only the slow work
        // (heat decay, the mock driver, the focused-channel prefetch sweep).
        // throttled by the gate (see sched.rs): a burst is applied as one batch
        // per gap, wider while typing, so keys repaint only the input line.
        let now = Instant::now();
        let mut dirty = woke;
        if !app.paused && gate.due(now) && drain_feed(&mut app) {
            gate.mark(now);
            dirty = true;
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
        if dirty || emote_mode(&app).next_flip_in().is_some() {
            // snapshot the animation clock + reset the per-draw blit budget, so every
            // emote in this frame is sampled at the same instant.
            if let Some(store) = &app.store {
                store.begin_frame();
            }
            let drew_at = Instant::now();
            // synchronized output (DECSET 2026): the terminal holds presentation
            // until the frame is complete, so a frame's blits land at once instead
            // of streaming in as visible tearing. foot/kitty/wezterm honour it.
            //
            // NOT inside tmux. tmux doesn't pass the mode through — it implements
            // it, and its implementation repaints the WHOLE pane when the frame
            // ends. our emotes reach the terminal as passthrough, which tmux cannot
            // see, so that repaint paints tmux's own blank cells straight over
            // every image we just drew. wrapping frames in it inside tmux means no
            // emote ever survives the frame it was drawn in.
            if sync {
                crossterm::execute!(io::stdout(), crossterm::terminal::BeginSynchronizedUpdate)?;
            }
            let drew = terminal.draw(|f| ui(f, &app));
            if sync {
                crossterm::execute!(io::stdout(), crossterm::terminal::EndSynchronizedUpdate)?;
            }
            drew?;
            dlog.draw();
            // exponential moving average — one slow frame shouldn't throttle us, a
            // sustained slow terminal should.
            draw_cost = (draw_cost * 3 + drew_at.elapsed()) / 4;
            // console tier: paint emote pixels onto the reserved cells now that the
            // text has flushed. terminal tiers draw inline during the frame above.
            if let Some(fb) = &app.fb {
                fb.blit();
            }
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
        // wake often enough to pick up chat, but only to look: no draw unless
        // something arrived (see `dirty`).
        woke = false;
        if intr::requested() {
            return Ok(());
        }
        if event::poll(wait.min(sched::POLL))? {
            woke = true;
            if let Event::Key(k) = event::read()? {
                gate.note_key(Instant::now());
                dlog.key();
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
            woke = true;
        }
    }
}

#[cfg(test)]
mod spec_tests {
    use super::*;
    use heatsync_core::Platform;

    #[test]
    fn plus_and_whitespace_both_merge() {
        let want = vec![
            (Platform::Twitch, "nl_kripp".to_string()),
            (Platform::Kick, "nl_kripp".to_string()),
            (Platform::Youtube, "4tDC0sKhTnA".to_string()),
        ];
        assert_eq!(parse_tab("nl_kripp+kick:nl_kripp+yt:4tDC0sKhTnA"), want);
        assert_eq!(
            parse_tab("twitch:nl_kripp kick:nl_kripp yt:4tDC0sKhTnA"),
            want
        );
        assert_eq!(
            parse_tab("  nl_kripp  +  kick:nl_kripp "),
            want[..2].to_vec()
        );
    }
}
