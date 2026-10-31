//! live relay client — one blocking WS connection on a background thread (no
//! async runtime). subscribes to every channel, parses frames via core::proto,
//! and streams normalized ChatLines back over an mpsc channel. reconnects with
//! backoff and re-subscribes; a read timeout lets the same thread heartbeat.
//! anon read — no auth needed to consume chat.

use std::collections::VecDeque;
use std::io::ErrorKind;
use std::net::TcpStream;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use heatsync_core::proto::{self, Event};
use heatsync_core::streamev::StreamEvents;
use heatsync_core::Platform;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::Message as WsMsg;

const WS_URL: &str = "wss://heatsync.org/ws";
const READ_TIMEOUT: Duration = Duration::from_millis(800);
const HEARTBEAT: Duration = Duration::from_secs(10);
const BACKOFF_MAX: Duration = Duration::from_secs(15);
/// gap between join/part frames — the extension spaces its connect burst 80ms
/// apart so the server's per-handler limits never drop a join. frames wait in
/// a queue and go out one per gap from the read loop, which never blocks.
const FRAME_GAP: Duration = Duration::from_millis(80);

type Socket = tungstenite::WebSocket<MaybeTlsStream<TcpStream>>;

/// a channel to subscribe to.
pub type Sub = (Platform, String);

/// outbound requests from the app to the WS thread.
pub enum Outbound {
    Chat {
        platform: Platform,
        channel: String,
        text: String,
    },
    Join {
        platform: Platform,
        channel: String,
    },
    Part {
        platform: Platform,
        channel: String,
    },
}

/// the app's handle for sending outbound frames.
pub type Tx = mpsc::Sender<Outbound>;

/// spawn the live feed. returns the inbound chat receiver and an outbound sender.
/// the thread runs until the inbound receiver is dropped (app exit). if `token`
/// is set, the thread authenticates on every (re)connect so sends are allowed.
pub fn spawn(subs: Vec<Sub>, token: Option<String>) -> (Receiver<ChatEvent>, Tx) {
    let (tx, rx) = mpsc::channel();
    let (out_tx, out_rx) = mpsc::channel();
    thread::spawn(move || run(subs, token, tx, out_rx));
    (rx, out_tx)
}

/// what the feed reports upward — chat plus connection state for the status bar.
// a line dwarfs the other variants, but they're short-lived and boxing every
// line buys nothing.
#[allow(clippy::large_enum_variant)]
pub enum ChatEvent {
    Line(proto::ChatLine),
    /// a mod removed lines (the mod-log note, if any, arrives as a Line).
    Delete(proto::Delete),
    /// followed channels already live at connect (the server's catch-up).
    FollowLive(Vec<Sub>),
    /// a channel pin was set / cleared.
    Pin(Platform, String, Option<heatsync_core::Pin>),
    /// a twitch channel's chat modes (roomstate).
    Room(String, proto::RoomState),
    Connected,
    Disconnected,
    Auth(bool),
    SendResult {
        ok: bool,
        error: Option<String>,
    },
    /// a heatsync name color changed (paint-space id).
    Color {
        id: String,
        color: String,
    },
    /// paints changed for these paint-space ids.
    Cosmetic(Vec<String>),
}

/// hand one parsed frame upward. false = the receiver is gone.
fn forward(ev: Event, tx: &Sender<ChatEvent>) -> bool {
    match ev {
        Event::Chat(l) => tx.send(ChatEvent::Line(l)).is_ok(),
        // a went-live never replays from a backfill (it was shown when it happened).
        Event::Backfill(lines) => lines
            .into_iter()
            .filter(|l| StreamEvents::replay_keeps(l, None))
            .all(|l| tx.send(ChatEvent::Line(l)).is_ok()),
        Event::Delete { line, del } => {
            line.is_none_or(|l| tx.send(ChatEvent::Line(l)).is_ok())
                && tx.send(ChatEvent::Delete(del)).is_ok()
        }
        Event::Batch(es) => es.into_iter().all(|e| forward(e, tx)),
        Event::Auth(ok) => {
            let _ = tx.send(ChatEvent::Auth(ok));
            true
        }
        Event::SendResult { ok, error } => {
            let _ = tx.send(ChatEvent::SendResult { ok, error });
            true
        }
        Event::Color { id, color } => tx.send(ChatEvent::Color { id, color }).is_ok(),
        Event::Cosmetic(ids) => tx.send(ChatEvent::Cosmetic(ids)).is_ok(),
        Event::FollowLive(v) => tx.send(ChatEvent::FollowLive(v)).is_ok(),
        Event::Pin(p, c, pin) => tx.send(ChatEvent::Pin(p, c, pin)).is_ok(),
        Event::Room(c, r) => tx.send(ChatEvent::Room(c, r)).is_ok(),
        Event::Pong | Event::Ignore => true,
    }
}

fn run(mut subs: Vec<Sub>, token: Option<String>, tx: Sender<ChatEvent>, out: Receiver<Outbound>) {
    let mut backoff = Duration::from_secs(1);
    let mut req = 0u64;
    loop {
        match session(&mut subs, &token, &tx, &out, &mut req) {
            // receiver gone → app closed, stop the thread.
            SessionEnd::ReceiverGone => return,
            SessionEnd::Dropped => {
                let _ = tx.send(ChatEvent::Disconnected);
                thread::sleep(backoff);
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
    }
}

enum SessionEnd {
    ReceiverGone,
    Dropped,
}

fn session(
    subs: &mut Vec<Sub>,
    token: &Option<String>,
    tx: &Sender<ChatEvent>,
    out: &Receiver<Outbound>,
    req: &mut u64,
) -> SessionEnd {
    let mut ws = match tungstenite::connect(WS_URL) {
        Ok((ws, _resp)) => ws,
        Err(_) => return SessionEnd::Dropped,
    };
    // non-blocking-ish reads so we can heartbeat + drain sends from one thread.
    if let Ok(sock) = tcp_of(&mut ws) {
        let _ = sock.set_read_timeout(Some(READ_TIMEOUT));
    }
    // authenticate first (required before any send is accepted).
    if let Some(t) = token {
        if ws.send(WsMsg::Text(proto::authenticate(t))).is_err() {
            return SessionEnd::Dropped;
        }
    }
    // resubscribe through the paced queue, not inline: 20+ tabs would stall
    // the loop (pings, heartbeat) for seconds.
    let mut queue: VecDeque<String> = subs.iter().flat_map(|(p, c)| proto::join(*p, c)).collect();
    let mut fast = false;
    let mut last_frame = Instant::now();
    if tx.send(ChatEvent::Connected).is_err() {
        return SessionEnd::ReceiverGone;
    }

    let mut last_hb = Instant::now();
    loop {
        // wake at the queue's cadence while frames wait, idle cadence otherwise.
        if fast != !queue.is_empty() {
            fast = !queue.is_empty();
            if let Ok(sock) = tcp_of(&mut ws) {
                let _ = sock.set_read_timeout(Some(if fast { FRAME_GAP } else { READ_TIMEOUT }));
            }
        }
        match ws.read() {
            Ok(WsMsg::Text(t)) => {
                if !forward(proto::parse(t.as_str()), tx) {
                    return SessionEnd::ReceiverGone;
                }
            }
            Ok(WsMsg::Ping(p)) => {
                let _ = ws.send(WsMsg::Pong(p));
            }
            Ok(WsMsg::Close(_)) => return SessionEnd::Dropped,
            Ok(_) => {}
            // read timeout / would-block → not an error, go drain sends + heartbeat.
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => return SessionEnd::Dropped,
        }

        // drain queued outbound requests.
        while let Ok(o) = out.try_recv() {
            // chat goes out at once; only join/part frames wait their turn.
            let frames = match o {
                Outbound::Chat {
                    platform: Platform::Kick,
                    channel,
                    text,
                } => {
                    *req += 1;
                    if ws
                        .send(WsMsg::Text(proto::send_kick(&channel, &text, *req)))
                        .is_err()
                    {
                        return SessionEnd::Dropped;
                    }
                    continue;
                }
                Outbound::Chat {
                    platform: Platform::Youtube,
                    channel,
                    text,
                } => {
                    *req += 1;
                    if ws
                        .send(WsMsg::Text(proto::send_youtube(&channel, &text, *req)))
                        .is_err()
                    {
                        return SessionEnd::Dropped;
                    }
                    continue;
                }
                // twitch has no relay send path — silently drop.
                Outbound::Chat {
                    platform: Platform::Twitch,
                    ..
                } => continue,
                Outbound::Join { platform, channel } => {
                    if !subs.iter().any(|(p, c)| *p == platform && c == &channel) {
                        subs.push((platform, channel.clone()));
                    }
                    proto::join(platform, &channel)
                }
                Outbound::Part { platform, channel } => {
                    subs.retain(|(p, c)| !(*p == platform && c == &channel));
                    proto::part(platform, &channel)
                }
            };
            queue.extend(frames);
        }

        if last_frame.elapsed() >= FRAME_GAP {
            if let Some(f) = queue.pop_front() {
                if ws.send(WsMsg::Text(f)).is_err() {
                    return SessionEnd::Dropped;
                }
                last_frame = Instant::now();
            }
        }

        if last_hb.elapsed() >= HEARTBEAT {
            if ws.send(WsMsg::Text(proto::heartbeat())).is_err() {
                return SessionEnd::Dropped;
            }
            last_hb = Instant::now();
        }
    }
}

/// borrow the underlying TcpStream to set a read timeout, through TLS or plain.
fn tcp_of(ws: &mut Socket) -> Result<&TcpStream, ()> {
    match ws.get_mut() {
        MaybeTlsStream::Plain(s) => Ok(s),
        MaybeTlsStream::Rustls(s) => Ok(s.get_ref()),
        _ => Err(()),
    }
}
