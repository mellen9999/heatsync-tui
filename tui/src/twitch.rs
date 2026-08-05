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

/// spawn the twitch sender. returns the send handle plus a note channel — auth
/// failures and twitch NOTICE rejections (followers-only, banned, slow mode…)
/// surface there instead of vanishing. the thread runs until the handle drops.
pub fn spawn(user: String, oauth: String) -> (Sender<Send>, Receiver<String>) {
    let (tx, rx) = mpsc::channel();
    let (note_tx, note_rx) = mpsc::channel();
    thread::spawn(move || run(user, oauth, rx, note_tx));
    (tx, note_rx)
}

fn run(user: String, oauth: String, rx: Receiver<Send>, notes: Sender<String>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        match session(&user, &oauth, &rx, &notes) {
            End::Closed => return, // app dropped the sender
            End::AuthFailed => {
                // permanent — reconnecting with the same token just loops.
                let _ = notes.send("twitch auth failed — run: heatsync login".into());
                return;
            }
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
    AuthFailed,
}

fn session(user: &str, oauth: &str, rx: &Receiver<Send>, notes: &Sender<String>) -> End {
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
        .send(WsMsg::Text(format!("PASS oauth:{oauth}\r\n")))
        .is_err()
    {
        return End::Dropped;
    }
    if ws
        .send(WsMsg::Text(format!("NICK {}\r\n", user.to_lowercase())))
        .is_err()
    {
        return End::Dropped;
    }

    // twitch silently drops PRIVMSG to a channel you haven't JOINed, so join each
    // channel once (per connection) the first time we post to it. sends queued
    // before the 001 welcome are buffered — a PRIVMSG that races auth is
    // silently eaten by twitch otherwise.
    let mut joined: HashSet<String> = HashSet::new();
    let mut authed = false;
    let mut pending: Vec<Send> = Vec::new();

    loop {
        match ws.read() {
            Ok(WsMsg::Text(t)) => {
                for line in t.as_str().lines() {
                    // twitch pings periodically; must reply or we get dropped.
                    if line.starts_with("PING") {
                        let _ = ws.send(WsMsg::Text("PONG :tmi.twitch.tv\r\n".into()));
                        continue;
                    }
                    if !authed && line.contains(" 001 ") {
                        authed = true;
                        for (chan, text) in pending.drain(..) {
                            if post(&mut ws, &mut joined, &chan, &text).is_err() {
                                return End::Dropped;
                            }
                        }
                        continue;
                    }
                    // login rejection arrives as a NOTICE before any 001.
                    if !authed && line.contains("Login authentication failed") {
                        return End::AuthFailed;
                    }
                    // channel NOTICEs carry the real reason a message was
                    // refused (followers-only, sub-only, banned, slow mode…).
                    if authed && line.contains("NOTICE #") {
                        if let Some(reason) = line.rsplit_once(" :").map(|(_, r)| r) {
                            let _ = notes.send(format!("twitch: {reason}"));
                        }
                    }
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

        // drain queued sends (buffered until the 001 welcome lands).
        loop {
            match rx.try_recv() {
                Ok((channel, text)) => {
                    if !authed {
                        pending.push((channel, text));
                        continue;
                    }
                    if post(&mut ws, &mut joined, &channel, &text).is_err() {
                        return End::Dropped;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return End::Closed,
            }
        }
    }
}

/// JOIN (once per connection) + PRIVMSG one message.
fn post(
    ws: &mut tungstenite::WebSocket<MaybeTlsStream<TcpStream>>,
    joined: &mut HashSet<String>,
    channel: &str,
    text: &str,
) -> Result<(), ()> {
    let chan = channel.to_lowercase();
    if joined.insert(chan.clone()) && ws.send(WsMsg::Text(format!("JOIN #{chan}\r\n"))).is_err() {
        return Err(());
    }
    ws.send(WsMsg::Text(format!("PRIVMSG #{chan} :{text}\r\n")))
        .map_err(|_| ())
}

fn tcp_of(ws: &mut tungstenite::WebSocket<MaybeTlsStream<TcpStream>>) -> Option<&TcpStream> {
    match ws.get_mut() {
        MaybeTlsStream::Plain(s) => Some(s),
        MaybeTlsStream::Rustls(s) => Some(s.get_ref()),
        _ => None,
    }
}
