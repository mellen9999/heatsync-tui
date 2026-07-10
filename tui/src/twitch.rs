//! direct twitch chat sending — the chatterino model. we hold the user's own
//! twitch oauth (chat:edit) and PRIVMSG straight to twitch IRC over websocket,
//! independent of HeatSync's relay (which only ingests twitch read-only). one
//! persistent connection on a background thread; reconnects; PING/PONG keepalive.

use std::collections::HashSet;
use std::io::ErrorKind;
use std::net::TcpStream;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::Duration;

use tungstenite::stream::MaybeTlsStream;
use tungstenite::Message as WsMsg;

const IRC_URL: &str = "wss://irc-ws.chat.twitch.tv:443";
const READ_TIMEOUT: Duration = Duration::from_millis(400);

/// (channel, text) to post.
pub type Send = (String, String);

/// spawn the twitch sender. returns a handle; the thread runs until it's dropped.
pub fn spawn(user: String, oauth: String) -> Sender<Send> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || run(user, oauth, rx));
    tx
}

fn run(user: String, oauth: String, rx: Receiver<Send>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        match session(&user, &oauth, &rx) {
            End::Closed => return, // app dropped the sender
            End::Dropped => {
                thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(15));
            }
        }
    }
}

enum End {
    Closed,
    Dropped,
}

fn session(user: &str, oauth: &str, rx: &Receiver<Send>) -> End {
    let mut ws = match tungstenite::connect(IRC_URL) {
        Ok((ws, _)) => ws,
        Err(_) => return End::Dropped,
    };
    if let Some(sock) = tcp_of(&mut ws) {
        let _ = sock.set_read_timeout(Some(READ_TIMEOUT));
    }
    // authenticate (lowercase nick; PASS carries the oauth: prefix). twitch's
    // ws-irc endpoint wants ONE irc command per frame — a combined
    // "PASS…\r\nNICK…\r\n" frame gets the connection dropped before auth, so
    // PASS and NICK must go as separate frames.
    if ws
        .send(WsMsg::Text(format!("PASS oauth:{oauth}\r\n").into()))
        .is_err()
    {
        return End::Dropped;
    }
    if ws
        .send(WsMsg::Text(format!("NICK {}\r\n", user.to_lowercase()).into()))
        .is_err()
    {
        return End::Dropped;
    }

    // twitch silently drops PRIVMSG to a channel you haven't JOINed, so join each
    // channel once (per connection) the first time we post to it.
    let mut joined: HashSet<String> = HashSet::new();

    loop {
        match ws.read() {
            Ok(WsMsg::Text(t)) => {
                // twitch pings periodically; must reply or we get dropped.
                if t.as_str().starts_with("PING") {
                    let _ = ws.send(WsMsg::Text("PONG :tmi.twitch.tv\r\n".into()));
                }
            }
            Ok(WsMsg::Ping(p)) => {
                let _ = ws.send(WsMsg::Pong(p));
            }
            Ok(WsMsg::Close(_)) => return End::Dropped,
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => return End::Dropped,
        }

        // drain queued sends.
        loop {
            match rx.try_recv() {
                Ok((channel, text)) => {
                    let chan = channel.to_lowercase();
                    if joined.insert(chan.clone())
                        && ws.send(WsMsg::Text(format!("JOIN #{chan}\r\n").into())).is_err()
                    {
                        return End::Dropped;
                    }
                    let line = format!("PRIVMSG #{chan} :{text}\r\n");
                    if ws.send(WsMsg::Text(line.into())).is_err() {
                        return End::Dropped;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return End::Closed,
            }
        }
    }
}

fn tcp_of(ws: &mut tungstenite::WebSocket<MaybeTlsStream<TcpStream>>) -> Option<&TcpStream> {
    match ws.get_mut() {
        MaybeTlsStream::Plain(s) => Some(s),
        MaybeTlsStream::Rustls(s) => Some(s.get_ref()),
        _ => None,
    }
}
