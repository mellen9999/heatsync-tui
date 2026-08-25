//! live relay client — one blocking WS connection on a background thread (no
//! async runtime). subscribes to every channel, parses frames via core::proto,
//! and streams normalized ChatLines back over an mpsc channel. reconnects with
//! backoff and re-subscribes; a read timeout lets the same thread heartbeat.
//! anon read — no auth needed to consume chat.

use std::io::ErrorKind;
use std::net::TcpStream;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use heatsync_core::proto::{self, Event};
use heatsync_core::Platform;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::Message as WsMsg;

const WS_URL: &str = "wss://heatsync.org/ws";
const READ_TIMEOUT: Duration = Duration::from_millis(800);
const HEARTBEAT: Duration = Duration::from_secs(10);
const BACKOFF_MAX: Duration = Duration::from_secs(15);

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
pub enum ChatEvent {
    Line(proto::ChatLine),
    Connected,
    Disconnected,
    Auth(bool),
    SendResult { ok: bool, error: Option<String> },
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
        if ws.send(WsMsg::Text(proto::authenticate(t).into())).is_err() {
            return SessionEnd::Dropped;
        }
    }
    for (platform, channel) in subs.iter() {
        if ws
            .send(WsMsg::Text(proto::join(*platform, channel).into()))
            .is_err()
        {
            return SessionEnd::Dropped;
        }
    }
    if tx.send(ChatEvent::Connected).is_err() {
        return SessionEnd::ReceiverGone;
    }

    let mut last_hb = Instant::now();
    loop {
        match ws.read() {
            Ok(WsMsg::Text(t)) => match proto::parse(t.as_str()) {
                Event::Chat(l) => {
                    if tx.send(ChatEvent::Line(l)).is_err() {
                        return SessionEnd::ReceiverGone;
                    }
                }
                Event::Backfill(lines) => {
                    for l in lines {
                        if tx.send(ChatEvent::Line(l)).is_err() {
                            return SessionEnd::ReceiverGone;
                        }
                    }
                }
                Event::Auth(ok) => {
                    let _ = tx.send(ChatEvent::Auth(ok));
                }
                Event::SendResult { ok, error } => {
                    let _ = tx.send(ChatEvent::SendResult { ok, error });
                }
                _ => {}
            },
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
            let frame = match o {
                Outbound::Chat {
                    platform: Platform::Kick,
                    channel,
                    text,
                } => {
                    *req += 1;
                    proto::send_kick(&channel, &text, *req)
                }
                // twitch has no send path — silently drop.
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
            if ws.send(WsMsg::Text(frame.into())).is_err() {
                return SessionEnd::Dropped;
            }
        }

        if last_hb.elapsed() >= HEARTBEAT {
            if ws.send(WsMsg::Text(proto::heartbeat().into())).is_err() {
                return SessionEnd::Dropped;
            }
            last_hb = Instant::now();
        }
    }
}

/// borrow the underlying TcpStream to set a read timeout, through TLS or plain.
fn tcp_of(ws: &mut tungstenite::WebSocket<MaybeTlsStream<TcpStream>>) -> Result<&TcpStream, ()> {
    match ws.get_mut() {
        MaybeTlsStream::Plain(s) => Ok(s),
        MaybeTlsStream::Rustls(s) => Ok(s.get_ref()),
        _ => Err(()),
    }
}
