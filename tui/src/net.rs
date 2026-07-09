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

/// spawn the live feed. returns the receiver of chat lines; the thread runs
/// until the receiver is dropped (app exit).
pub fn spawn(subs: Vec<Sub>) -> Receiver<ChatEvent> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || run(subs, tx));
    rx
}

/// what the feed reports upward — chat plus connection state for the status bar.
pub enum ChatEvent {
    Line(proto::ChatLine),
    Connected,
    Disconnected,
}

fn run(subs: Vec<Sub>, tx: Sender<ChatEvent>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        match session(&subs, &tx) {
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

fn session(subs: &[Sub], tx: &Sender<ChatEvent>) -> SessionEnd {
    let mut ws = match tungstenite::connect(WS_URL) {
        Ok((ws, _resp)) => ws,
        Err(_) => return SessionEnd::Dropped,
    };
    // non-blocking-ish reads so we can heartbeat from this one thread.
    if let Ok(sock) = tcp_of(&mut ws) {
        let _ = sock.set_read_timeout(Some(READ_TIMEOUT));
    }
    for (platform, channel) in subs {
        if ws.send(WsMsg::Text(proto::join(*platform, channel).into())).is_err() {
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
                _ => {}
            },
            Ok(WsMsg::Ping(p)) => {
                let _ = ws.send(WsMsg::Pong(p));
            }
            Ok(WsMsg::Close(_)) => return SessionEnd::Dropped,
            Ok(_) => {}
            // read timeout / would-block → not an error, go heartbeat.
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => return SessionEnd::Dropped,
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
