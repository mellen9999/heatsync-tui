use crate::app::{App, Feed, InputMode};
use crate::feed::{spawn_emote_fetch, spawn_history};
use crate::net::Sub;
use crate::{hsauth, key, modcmd, net, picker};
use crate::{parse_sub, parse_tab, save_state, yt_video_id};
use crossterm::event::{KeyCode, KeyModifiers};
use heatsync_core::complete::Completion;
use heatsync_core::{edit, slash};
use heatsync_core::{Channel, Message, Platform};
use std::time::Instant;

#[derive(PartialEq)]
pub(crate) enum Flow {
    Continue,
    Quit,
}

/// dispatch a keypress by mode. Normal = navigation; Insert = compose a message;
/// Pick = the live-channel picker, Search = type a `/` pattern. h/l always change
/// channel; j/k are cursor motion, except on a vertical tab bar where (in
/// normal mode) they walk the channel list the eye is looking at.
pub(crate) fn handle_key(app: &mut App, k: crossterm::event::KeyEvent) -> Flow {
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
        InputMode::Pick => pick_key(app, k),
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
                KeyCode::Char('a') | KeyCode::Char('o') => open_picker(app),
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
        KeyCode::Char('k')
        | KeyCode::Char('h')
        | KeyCode::Up
        | KeyCode::Left
        | KeyCode::BackTab => tab_and_continue(app, -1),
        KeyCode::Char('i') | KeyCode::Char('a') => {
            app.mode = InputMode::Insert;
            app.line.focus();
            app.status = None;
            Flow::Continue
        }
        KeyCode::Char('o') => {
            open_picker(app);
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
        KeyCode::Char('t') => {
            app.stamps = !app.stamps;
            app.status = Some(format!(
                "timestamps {}",
                if app.stamps { "on" } else { "off" }
            ));
            save_state(app);
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

/// `o`: open the picker. a copy fetched under 30s ago is used as is; an older
/// one shows while a fresh fetch runs off-thread.
fn open_picker(app: &mut App) {
    app.status = None;
    app.mode = InputMode::Pick;
    if let Some(f) = app.pick_cache.fresh() {
        app.picker = Some(picker::Picker::new(Some(f.clone()), false));
        return;
    }
    app.picker = Some(picker::Picker::new(app.pick_cache.any().cloned(), true));
    if app.pick_inflight {
        return;
    }
    app.pick_inflight = true;
    let mine: Vec<(Platform, String)> = app
        .channels
        .iter()
        .flat_map(|c| c.subs().map(|(p, n)| (p, n.to_string())))
        .collect();
    let tx = app.pick_tx.clone();
    let hs = app.hs.clone();
    std::thread::spawn(move || {
        let _ = tx.send(picker::fetch(&mine, hs.as_ref()));
    });
}

pub(crate) fn drain_picker(app: &mut App) {
    while let Ok(f) = app.pick_rx.try_recv() {
        app.pick_inflight = false;
        // a failed fetch must not pin an empty list for 30s.
        if f.error.is_none() {
            app.pick_cache = picker::Cache(Some((Instant::now(), f.clone())));
        }
        if let Some(p) = &mut app.picker {
            p.set(f);
        }
    }
}

fn pick_key(app: &mut App, k: crossterm::event::KeyEvent) {
    let Some(p) = &mut app.picker else {
        app.mode = InputMode::Normal;
        return;
    };
    match p.key(k) {
        picker::Action::None => {}
        picker::Action::Close => {
            app.picker = None;
            app.mode = InputMode::Normal;
        }
        picker::Action::Join(spec) => {
            app.picker = None;
            app.mode = InputMode::Normal;
            open_channel(app, &spec);
        }
        picker::Action::Merge(spec) => {
            app.picker = None;
            app.mode = InputMode::Normal;
            // the current tab's own sources + the new one: open_channel folds
            // the extras into the existing tab.
            let cur: Vec<String> = app
                .channels
                .get(app.focus)
                .map(|c| c.subs().map(|(p, n)| picker::spec_of(p, n)).collect())
                .unwrap_or_default();
            let tok = if cur.is_empty() {
                spec
            } else {
                format!("{}+{spec}", cur.join("+"))
            };
            open_channel(app, &tok);
        }
    }
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
    if let Some(i) = app.channels.iter().position(|c| c.matches(platform, &name)) {
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
pub(crate) fn slot_of(ch: &Channel, p: Platform) -> Option<String> {
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
    if old.as_deref().is_some_and(|o| o.eq_ignore_ascii_case(&new)) {
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
    // /ban /unban /timeout /delete: twitch no longer runs these as chat text,
    // so they go through heatsync as real mod actions.
    if let Some(p) = modcmd::parse(&text, |u| last_id_of(app, u)) {
        run_mod(app, p);
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
                id: None,
                gone: None,
                reply_to: None,
                note: None,
                extra: Default::default(),
                heat: 0.0,
                uid: None,
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
        Platform::Twitch => {
            if let Some(client) = app.hs.clone() {
                send_via_heatsync(app, client, platform, name.clone(), text);
                app.line.accept();
                app.status = Some(format!("sent → {name}"));
            } else {
                match &app.twitch_tx {
                    Some(tx) => {
                        // a dead sender (auth failed for good) must not look like a send.
                        if tx.send((name.clone(), text)).is_ok() {
                            app.line.accept();
                            app.status = Some(format!("sent → {name}"));
                        } else {
                            app.status =
                                Some("twitch sender stopped — run: heatsync-tui login".into());
                        }
                    }
                    None => app.status = Some("not logged in — run: heatsync-tui login".into()),
                }
            }
        }
        // heatsync login first; then a kick token the user supplied; then the ext relay.
        Platform::Kick => {
            if let Some(client) = app.hs.clone() {
                send_via_heatsync(app, client, platform, name.clone(), text);
                app.line.accept();
                app.status = Some(format!("sent → {name}"));
            } else if let Some(ktx) = &app.kick_tx {
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
                app.status = Some("not logged in — run: heatsync-tui login kick".into());
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

/// post `text` through heatsync on its own thread; only a failure speaks.
fn send_via_heatsync(
    app: &App,
    client: hsauth::Client,
    platform: Platform,
    name: String,
    text: String,
) {
    let chan = name.clone();
    hsauth::spawn_note(app.hs_tx.clone(), platform, name, move || {
        match client.send(platform, &chan, &text) {
            Ok(()) => hsauth::Outcome {
                ok: true,
                text: String::new(),
            },
            Err(e) => hsauth::Outcome { ok: false, text: e },
        }
    });
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

/// a mod command typed in the composer → a real mod action through heatsync.
fn run_mod(app: &mut App, parsed: modcmd::Parsed) {
    let (action, target) = match parsed {
        modcmd::Parsed::Usage(u) => {
            app.status = Some(u.into()); // keep the draft so it can be fixed
            return;
        }
        modcmd::Parsed::Run(a, t) => (a, t),
    };
    let (platform, name) = {
        let c = &app.channels[app.focus];
        (c.platform, c.name.clone())
    };
    if matches!(app.feed, Feed::Mock(_)) {
        app.status = Some("offline demo — mod actions are off".into());
        return;
    }
    match platform {
        Platform::Twitch => {}
        Platform::Kick => {
            app.status = Some("mod tools aren't on kick yet — use kick.com for that".into());
            return;
        }
        Platform::Youtube => {
            app.status = Some("mod tools are twitch-only for now".into());
            return;
        }
    }
    let Some(client) = app.hs.clone() else {
        app.status = Some("log in first to use mod tools — run: heatsync-tui login".into());
        return;
    };
    app.line.accept();
    app.status = Some("working…".into());
    let chan = name.clone();
    hsauth::spawn_note(app.hs_tx.clone(), platform, name, move || {
        client.mod_action(platform, &chan, &target, &action)
    });
}

/// the newest message id this user has in the focused tab (for `/delete <user>`).
fn last_id_of(app: &App, user: &str) -> Option<String> {
    app.channels
        .get(app.focus)?
        .messages
        .iter()
        .rev()
        .find(|m| m.user.eq_ignore_ascii_case(user) && m.id.is_some())
        .and_then(|m| m.id.clone())
}
