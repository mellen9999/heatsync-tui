//! "log in with heatsync" — the terminal's one login.
//!
//! the terminal never holds a twitch or kick token. it asks heatsync.org for a
//! pairing code, the person approves it on the site, and the terminal keeps ONE
//! heatsync token (scoped to this client, revocable, expiring) in
//! ~/.config/heatsync/session at 0600. sends and mod actions go through
//! heatsync.org, which holds the platform tokens server-side — same as the
//! extension. nothing here ever prints or logs the token.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use heatsync_core::Platform;
use serde_json::{json, Value};

use crate::config;

pub const DEFAULT_BASE: &str = "https://heatsync.org";
/// rotate the token once it has less than this long left.
const ROTATE_WITHIN_SECS: i64 = 7 * 24 * 3600;

// ---- base url -------------------------------------------------------------

/// the server to talk to: `HEATSYNC_URL` (for a local test server) or heatsync.org.
/// the token rides on every request, so plain http is only allowed to loopback.
pub fn base_url() -> Result<String, String> {
    match std::env::var("HEATSYNC_URL") {
        Ok(v) if !v.trim().is_empty() => clean_base(&v),
        _ => Ok(DEFAULT_BASE.to_string()),
    }
}

pub fn clean_base(raw: &str) -> Result<String, String> {
    let s = raw.trim().trim_end_matches('/');
    let rest = if let Some(r) = s.strip_prefix("https://") {
        r
    } else if let Some(r) = s.strip_prefix("http://") {
        let host = r.split(['/', ':']).next().unwrap_or("");
        if !matches!(host, "localhost" | "127.0.0.1" | "[::1]") {
            return Err(format!(
                "{s} is plain http — your login would cross the network unencrypted. use https."
            ));
        }
        r
    } else {
        return Err(format!("{s} is not a web address (start it with https://)"));
    };
    let authority = rest.split('/').next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') || s.contains(char::is_whitespace) {
        return Err(format!("{s} is not a usable address"));
    }
    Ok(s.to_string())
}

// ---- the session file -----------------------------------------------------

/// what a login leaves on disk. Debug is hand-written so a stray `{:?}` can never
/// print the token; there is no Display at all.
#[derive(Clone, PartialEq)]
pub struct Session {
    token: String,
    pub base: String,
    pub expires_at: String,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("token", &"[REDACTED]")
            .field("base", &self.base)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl Session {
    pub fn new(token: String, base: String, expires_at: String) -> Session {
        Session {
            token,
            base,
            expires_at,
        }
    }
}

fn session_path() -> Option<PathBuf> {
    Some(config::dir()?.join("session"))
}

fn serialize(s: &Session) -> String {
    format!(
        "token={}\nbase={}\nexpires_at={}\n",
        s.token, s.base, s.expires_at
    )
}

fn parse_session(text: &str) -> Option<Session> {
    let (mut token, mut base, mut exp) = (None, None, None);
    for line in text.lines() {
        match line.split_once('=') {
            Some(("token", v)) => token = Some(v.trim().to_string()),
            Some(("base", v)) => base = Some(v.trim().to_string()),
            Some(("expires_at", v)) => exp = Some(v.trim().to_string()),
            _ => {}
        }
    }
    let token = token.filter(|t| !t.is_empty())?;
    Some(Session {
        token,
        base: clean_base(&base?).ok()?,
        expires_at: exp.unwrap_or_default(),
    })
}

/// write the session so it is NEVER readable by anyone else, not even for an
/// instant: a fresh 0600 file, then an atomic rename over the old one.
pub fn save_session(s: &Session) -> std::io::Result<()> {
    let path = session_path().ok_or_else(|| std::io::Error::other("no config dir"))?;
    save_session_at(&path, s)
}

fn save_session_at(path: &std::path::Path, s: &Session) -> std::io::Result<()> {
    if let Some(d) = path.parent() {
        fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("tmp");
    let _ = fs::remove_file(&tmp);
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    f.write_all(serialize(s).as_bytes())?;
    f.sync_all()?;
    fs::rename(&tmp, path)
}

/// the saved login, if any. a file anyone else can read is refused: that token
/// is as good as the person, so a loose file is treated as compromised.
pub fn load_session() -> Option<Session> {
    let path = session_path()?;
    load_session_at(&path)
}

fn load_session_at(path: &std::path::Path) -> Option<Session> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path).ok()?.permissions().mode();
        if mode & 0o077 != 0 {
            eprintln!(
                "heatsync: {} is readable by other users — run: chmod 600 {}  (or log in again)",
                path.display(),
                path.display()
            );
            return None;
        }
    }
    parse_session(&fs::read_to_string(path).ok()?)
}

pub fn delete_session() -> bool {
    session_path().is_some_and(|p| fs::remove_file(p).is_ok())
}

// ---- http -----------------------------------------------------------------

/// one request/response, so the whole client can be tested against a fake.
pub trait Transport: Send + Sync {
    /// `Ok((status, json body))` for any answer the server gave (even 4xx/5xx);
    /// `Err(words)` only when no answer arrived.
    fn call(
        &self,
        method: &str,
        url: &str,
        bearer: Option<&str>,
        body: Option<&Value>,
    ) -> Result<(u16, Value), String>;
}

pub struct UreqTransport;

impl Transport for UreqTransport {
    fn call(
        &self,
        method: &str,
        url: &str,
        bearer: Option<&str>,
        body: Option<&Value>,
    ) -> Result<(u16, Value), String> {
        // no redirects: an api answer that bounces elsewhere is wrong, and the
        // token must never follow it to another host.
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(15))
            .redirects(0)
            .user_agent("Mozilla/5.0 (heatsync-tui/0.1; +https://heatsync.org)")
            .build();
        let mut req = agent.request(method, url).set("Accept", "application/json");
        if let Some(t) = bearer {
            req = req.set("Authorization", &format!("Bearer {t}"));
        }
        let res = match body {
            Some(b) => req.send_json(b.clone()),
            None => req.call(),
        };
        match res {
            Ok(r) => {
                let s = r.status();
                Ok((s, r.into_json().unwrap_or(Value::Null)))
            }
            Err(ureq::Error::Status(c, r)) => Ok((c, r.into_json().unwrap_or(Value::Null))),
            Err(_) => Err("can't reach heatsync — check your connection and try again".into()),
        }
    }
}

// ---- the pairing flow -----------------------------------------------------

#[derive(Debug, PartialEq)]
pub struct Pair {
    pub device_code: String,
    pub user_code: String,
    pub url: String,
    pub interval: u64,
    pub expires_in: u64,
}

pub fn pair_start(t: &dyn Transport, base: &str) -> Result<Pair, String> {
    let (st, v) = t.call(
        "POST",
        &format!("{base}/api/cli/pair/start"),
        None,
        Some(&json!({ "label": label() })),
    )?;
    if st != 200 {
        return Err(explain(st, &v, base));
    }
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    Ok(Pair {
        device_code: s("device_code").ok_or("heatsync sent an odd answer — try again")?,
        user_code: s("user_code").ok_or("heatsync sent an odd answer — try again")?,
        // built from OUR base, never taken from the reply: the page you are told to
        // type a code into is the server you are talking to.
        url: format!("{base}/cli"),
        interval: v
            .get("interval")
            .and_then(Value::as_u64)
            .unwrap_or(3)
            .clamp(1, 30),
        expires_in: v.get("expires_in").and_then(Value::as_u64).unwrap_or(600),
    })
}

/// what shows on the approve page so you can tell it is YOUR terminal.
fn label() -> String {
    let host = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| fs::read_to_string("/etc/hostname").ok())
        .unwrap_or_default();
    let host: String = host
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '.')
        .take(40)
        .collect();
    if host.is_empty() {
        "heatsync-tui".into()
    } else {
        format!("heatsync-tui on {host}")
    }
}

#[derive(Debug, PartialEq)]
pub enum Poll {
    Pending,
    SlowDown,
    Approved(Session),
    Failed(String),
}

pub fn poll_once(t: &dyn Transport, base: &str, device_code: &str) -> Poll {
    let r = t.call(
        "POST",
        &format!("{base}/api/cli/pair/poll"),
        None,
        Some(&json!({ "device_code": device_code })),
    );
    let (st, v) = match r {
        Ok(x) => x,
        Err(e) => return Poll::Failed(e),
    };
    match (st, v.get("status").and_then(Value::as_str)) {
        (200, Some("pending")) => Poll::Pending,
        (200, Some("approved")) => match v.get("token").and_then(Value::as_str) {
            Some(tok) => Poll::Approved(Session::new(
                tok.to_string(),
                base.to_string(),
                v.get("expires_at")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            )),
            None => Poll::Failed("heatsync sent an odd answer — try logging in again".into()),
        },
        (429, _) => Poll::SlowDown,
        (410, _) => Poll::Failed("that code expired — run the login again".into()),
        _ => Poll::Failed(explain(st, &v, base)),
    }
}

// ---- the signed-in client -------------------------------------------------

/// cheap to clone; every send/mod call runs on its own thread with a clone.
#[derive(Clone)]
pub struct Client {
    base: String,
    token: String,
    t: Arc<dyn Transport>,
}

/// the answer to a mod/send call, ready to show: `ok` colors nothing, it just
/// decides whether the line is a confirmation or a problem.
#[derive(Debug, PartialEq)]
pub struct Outcome {
    pub ok: bool,
    pub text: String,
}

#[derive(Debug, PartialEq)]
pub enum ModAction {
    Ban,
    Unban,
    Timeout(u32),
    Delete,
}

impl Client {
    pub fn new(s: &Session, t: Arc<dyn Transport>) -> Client {
        Client {
            base: s.base.clone(),
            token: s.token.clone(),
            t,
        }
    }

    fn call(&self, method: &str, path: &str, body: Option<Value>) -> Result<(u16, Value), String> {
        self.t.call(
            method,
            &format!("{}{path}", self.base),
            Some(&self.token),
            body.as_ref(),
        )
    }

    /// who am i. Ok(json) on success, Err(plain words) otherwise.
    pub fn me(&self) -> Result<Value, String> {
        let (st, v) = self.call("GET", "/api/cli/me", None)?;
        if st == 200 {
            Ok(v)
        } else {
            Err(explain(st, &v, &self.base))
        }
    }

    pub fn rotate(&self) -> Result<Session, String> {
        let (st, v) = self.call("POST", "/api/cli/rotate", Some(json!({})))?;
        match (st, v.get("token").and_then(Value::as_str)) {
            (200, Some(tok)) => Ok(Session::new(
                tok.to_string(),
                self.base.clone(),
                v.get("expires_at")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            )),
            _ => Err(explain(st, &v, &self.base)),
        }
    }

    /// tell the server to kill this token. best effort: the file goes either way.
    pub fn revoke(&self) -> bool {
        matches!(self.call("DELETE", "/api/cli/token", None), Ok((200, _)))
    }

    pub fn mod_action(
        &self,
        platform: Platform,
        channel: &str,
        target: &str,
        action: &ModAction,
    ) -> Outcome {
        if platform != Platform::Twitch {
            return Outcome {
                ok: false,
                text: "mod tools aren't on kick yet — use kick.com for that".into(),
            };
        }
        let body = mod_body(channel, target, action);
        match self.call("POST", "/api/cli/mod", Some(body)) {
            Ok((200, _)) => Outcome {
                ok: true,
                text: mod_done(action, target),
            },
            Ok((st, v)) => Outcome {
                ok: false,
                text: explain(st, &v, &self.base),
            },
            Err(e) => Outcome { ok: false, text: e },
        }
    }

    /// send chat through heatsync. Ok = posted (nothing to show), Err = words for the user.
    pub fn send(&self, platform: Platform, channel: &str, text: &str) -> Result<(), String> {
        let path = match platform {
            Platform::Twitch => "/api/cli/twitch/send",
            Platform::Kick => "/api/cli/kick/send",
            Platform::Youtube => return Err("youtube sends need the extension".into()),
        };
        match self.call(
            "POST",
            path,
            Some(json!({ "channel": channel, "text": text })),
        ) {
            Ok((200, _)) => Ok(()),
            Ok((st, v)) => Err(explain(st, &v, &self.base)),
            Err(e) => Err(e),
        }
    }
}

pub fn mod_body(channel: &str, target: &str, action: &ModAction) -> Value {
    let mut b = json!({ "platform": "twitch", "channel": channel });
    match action {
        ModAction::Ban => {
            b["action"] = json!("ban");
            b["target"] = json!(target);
        }
        ModAction::Unban => {
            b["action"] = json!("unban");
            b["target"] = json!(target);
        }
        ModAction::Timeout(secs) => {
            b["action"] = json!("timeout");
            b["target"] = json!(target);
            b["duration_seconds"] = json!(secs);
        }
        ModAction::Delete => {
            b["action"] = json!("delete_message");
            b["message_id"] = json!(target);
        }
    }
    b
}

fn mod_done(action: &ModAction, target: &str) -> String {
    match action {
        ModAction::Ban => format!("banned {target}"),
        ModAction::Unban => format!("unbanned {target}"),
        ModAction::Timeout(s) => format!("timed out {target} for {}", human_secs(*s)),
        ModAction::Delete => "deleted that message".into(),
    }
}

pub fn human_secs(s: u32) -> String {
    match s {
        s if s % 86400 == 0 => format!("{}d", s / 86400),
        s if s % 3600 == 0 => format!("{}h", s / 3600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// a server error → one plain sentence, with the exact link when the fix is
/// "link or upgrade on heatsync.org". never echoes anything token-shaped.
pub fn explain(status: u16, v: &Value, base: &str) -> String {
    let err = v.get("error").and_then(Value::as_str).unwrap_or("");
    let code = v.get("error_code").and_then(Value::as_str).unwrap_or("");
    let relink = v
        .get("relink_required")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let pack = v.get("scope_pack").and_then(Value::as_str);
    let platform = v
        .get("platform")
        .and_then(Value::as_str)
        .unwrap_or("twitch");
    if code == "cli_token_invalid" {
        return "you're not logged in (or it expired) — run: heatsync-tui login".into();
    }
    if code == "not_mod" {
        return "you're not a moderator in this channel".into();
    }
    if code == "scope_missing" && !relink {
        return "this login can't do that — run: heatsync-tui login".into();
    }
    if relink || code == "not_linked" {
        let url = match (platform, pack) {
            ("kick", Some(p)) => format!("{base}/api/auth/kick/login?scopes={p}"),
            ("kick", None) => format!("{base}/api/auth/kick/login"),
            (_, Some(p)) => format!("{base}/api/auth/login?scopes={p}"),
            (_, None) => format!("{base}/api/auth/login"),
        };
        let what = match pack {
            Some("mod") => "allow mod tools",
            Some("chatsend") => "allow chat from the terminal",
            _ => "link your account",
        };
        return format!("{what} on {platform} first — open {url}");
    }
    match status {
        429 => "too fast — wait a moment".into(),
        s if s >= 500 => "heatsync had a problem — try again in a minute".into(),
        _ if !err.is_empty() => err.chars().take(160).collect(),
        s => format!("heatsync said no ({s})"),
    }
}

// ---- time -----------------------------------------------------------------

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `2026-10-04T20:00:00.000Z` → unix seconds. only the shape the server sends.
pub fn parse_iso(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (hh, mm, ss) = (n(11..13)?, n(14..16)?, n(17..19)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // days from civil (Howard Hinnant)
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + hh * 3600 + mm * 60 + ss)
}

pub fn needs_rotation(expires_at: &str, now: i64) -> bool {
    parse_iso(expires_at).is_some_and(|e| e - now < ROTATE_WITHIN_SECS)
}

// ---- startup --------------------------------------------------------------

/// the signed-in client for the TUI, if a login exists. renews the token when it
/// is close to expiring; if the server says it's dead, tells the user once.
pub fn load_client() -> Option<Client> {
    let s = load_session()?;
    let t: Arc<dyn Transport> = Arc::new(UreqTransport);
    let c = Client::new(&s, t.clone());
    if needs_rotation(&s.expires_at, now_secs()) {
        match c.rotate() {
            Ok(n) => {
                let _ = save_session(&n);
                return Some(Client::new(&n, t));
            }
            Err(e) if e.contains("heatsync-tui login") => {
                eprintln!("heatsync: {e}");
                return None;
            }
            Err(_) => {} // offline right now: keep the current token, try next launch
        }
    }
    Some(c)
}

/// a finished send/mod call → the tab note the UI shows.
pub struct Note {
    pub platform: Platform,
    pub channel: String,
    pub text: String,
}

/// run `f` off the UI thread and post its words back as a note.
pub fn spawn_note<F>(tx: Sender<Note>, platform: Platform, channel: String, f: F)
where
    F: FnOnce() -> Outcome + Send + 'static,
{
    std::thread::spawn(move || {
        let o = f();
        let _ = tx.send(Note {
            platform,
            channel,
            text: o.text,
        });
    });
}

// ---- tests ----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// canned answers, and a record of every request (to prove what was sent).
    type Seen = (String, String, Option<String>, Option<Value>);
    struct Fake {
        replies: Mutex<Vec<Result<(u16, Value), String>>>,
        seen: Mutex<Vec<Seen>>,
    }
    impl Fake {
        fn new(r: Vec<Result<(u16, Value), String>>) -> Arc<Fake> {
            Arc::new(Fake {
                replies: Mutex::new(r),
                seen: Mutex::new(vec![]),
            })
        }
    }
    impl Transport for Fake {
        fn call(
            &self,
            m: &str,
            u: &str,
            b: Option<&str>,
            body: Option<&Value>,
        ) -> Result<(u16, Value), String> {
            self.seen.lock().unwrap().push((
                m.into(),
                u.into(),
                b.map(str::to_string),
                body.cloned(),
            ));
            self.replies.lock().unwrap().remove(0)
        }
    }

    fn client(f: &Arc<Fake>) -> Client {
        Client::new(
            &Session::new(
                "hscli_SECRET".into(),
                "https://h.test".into(),
                String::new(),
            ),
            f.clone(),
        )
    }

    #[test]
    fn base_must_be_https_or_loopback() {
        assert_eq!(
            clean_base("https://heatsync.org/").unwrap(),
            "https://heatsync.org"
        );
        assert_eq!(
            clean_base("http://127.0.0.1:3096").unwrap(),
            "http://127.0.0.1:3096"
        );
        assert_eq!(
            clean_base("http://localhost:3001/").unwrap(),
            "http://localhost:3001"
        );
        assert!(clean_base("http://heatsync.org").is_err());
        assert!(clean_base("http://localhost.evil.com").is_err());
        assert!(clean_base("https://user@evil.com").is_err());
        assert!(clean_base("heatsync.org").is_err());
        assert!(clean_base("https://").is_err());
    }

    #[test]
    fn debug_never_shows_the_token() {
        let s = Session::new("hscli_SECRET".into(), "https://h.test".into(), "x".into());
        assert!(!format!("{s:?}").contains("hscli_SECRET"));
        assert!(format!("{s:?}").contains("REDACTED"));
    }

    #[test]
    fn session_file_is_0600_roundtrips_and_loose_perms_are_refused() {
        let dir = std::env::temp_dir().join(format!("hs-sess-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let p = dir.join("session");
        let s = Session::new(
            "hscli_ABC".into(),
            "https://h.test".into(),
            "2026-10-04T20:00:00.000Z".into(),
        );
        save_session_at(&p, &s).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(load_session_at(&p), Some(s.clone()));
        // overwriting keeps 0600 and leaves no temp file behind
        save_session_at(&p, &s).unwrap();
        assert!(!p.with_extension("tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                0o600
            );
            fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
            assert_eq!(load_session_at(&p), None);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn iso_parse_and_rotation_window() {
        assert_eq!(parse_iso("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(parse_iso("2026-10-04T20:00:00.000Z"), Some(1791144000));
        assert_eq!(parse_iso("nonsense"), None);
        let now = parse_iso("2026-10-04T20:00:00Z").unwrap();
        assert!(needs_rotation("2026-10-08T20:00:00.000Z", now));
        assert!(!needs_rotation("2026-11-04T20:00:00.000Z", now));
        assert!(!needs_rotation("", now)); // unknown expiry: leave it alone
    }

    #[test]
    fn pair_start_reads_codes_and_clamps_interval() {
        let f = Fake::new(vec![Ok((
            200,
            json!({"device_code":"dev","user_code":"ABCD-EFGH","verification_uri":"https://h.test/cli","interval":999,"expires_in":600}),
        ))]);
        let p = pair_start(f.as_ref(), "https://h.test").unwrap();
        assert_eq!(p.user_code, "ABCD-EFGH");
        assert_eq!(p.interval, 30);
        let seen = f.seen.lock().unwrap();
        assert_eq!(seen[0].1, "https://h.test/api/cli/pair/start");
        assert_eq!(seen[0].2, None); // no credentials on the start call
    }

    #[test]
    fn poll_maps_every_server_state() {
        let one = |st: u16, v: Value| {
            poll_once(
                Fake::new(vec![Ok((st, v))]).as_ref(),
                "https://h.test",
                "dev",
            )
        };
        assert_eq!(one(200, json!({"status":"pending"})), Poll::Pending);
        assert_eq!(one(429, json!({"error_code":"slow_down"})), Poll::SlowDown);
        assert!(matches!(one(410, json!({})), Poll::Failed(m) if m.contains("expired")));
        match one(
            200,
            json!({"status":"approved","token":"hscli_T","expires_at":"2026-11-03T00:00:00.000Z"}),
        ) {
            Poll::Approved(s) => {
                assert_eq!(s.expires_at, "2026-11-03T00:00:00.000Z");
                assert_eq!(s.base, "https://h.test");
            }
            o => panic!("{o:?}"),
        }
        assert!(matches!(
            poll_once(Fake::new(vec![Err("down".into())]).as_ref(), "b", "d"),
            Poll::Failed(_)
        ));
    }

    #[test]
    fn mod_requests_are_built_right_and_carry_the_bearer() {
        let f = Fake::new(
            vec![Ok((200, json!({"success":true}))); 4]
                .into_iter()
                .collect(),
        );
        let c = client(&f);
        assert!(
            c.mod_action(Platform::Twitch, "chan", "troll", &ModAction::Ban)
                .ok
        );
        assert!(
            c.mod_action(Platform::Twitch, "chan", "troll", &ModAction::Timeout(600))
                .ok
        );
        assert!(
            c.mod_action(Platform::Twitch, "chan", "troll", &ModAction::Unban)
                .ok
        );
        let o = c.mod_action(Platform::Twitch, "chan", "abc-id", &ModAction::Delete);
        assert_eq!(o.text, "deleted that message");
        let seen = f.seen.lock().unwrap();
        assert_eq!(seen[0].1, "https://h.test/api/cli/mod");
        assert_eq!(seen[0].2.as_deref(), Some("hscli_SECRET"));
        assert_eq!(
            seen[0].3,
            Some(json!({"platform":"twitch","channel":"chan","action":"ban","target":"troll"}))
        );
        assert_eq!(seen[1].3.as_ref().unwrap()["duration_seconds"], 600);
        assert_eq!(
            seen[3].3,
            Some(
                json!({"platform":"twitch","channel":"chan","action":"delete_message","message_id":"abc-id"})
            )
        );
    }

    #[test]
    fn kick_mod_says_not_yet_without_any_request() {
        let f = Fake::new(vec![]);
        let o = client(&f).mod_action(Platform::Kick, "chan", "x", &ModAction::Ban);
        assert!(!o.ok && o.text.contains("aren't on kick yet"));
        assert!(f.seen.lock().unwrap().is_empty());
    }

    #[test]
    fn errors_read_in_plain_words_with_the_exact_link() {
        let base = "https://h.test";
        let not_mod = explain(403, &json!({"error_code":"not_mod","error":"x"}), base);
        assert_eq!(not_mod, "you're not a moderator in this channel");
        let scope = explain(
            401,
            &json!({"relink_required":true,"platform":"twitch","scope_pack":"mod"}),
            base,
        );
        assert_eq!(
            scope,
            "allow mod tools on twitch first — open https://h.test/api/auth/login?scopes=mod"
        );
        let send = explain(
            401,
            &json!({"relink_required":true,"platform":"twitch","scope_pack":"chatsend"}),
            base,
        );
        assert!(send.contains("scopes=chatsend") && send.contains("allow chat from the terminal"));
        let unlinked = explain(
            401,
            &json!({"relink_required":true,"platform":"twitch"}),
            base,
        );
        assert_eq!(
            unlinked,
            "link your account on twitch first — open https://h.test/api/auth/login"
        );
        let kick = explain(
            403,
            &json!({"error_code":"not_linked","platform":"kick"}),
            base,
        );
        assert!(kick.contains("/api/auth/kick/login"));
        assert!(
            explain(401, &json!({"error_code":"cli_token_invalid"}), base)
                .contains("heatsync-tui login")
        );
        assert!(explain(502, &json!({}), base).contains("try again"));
        assert_eq!(explain(429, &json!({}), base), "too fast — wait a moment");
    }

    #[test]
    fn a_server_error_never_echoes_the_token() {
        let f = Fake::new(vec![Ok((400, json!({"error":"bad"})))]);
        let o = client(&f).mod_action(Platform::Twitch, "c", "t", &ModAction::Ban);
        assert!(!o.text.contains("hscli_SECRET"));
    }

    #[test]
    fn send_picks_the_platform_endpoint_and_surfaces_errors() {
        let f = Fake::new(vec![
            Ok((200, json!({"success":true}))),
            Ok((200, json!({"success":true}))),
            Ok((400, json!({"error":"slow mode","error_code":"rejected"}))),
        ]);
        let c = client(&f);
        assert!(c.send(Platform::Twitch, "chan", "hi").is_ok());
        assert!(c.send(Platform::Kick, "chan", "hi").is_ok());
        assert_eq!(
            c.send(Platform::Twitch, "chan", "hi"),
            Err("slow mode".into())
        );
        let seen = f.seen.lock().unwrap();
        assert!(seen[0].1.ends_with("/api/cli/twitch/send"));
        assert!(seen[1].1.ends_with("/api/cli/kick/send"));
    }

    #[test]
    fn human_secs_picks_the_biggest_whole_unit() {
        assert_eq!(human_secs(600), "10m");
        assert_eq!(human_secs(90), "90s");
        assert_eq!(human_secs(7200), "2h");
        assert_eq!(human_secs(86400), "1d");
    }
}
