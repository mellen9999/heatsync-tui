//! headless subcommands — the archive corpus as pipeable unix commands.
//! `heatsync log <channel> <date>` · `search <query>` · `hot`. plain stdout,
//! grep/fzf-friendly. this is the moat chatterino structurally can't match.

use std::time::{Duration, Instant};

use heatsync_core::Platform;

use crate::config;
use crate::http;
use crate::net::{self, ChatEvent};

/// split a `kick:name` / `twitch:name` / `name` token into (platform, name).
fn split_channel(tok: &str) -> (Platform, &str) {
    if let Some(rest) = tok.strip_prefix("kick:") {
        (Platform::Kick, rest)
    } else if let Some(rest) = tok.strip_prefix("twitch:") {
        (Platform::Twitch, rest)
    } else {
        (Platform::Twitch, tok)
    }
}

/// `heatsync log <channel> <YYYY-MM-DD>`
pub fn log(args: &[String]) -> std::io::Result<()> {
    let (channel, date) = match args {
        [c, d, ..] => (c.as_str(), d.as_str()),
        _ => {
            eprintln!("usage: heatsync log <channel> <YYYY-MM-DD>");
            std::process::exit(2);
        }
    };
    let (_plat, name) = split_channel(channel);
    let from = format!("{date}T00:00:00Z");
    let to = format!("{date}T23:59:59Z");
    match http::channel_log(name, &from, &to, 100) {
        Some(page) if !page.results.is_empty() => {
            for m in &page.results {
                let who = m.display_name.as_deref().unwrap_or(&m.username);
                println!("{}  {}: {}", short_ts(&m.timestamp), who, m.message);
            }
        }
        Some(_) => eprintln!("no logs for {name} on {date}"),
        None => {
            eprintln!("request failed");
            std::process::exit(1);
        }
    }
    Ok(())
}

/// `heatsync search <query> [channel]`
pub fn search(args: &[String]) -> std::io::Result<()> {
    let q = match args.first() {
        Some(q) => q.as_str(),
        None => {
            eprintln!("usage: heatsync search <query> [channel]");
            std::process::exit(2);
        }
    };
    let channel = args.get(1).map(|s| split_channel(s).1);
    match http::search(q, channel, 50) {
        Some(page) if !page.results.is_empty() => {
            for m in &page.results {
                let who = m.display_name.as_deref().unwrap_or(&m.username);
                println!(
                    "{}  [{}/{}] {}: {}",
                    short_ts(&m.timestamp),
                    m.platform,
                    m.channel,
                    who,
                    m.message
                );
            }
        }
        Some(_) => eprintln!("no matches for {q:?}"),
        None => {
            eprintln!("request failed");
            std::process::exit(1);
        }
    }
    Ok(())
}

/// `heatsync hot [hours]` — hottest posts, with real heat scores.
pub fn hot(args: &[String]) -> std::io::Result<()> {
    let hours: u32 = args.first().and_then(|s| s.parse().ok()).unwrap_or(6);
    match http::hot(25, hours) {
        Some(page) if !page.messages.is_empty() => {
            for m in &page.messages {
                let who = m.display_name.as_deref().unwrap_or(&m.username);
                let subj = m.subject.as_deref().unwrap_or("");
                println!(
                    "{:>6.0}  (peak {:>6.0})  {}: {} {}",
                    m.heat, m.max_heat, who, m.content, subj
                );
            }
        }
        Some(_) => eprintln!("nothing hot in the last {hours}h"),
        None => {
            eprintln!("request failed");
            std::process::exit(1);
        }
    }
    Ok(())
}

/// `heatsync login` — set up direct twitch sending (chatterino-style).
pub fn login() -> std::io::Result<()> {
    match config::ensure_token_file() {
        Some(p) => {
            println!("twitch sending — sends go DIRECT to twitch (like chatterino), not via heatsync:");
            println!("  1. get a twitch oauth token with the 'chat:edit' scope");
            println!("     e.g. https://twitchtokengenerator.com  (pick a bot/chat token)");
            println!("  2. edit {}", p.display());
            println!("       twitch_user=your_twitch_username");
            println!("       twitch_oauth=oauth:xxxxxxxxxxxxxxxx");
            println!("  3. restart heatsync. reading still comes through heatsync; only sending is direct.");
            println!("  (or set TWITCH_USER / TWITCH_OAUTH env instead of the file.)");
        }
        None => eprintln!("could not resolve ~/.config/heatsync/"),
    }
    Ok(())
}

/// `heatsync probe [channels…]` — connect to the live relay, print raw lines
/// for 8s, report the count. an end-to-end WS smoke test (and a debug tap).
pub fn probe(args: &[String]) -> std::io::Result<()> {
    let subs = if args.is_empty() {
        vec![(Platform::Twitch, "xqc".to_string())]
    } else {
        args.iter()
            .map(|a| {
                let (p, n) = split_channel(a);
                (p, n.to_string())
            })
            .collect()
    };
    eprintln!("probing {} channel(s) for 8s…", subs.len());
    let (rx, _out) = net::spawn(subs, None);
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut n = 0u32;
    while let Some(rem) = deadline.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(rem) {
            Ok(ChatEvent::Line(l)) => {
                println!("[{}/{}] {}: {}", plat_tag(l.platform), l.channel, l.user, l.content);
                n += 1;
            }
            Ok(ChatEvent::Connected) => eprintln!("· connected"),
            Ok(ChatEvent::Disconnected) => eprintln!("· disconnected"),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    eprintln!("{n} lines in 8s");
    Ok(())
}

fn plat_tag(p: Platform) -> &'static str {
    p.tag()
}

/// trim an ISO timestamp to `HH:MM:SS` for terminal density.
fn short_ts(ts: &str) -> &str {
    ts.split('T')
        .nth(1)
        .map(|t| &t[..t.len().min(8)])
        .unwrap_or(ts)
}
