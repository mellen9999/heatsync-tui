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
    // Default 720h bounds the server-side scan only — the heat floor (12h
    // decay) decides what's hot, so a narrow default would just hide live heat.
    let hours: u32 = args.first().and_then(|s| s.parse().ok()).unwrap_or(720);
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

/// `heatsync render-test <base-url> [overlay-url…]` — VERIFICATION harness (not a
/// user command). composites a stack via the real pipeline and dumps the result:
/// the composited RGBA frame(s) as PNG, and the actual sixel bytes the terminal
/// would receive. decode those (sixel2png) to prove rendering end-to-end without
/// needing to eyeball a live terminal. run with `env -u TMUX` for raw (unwrapped)
/// sixel.
pub fn render_test(args: &[String]) -> std::io::Result<()> {
    use ratatui_image::picker::ProtocolType;

    if args.is_empty() {
        eprintln!("usage: heatsync render-test <base-url> [overlay-url …]");
        return Ok(());
    }
    let key = args.join("\n");
    let Some(frames) = crate::emote::render::composite_frames(&key, 32) else {
        eprintln!("render-test: composite failed (fetch/decode)");
        return Ok(());
    };
    let (w, h) = (frames[0].0.width(), frames[0].0.height());
    eprintln!(
        "render-test: {} layer(s) → {} composited frame(s), {w}x{h}px",
        args.len(),
        frames.len()
    );
    if frames.len() > 1 {
        let total: u32 = frames.iter().map(|f| f.1.max(20)).sum();
        let fps = frames.len() as f64 * 1000.0 / total.max(1) as f64;
        eprintln!(
            "  authored: {:.0}ms loop at {fps:.1}fps (a 100ms redraw cadence would \
             show {:.0} of these {} frames)",
            total,
            (total as f64 / 100.0).floor(),
            frames.len()
        );
    }
    let idxs = if frames.len() > 1 {
        vec![0usize, frames.len() / 2]
    } else {
        vec![0usize]
    };
    for i in &idxs {
        let p = format!("/tmp/emote_composite_{i}.png");
        if frames[*i].0.save(&p).is_ok() {
            eprintln!("  composite frame {i} → {p}");
        }
    }
    // run the REAL sizing + palette + encode path for an 8x16 cell terminal and
    // report the two things that were broken: the cell footprint, and how much
    // the sixel payload churns between consecutive frames. that churn IS the
    // flashing — a shared palette should leave only real motion changing.
    let (cw, ch) = (8u16, 16u16);
    let mut picker = ratatui_image::picker::Picker::from_fontsize((cw, ch));
    picker.set_protocol_type(ProtocolType::Sixel);
    let block_h = crate::emote::render::EMOTE_H as u32 * ch as u32;
    let w_cells = crate::emote::render::width_cells(w as f32 / h as f32, cw, ch);
    eprintln!(
        "  footprint on an {cw}x{ch} cell: {w_cells}x{} cells = {}x{block_h}px (source {w}x{h})",
        crate::emote::render::EMOTE_H,
        w_cells as u32 * cw as u32,
    );

    let take = frames.len().min(24);
    let mut canvases: Vec<image::RgbaImage> = frames[..take]
        .iter()
        .map(|(f, _)| crate::emote::render::fit_center(f, w_cells as u32 * cw as u32, block_h))
        .collect();
    for c in &mut canvases {
        crate::emote::render::flatten_onto_black(c);
    }
    let raw = encode_all(&picker, &canvases, w_cells);
    crate::emote::render::stabilize_palette(&mut canvases);
    let stable = encode_all(&picker, &canvases, w_cells);

    if let Some(first) = stable.first() {
        std::fs::write("/tmp/emote.six", first)?;
        let wrapped = first.contains("tmux;");
        eprintln!(
            "  sixel bytes → /tmp/emote.six ({} bytes){}",
            first.len(),
            if wrapped { " [tmux-wrapped — rerun with `env -u TMUX`]" } else { "" }
        );
    }
    if take > 1 {
        let px = (w_cells as u32 * cw as u32, block_h);
        eprintln!(
            "  pixels changing between consecutive frames (lower = less shimmer):"
        );
        eprintln!("    per-frame palette (before): {:.1}%", pixel_churn(&raw, px) * 100.0);
        eprintln!("    shared palette   (after):  {:.1}%", pixel_churn(&stable, px) * 100.0);
        eprintln!(
            "    source frames actually differ by: {:.1}% (the real motion)",
            source_churn(&canvases_src(&frames[..take], w_cells as u32 * cw as u32, block_h)) * 100.0
        );
    }
    Ok(())
}

fn canvases_src(frames: &[(image::RgbaImage, u32)], tw: u32, th: u32) -> Vec<image::RgbaImage> {
    frames
        .iter()
        .map(|(f, _)| {
            let mut c = crate::emote::render::fit_center(f, tw, th);
            crate::emote::render::flatten_onto_black(&mut c);
            c
        })
        .collect()
}

/// mean fraction of pixels differing between consecutive source frames — the
/// floor. anything a decoded sixel churns ABOVE this is encoder shimmer.
fn source_churn(frames: &[image::RgbaImage]) -> f64 {
    if frames.len() < 2 {
        return 0.0;
    }
    let mut total = 0.0;
    for w in frames.windows(2) {
        let n = w[0].pixels().len().max(1);
        let diff = w[0].pixels().zip(w[1].pixels()).filter(|(a, b)| a != b).count();
        total += diff as f64 / n as f64;
    }
    total / (frames.len() - 1) as f64
}

/// decode each sixel back to pixels and report the mean fraction that changes
/// between consecutive frames. this is what the eye sees flicker.
fn pixel_churn(frames: &[String], (w, h): (u32, u32)) -> f64 {
    let decoded: Vec<Vec<u32>> = frames.iter().map(|s| sixel_decode(s, w, h)).collect();
    if decoded.len() < 2 {
        return 0.0;
    }
    let mut total = 0.0;
    for pair in decoded.windows(2) {
        let n = (w * h) as usize;
        let diff = pair[0].iter().zip(&pair[1]).filter(|(a, b)| a != b).count();
        total += diff as f64 / n.max(1) as f64;
    }
    total / (decoded.len() - 1) as f64
}

/// minimal sixel reader — enough of the grammar to rebuild the pixel grid an
/// emote encodes to (palette defs, color select, 6-pixel data bytes, RLE, `$`
/// carriage return, `-` newline). verification-only.
fn sixel_decode(data: &str, w: u32, h: u32) -> Vec<u32> {
    let mut out = vec![0u32; (w * h) as usize];
    let mut palette = std::collections::HashMap::new();
    let b: Vec<char> = data.chars().collect();
    let mut i = 0usize;
    // skip to the start of sixel data (after the `q` of the DCS header)
    while i < b.len() && b[i] != 'q' {
        i += 1;
    }
    i += 1;
    let (mut x, mut band, mut color) = (0u32, 0u32, 0u32);
    let num = |b: &[char], i: &mut usize| -> u32 {
        let mut n = 0u32;
        while *i < b.len() && b[*i].is_ascii_digit() {
            n = n.saturating_mul(10).saturating_add(b[*i] as u32 - '0' as u32);
            *i += 1;
        }
        n
    };
    while i < b.len() {
        match b[i] {
            '#' => {
                i += 1;
                let idx = num(&b, &mut i);
                if i < b.len() && b[i] == ';' {
                    i += 1;
                    let _mode = num(&b, &mut i);
                    let mut c = [0u32; 3];
                    for slot in c.iter_mut() {
                        if i < b.len() && b[i] == ';' {
                            i += 1;
                        }
                        *slot = num(&b, &mut i);
                    }
                    palette.insert(idx, (c[0] << 16) | (c[1] << 8) | c[2] | 1 << 24);
                }
                color = idx;
            }
            '!' => {
                i += 1;
                let n = num(&b, &mut i);
                if i < b.len() {
                    let ch = b[i];
                    i += 1;
                    for _ in 0..n {
                        put(&mut out, w, h, x, band, ch, *palette.get(&color).unwrap_or(&0));
                        x += 1;
                    }
                }
            }
            '$' => {
                x = 0;
                i += 1;
            }
            '-' => {
                x = 0;
                band += 1;
                i += 1;
            }
            '"' => {
                i += 1;
                while i < b.len() && (b[i].is_ascii_digit() || b[i] == ';') {
                    i += 1;
                }
            }
            c if ('?'..='~').contains(&c) => {
                put(&mut out, w, h, x, band, c, *palette.get(&color).unwrap_or(&0));
                x += 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    out
}

fn put(out: &mut [u32], w: u32, h: u32, x: u32, band: u32, ch: char, rgb: u32) {
    if x >= w {
        return;
    }
    let bits = (ch as u32).wrapping_sub('?' as u32);
    for bit in 0..6u32 {
        if bits & (1 << bit) != 0 {
            let y = band * 6 + bit;
            if y < h {
                out[(y * w + x) as usize] = rgb;
            }
        }
    }
}

fn encode_all(picker: &ratatui_image::picker::Picker, canvases: &[image::RgbaImage], w_cells: u16) -> Vec<String> {
    let size = ratatui::layout::Rect::new(0, 0, w_cells, crate::emote::render::EMOTE_H);
    canvases
        .iter()
        .filter_map(|c| {
            match picker.new_protocol(
                image::DynamicImage::ImageRgba8(c.clone()),
                size,
                ratatui_image::Resize::Fit(None),
            ) {
                Ok(ratatui_image::protocol::Protocol::Sixel(s)) => Some(s.data),
                _ => None,
            }
        })
        .collect()
}

