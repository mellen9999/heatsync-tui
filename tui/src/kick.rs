//! direct kick chat sending + kick OAuth (PKCE). like the twitch path, the
//! client sends straight to kick's public API with the user's own token
//! (chat:write), independent of heatsync's extension-relay. `heatsync login kick`
//! runs the OAuth authorization-code + PKCE flow (loopback redirect) to obtain a
//! token — kick has no token generator, so we do it properly.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::config;

const API: &str = "https://api.kick.com/public/v1";
const ID: &str = "https://id.kick.com/oauth";
const REDIRECT: &str = "http://localhost:8991/callback";

/// (channel slug, text) to post.
pub type Send = (String, String);

/// spawn the kick sender; runs until the handle is dropped.
pub fn spawn(token: String) -> Sender<Send> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || run(token, rx));
    tx
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(12))
        .build()
}

fn run(token: String, rx: Receiver<Send>) {
    let mut ids: HashMap<String, u64> = HashMap::new();
    while let Ok((slug, text)) = rx.recv() {
        let id = match ids.get(&slug) {
            Some(&id) => id,
            None => match resolve(&token, &slug) {
                Some(id) => {
                    ids.insert(slug.clone(), id);
                    id
                }
                None => continue,
            },
        };
        let _ = post(&token, id, &text);
    }
}

/// slug → numeric broadcaster_user_id (kick's send API needs the id, not slug).
fn resolve(token: &str, slug: &str) -> Option<u64> {
    let url = format!("{API}/channels?slug={slug}");
    let v: Value = agent()
        .get(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .call()
        .ok()?
        .into_json()
        .ok()?;
    let d = v.get("data")?;
    let obj = if d.is_array() { d.get(0)? } else { d };
    obj.get("broadcaster_user_id").and_then(Value::as_u64)
}

fn post(token: &str, id: u64, text: &str) -> Result<(), ureq::Error> {
    agent()
        .post(&format!("{API}/chat"))
        .set("Authorization", &format!("Bearer {token}"))
        .send_json(json!({ "content": text, "type": "user", "broadcaster_user_id": id }))?;
    Ok(())
}

// ---- OAuth (PKCE) ---------------------------------------------------------

/// `heatsync login kick` — authorization-code + PKCE flow via a loopback redirect.
pub fn login() -> std::io::Result<()> {
    let client_id = match std::env::var("KICK_CLIENT_ID")
        .ok()
        .filter(|s| !s.is_empty())
    {
        Some(c) => c,
        None => {
            println!("kick sending needs a kick app (kick has no token generator):");
            println!("  1. create an app at https://kick.com/settings/developer");
            println!("     redirect URL: {REDIRECT}   scopes: chat:write, channel:read");
            println!("  2. export KICK_CLIENT_ID=<your app client id>");
            println!("  3. re-run: heatsync login kick");
            return Ok(());
        }
    };

    let verifier = rand_b64url(32);
    let challenge = b64url(&sha256(verifier.as_bytes()));
    let state = rand_b64url(16);
    let auth_url = format!(
        "{ID}/authorize?response_type=code&client_id={client_id}&redirect_uri={}&scope={}&code_challenge={challenge}&code_challenge_method=S256&state={state}",
        pct(REDIRECT),
        pct("chat:write channel:read"),
    );

    let listener = TcpListener::bind("127.0.0.1:8991")?;
    println!("authorize kick in your browser. if it doesn't open, visit:\n\n{auth_url}\n");
    let _ = Command::new("xdg-open").arg(&auth_url).spawn();

    let code = match wait_for_code(&listener, &state) {
        Some(c) => c,
        None => {
            eprintln!("no valid authorization received");
            return Ok(());
        }
    };
    match exchange(&client_id, &code, &verifier) {
        Some(tok) => {
            config::save_kick_token(&tok);
            println!("kick linked ✓ — restart heatsync and kick sends go direct.");
        }
        None => eprintln!("token exchange failed"),
    }
    Ok(())
}

/// block until the loopback redirect hits with a matching state; return `code`.
fn wait_for_code(listener: &TcpListener, state: &str) -> Option<String> {
    let (mut stream, _) = listener.accept().ok()?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line).ok()?;
    // "GET /callback?code=...&state=... HTTP/1.1"
    let target = line.split_whitespace().nth(1)?;
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    let mut code = None;
    let mut got_state = None;
    for kv in query.split('&') {
        match kv.split_once('=') {
            Some(("code", v)) => code = Some(v.to_string()),
            Some(("state", v)) => got_state = Some(v.to_string()),
            _ => {}
        }
    }
    let body = "<html><body style='font-family:monospace;background:#111;color:#ff8700'>heatsync: kick linked. you can close this tab.</body></html>";
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    if got_state.as_deref() == Some(state) {
        code
    } else {
        None // CSRF guard: state mismatch
    }
}

fn exchange(client_id: &str, code: &str, verifier: &str) -> Option<String> {
    let v: Value = agent()
        .post(&format!("{ID}/token"))
        .send_form(&[
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("redirect_uri", REDIRECT),
            ("code_verifier", verifier),
            ("code", code),
        ])
        .ok()?
        .into_json()
        .ok()?;
    v.get("access_token")
        .and_then(Value::as_str)
        .map(str::to_string)
}

// ---- small crypto/encoding helpers ---------------------------------------

fn sha256(bytes: &[u8]) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().to_vec()
}

/// URL-safe base64 without padding (RFC 4648 §5).
fn b64url(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        for i in 0..(chunk.len() + 1) {
            out.push(T[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
        }
    }
    out
}

fn rand_b64url(n: usize) -> String {
    let mut buf = vec![0u8; n];
    // security-critical (PKCE verifier + CSRF state): never fall back to zeroed
    // bytes — abort the flow loudly if the CSPRNG is unavailable.
    getrandom::getrandom(&mut buf)
        .expect("CSPRNG unavailable — cannot generate secure OAuth values");
    b64url(&buf)
}

/// percent-encode a query/redirect value.
fn pct(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_rfc7636_vector() {
        // RFC 7636 appendix B: verifier → S256 challenge.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = b64url(&sha256(verifier.as_bytes()));
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn b64url_is_unpadded_urlsafe() {
        assert_eq!(b64url(b"foobar"), "Zm9vYmFy");
        assert_eq!(b64url(b"fo"), "Zm8"); // no padding
        assert!(!b64url(&[0xff, 0xff, 0xff]).contains(['+', '/', '=']));
    }
}
