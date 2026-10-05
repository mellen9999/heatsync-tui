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

/// something the sender wants the user to see. `channel` is the tab it belongs
/// to (lowercase, no #); None = about the connection as a whole.
#[derive(Debug, PartialEq, Eq)]
pub struct Note {
    pub channel: Option<String>,
    pub text: String,
}

/// twitch NOTICE msg-id → plain words. the list mirrors the extension's
/// (multichat/auth-irc.js), so a rejected send reads the same in both.
pub fn notice_text(msg_id: &str) -> Option<&'static str> {
    Some(match msg_id {
        "msg_followersonly" => "followers-only mode — follow the channel to chat",
        "msg_followersonly_followed" => "follow the channel a bit longer to chat",
        "msg_followersonly_zero" => "followers-only — you need to follow first",
        "msg_subsonly" => "subscribers-only — sub to chat here",
        "msg_emoteonly" => "emote-only mode — message must be all emotes",
        "msg_slowmode" => "slow mode — please wait a moment",
        "msg_r9k" => "unique-chat mode — message must be unique",
        "msg_duplicate" => "duplicate message — twitch rejected it",
        "msg_banned" => "you are banned from this channel",
        "msg_timedout" => "you are timed out",
        "msg_rejected" => "automod is checking your message",
        "msg_rejected_mandatory" => "automod blocked your message",
        "msg_channel_suspended" => "channel is suspended",
        "msg_channel_blocked" => "channel is blocking messages",
        "msg_verified_email" => "channel requires a verified email to chat",
        "msg_requires_verified_phone_number" => "channel requires a verified phone to chat",
        "no_permission" => "no permission to do that here",
        "unrecognized_cmd" => "twitch did not recognize that command",
        "tos_ban" => "you are banned from twitch",
        _ => return None,
    })
}

/// the `msg-id` tag of an irc line, if it carries tags.
fn msg_id(line: &str) -> Option<&str> {
    let tags = line.strip_prefix('@')?.split(' ').next()?;
    tags.split(';')
        .find_map(|kv| kv.strip_prefix("msg-id="))
}

/// turn one irc line into a user-facing note, if it is a NOTICE worth showing:
/// a known send rejection (friendly text), or any other `msg_*` refusal with
/// twitch's own words. everything else (hosts, mode chatter) stays quiet.
pub fn classify_notice(line: &str) -> Option<Note> {
    let (_, after) = line.split_once(" NOTICE ")?;
    let (target, text) = after.split_once(" :").unwrap_or((after, ""));
    let channel = target
        .trim()
        .strip_prefix('#')
        .map(|c| c.to_lowercase());
    let text = text.trim_end();
    let id = msg_id(line);
    let text = match id.and_then(notice_text) {
        Some(t) => t.to_string(),
        None if id.is_some_and(|i| i.starts_with("msg_")) && !text.is_empty() => {
            format!("twitch: {text}")
        }
        None => return None,
    };
    Some(Note { channel, text })
}

/// the login was refused — no point reconnecting with the same token.
fn is_auth_failure(line: &str) -> bool {
    line.contains("Login authentication failed")
        || line.contains("Improperly formatted auth")
        || line.contains("Login unsuccessful")
}

/// the 001 welcome: the server accepted us and will now keep our PRIVMSGs.
fn is_welcome(line: &str) -> bool {
    line.contains(" 001 ")
}

/// holds sends until the connection is authenticated — a PRIVMSG that races
/// auth is silently eaten by twitch. survives reconnects (pending stays queued);
/// on a permanent auth failure `fail` hands back everything that never left.
#[derive(Default)]
pub struct Gate {
    authed: bool,
    pending: Vec<Send>,
}

impl Gate {
    /// a send arrives: Some(it) if it can go now, None if it was queued.
    pub fn submit(&mut self, s: Send) -> Option<Send> {
        if self.authed {
            Some(s)
        } else {
            self.pending.push(s);
            None
        }
    }

    /// welcome landed: open the gate, release the queue in order.
    pub fn open(&mut self) -> Vec<Send> {
        self.authed = true;
        std::mem::take(&mut self.pending)
    }

    /// a new connection starts unauthenticated again.
    pub fn close(&mut self) {
        self.authed = false;
    }

    /// auth failed for good: everything queued is lost — return it to report.
    pub fn fail(&mut self) -> Vec<Send> {
        self.authed = false;
        std::mem::take(&mut self.pending)
    }
}

/// spawn the twitch sender. returns the send handle plus a note channel — auth
/// failures and twitch NOTICE rejections (followers-only, banned, slow mode…)
/// surface there instead of vanishing. the thread runs until the handle drops.
pub fn spawn(user: String, oauth: String) -> (Sender<Send>, Receiver<Note>) {
    let (tx, rx) = mpsc::channel();
    let (note_tx, note_rx) = mpsc::channel();
    thread::spawn(move || run(user, oauth, rx, note_tx));
    (tx, note_rx)
}

fn run(user: String, oauth: String, rx: Receiver<Send>, notes: Sender<Note>) {
    let mut backoff = Duration::from_secs(1);
    let mut gate = Gate::default();
    loop {
        gate.close();
        match session(&user, &oauth, &rx, &notes, &mut gate) {
            End::Closed => return, // app dropped the sender
            End::AuthFailed => {
                // permanent — reconnecting with the same token just loops.
                // fail loud: the connection, then every send that never left.
                let _ = notes.send(Note {
                    channel: None,
                    text: "twitch auth failed — run: heatsync login".into(),
                });
                for (chan, _) in gate.fail() {
                    let _ = notes.send(Note {
                        channel: Some(chan.to_lowercase()),
                        text: "not sent — twitch auth failed".into(),
                    });
                }
                // anything typed from now on is refused the same way.
                while let Ok((chan, _)) = rx.recv() {
                    let _ = notes.send(Note {
                        channel: Some(chan.to_lowercase()),
                        text: "not sent — twitch auth failed".into(),
                    });
                }
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

fn session(
    user: &str,
    oauth: &str,
    rx: &Receiver<Send>,
    notes: &Sender<Note>,
    gate: &mut Gate,
) -> End {
    let mut ws = match tungstenite::connect(IRC_URL) {
        Ok((ws, _)) => ws,
        Err(_) => return End::Dropped,
    };
    if let Some(sock) = tcp_of(&mut ws) {
        let _ = sock.set_read_timeout(Some(READ_TIMEOUT));
    }
    // twitch's ws-irc endpoint wants ONE irc command per frame — a combined
    // frame gets the connection dropped before auth. CAP first: the tags cap is
    // what puts `msg-id` on NOTICEs, which is how a rejection gets named.
    for cmd in [
        "CAP REQ :twitch.tv/tags".to_string(),
        format!("PASS oauth:{oauth}"),
        format!("NICK {}", user.to_lowercase()),
    ] {
        if ws.send(WsMsg::Text(format!("{cmd}\r\n"))).is_err() {
            return End::Dropped;
        }
    }

    // twitch silently drops PRIVMSG to a channel you haven't JOINed, so join each
    // channel once (per connection) the first time we post to it.
    let mut joined: HashSet<String> = HashSet::new();

    loop {
        match ws.read() {
            Ok(WsMsg::Text(t)) => {
                for line in t.as_str().lines() {
                    // twitch pings periodically; must reply or we get dropped.
                    if line.starts_with("PING") {
                        let _ = ws.send(WsMsg::Text("PONG :tmi.twitch.tv\r\n".into()));
                    } else if is_welcome(line) {
                        for (chan, text) in gate.open() {
                            if post(&mut ws, &mut joined, &chan, &text).is_err() {
                                return End::Dropped;
                            }
                        }
                    } else if is_auth_failure(line) {
                        return End::AuthFailed;
                    } else if let Some(note) = classify_notice(line) {
                        let _ = notes.send(note);
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

        // drain queued sends — the gate holds them until the welcome lands.
        loop {
            match rx.try_recv() {
                Ok(send) => {
                    if let Some((chan, text)) = gate.submit(send) {
                        if post(&mut ws, &mut joined, &chan, &text).is_err() {
                            return End::Dropped;
                        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_ext_msg_id_has_words() {
        for id in [
            "msg_followersonly", "msg_followersonly_followed", "msg_followersonly_zero",
            "msg_subsonly", "msg_emoteonly", "msg_slowmode", "msg_r9k", "msg_duplicate",
            "msg_banned", "msg_timedout", "msg_rejected", "msg_rejected_mandatory",
            "msg_channel_suspended", "msg_channel_blocked", "msg_verified_email",
            "msg_requires_verified_phone_number", "no_permission", "unrecognized_cmd", "tos_ban",
        ] {
            assert!(notice_text(id).is_some(), "{id}");
        }
        assert_eq!(notice_text("msg_slowmode"), Some("slow mode — please wait a moment"));
        assert_eq!(notice_text("host_on"), None);
    }

    #[test]
    fn notice_names_channel_and_reason() {
        let l = "@msg-id=msg_banned :tmi.twitch.tv NOTICE #Streamer :You are permanently banned from talking in streamer.";
        assert_eq!(
            classify_notice(l),
            Some(Note {
                channel: Some("streamer".into()),
                text: "you are banned from this channel".into()
            })
        );
    }

    #[test]
    fn unknown_refusals_pass_twitchs_own_words_but_chatter_stays_quiet() {
        let l = "@msg-id=msg_something_new :tmi.twitch.tv NOTICE #c :Nope : really.";
        assert_eq!(classify_notice(l).unwrap().text, "twitch: Nope : really.");
        assert_eq!(classify_notice("@msg-id=host_on :tmi.twitch.tv NOTICE #c :Now hosting x."), None);
        assert_eq!(classify_notice(":tmi.twitch.tv 001 me :Welcome"), None);
        assert_eq!(classify_notice("@msg-id=msg_banned :tmi.twitch.tv NOTICE * :x").unwrap().channel, None);
    }

    #[test]
    fn auth_failure_and_welcome_detection() {
        assert!(is_auth_failure(":tmi.twitch.tv NOTICE * :Login authentication failed"));
        assert!(is_auth_failure(":tmi.twitch.tv NOTICE * :Improperly formatted auth"));
        assert!(is_welcome(":tmi.twitch.tv 001 me :Welcome, GLHF!"));
        assert!(!is_welcome("PING :tmi.twitch.tv"));
    }

    fn s(c: &str, t: &str) -> Send {
        (c.into(), t.into())
    }

    #[test]
    fn gate_queues_until_welcome_then_releases_in_order() {
        let mut g = Gate::default();
        assert_eq!(g.submit(s("a", "1")), None);
        assert_eq!(g.submit(s("a", "2")), None);
        assert_eq!(g.open(), vec![s("a", "1"), s("a", "2")]);
        assert_eq!(g.submit(s("a", "3")), Some(s("a", "3")));
    }

    #[test]
    fn gate_keeps_queue_across_a_reconnect_and_fails_loud() {
        let mut g = Gate::default();
        g.open();
        g.close(); // connection dropped
        assert_eq!(g.submit(s("a", "1")), None, "unauthenticated again");
        assert_eq!(g.fail(), vec![s("a", "1")]);
        assert_eq!(g.fail(), vec![], "nothing left to report twice");
    }
}
