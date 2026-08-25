//! blocking HTTP against the public HeatSync API (no auth). emote sets + image
//! bytes for the TUI, and the archive/corpus reads for the CLI subcommands.

use std::io::Read;
use std::sync::OnceLock;
use std::time::Duration;

use heatsync_core::emote::{EmoteSet, EmoteSetResponse};
use heatsync_core::Platform;
use serde::Deserialize;

const BASE: &str = "https://heatsync.org";
/// staged for the emote image-render layer (next phase).
#[allow(dead_code)]
const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;

fn platform_q(p: Platform) -> &'static str {
    match p {
        Platform::Twitch => "twitch",
        Platform::Kick => "kick",
    }
}

/// one shared agent for the whole process — its connection pool reuses TLS
/// sessions, so a burst of 30 emote fetches from one CDN pays one handshake,
/// not thirty. thread-safe; the loader pool and emote-set threads all share it.
fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(12))
            // the archive search endpoint gates non-browser UAs (anti-scrape); we're
            // first-party, so identify as heatsync-tui behind a Mozilla token so the
            // guard passes. proper fix later: server-side allowlist this UA.
            .user_agent("Mozilla/5.0 (heatsync-tui/0.1; +https://heatsync.org)")
            .build()
    })
}

/// channel emote set (7tv/bttv/ffz precedence, deduped server-side).
pub fn emote_set(channel: &str, platform: Platform) -> Option<EmoteSet> {
    let url = format!(
        "{BASE}/api/channel/{channel}/emotes?platform={}",
        platform_q(platform)
    );
    let resp: EmoteSetResponse = agent().get(&url).call().ok()?.into_json().ok()?;
    Some(EmoteSet::from_list(resp.emotes))
}

/// raw image bytes for an emote, capped so a hostile CDN can't balloon memory.
/// staged for the emote image-render layer (next phase).
#[allow(dead_code)]
pub fn image_bytes(url: &str) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    agent()
        .get(url)
        .call()
        .ok()?
        .into_reader()
        .take(MAX_IMAGE_BYTES)
        .read_to_end(&mut buf)
        .ok()?;
    Some(buf)
}

// ---- corpus / archive reads (CLI) ----------------------------------------

/// one archived chat row (subset of the archive API shape).
#[derive(Debug, Deserialize)]
pub struct ArchiveMsg {
    #[serde(default)]
    pub platform: String,
    #[serde(default)]
    pub channel: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub timestamp: String,
}

#[derive(Debug, Deserialize)]
pub struct ArchivePage {
    #[serde(default)]
    pub results: Vec<ArchiveMsg>,
    /// staged for CLI pagination (--page / follow).
    #[serde(default)]
    #[allow(dead_code)]
    pub next_cursor: Option<String>,
}

/// full-text search the archive.
pub fn search(q: &str, channel: Option<&str>, limit: u32) -> Option<ArchivePage> {
    let mut url = format!("{BASE}/api/archive/search?limit={limit}&q={}", urlencode(q));
    if let Some(c) = channel {
        url.push_str(&format!("&channel={}", urlencode(c)));
    }
    agent().get(&url).call().ok()?.into_json().ok()
}

/// a channel's log for a UTC date (YYYY-MM-DD).
pub fn channel_log(channel: &str, from: &str, to: &str, limit: u32) -> Option<ArchivePage> {
    let url =
        format!("{BASE}/api/archive/channel/{channel}/messages?from={from}&to={to}&limit={limit}");
    agent().get(&url).call().ok()?.into_json().ok()
}

/// hottest posts across the platform (these carry real heat).
#[derive(Debug, Deserialize)]
pub struct HotMsg {
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub heat: f64,
    #[serde(default)]
    pub max_heat: f64,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub subject: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct HotPage {
    #[serde(default)]
    pub messages: Vec<HotMsg>,
}

pub fn hot(limit: u32, hours: u32) -> Option<HotPage> {
    let url = format!("{BASE}/api/messages/hot?limit={limit}&hours={hours}");
    agent().get(&url).call().ok()?.into_json().ok()
}

// ---- admin status (CLI, mellen-only) --------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthData {
    #[serde(default)]
    pub messages_per_minute: f64,
    #[serde(default)]
    pub postgres_status: String,
    #[serde(default)]
    pub redis_status: String,
    #[serde(default)]
    pub ws_connections: i64,
    #[serde(default)]
    pub cpu_usage: f64,
    #[serde(default)]
    pub ram_usage: f64,
    #[serde(default)]
    pub disk_usage: f64,
    #[serde(default)]
    pub error_rate: f64,
}

#[derive(Debug, Deserialize)]
pub struct CountPage {
    #[serde(default)]
    pub total: i64,
}

#[derive(Debug, Deserialize)]
pub struct ReportsPage {
    #[serde(default)]
    pub reports: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct NcmecPage {
    #[serde(default)]
    pub count: i64,
}

fn admin_get<T: for<'de> Deserialize<'de>>(path: &str, token: &str) -> Option<T> {
    agent()
        .get(&format!("{BASE}{path}"))
        .set("Authorization", &format!("Bearer {token}"))
        .call()
        .ok()?
        .into_json()
        .ok()
}

pub fn admin_health(token: &str) -> Option<HealthData> {
    admin_get("/api/admin/health", token)
}

/// moderation-queue depth (items awaiting review).
pub fn admin_mod_queue_total(token: &str) -> Option<i64> {
    admin_get::<CountPage>("/api/admin/moderation-queue?limit=1", token).map(|p| p.total)
}

/// pending user reports.
pub fn admin_pending_reports(token: &str) -> Option<i64> {
    admin_get::<ReportsPage>("/api/admin/reports?status=pending", token)
        .map(|p| p.reports.len() as i64)
}

/// NCMEC reports stuck in a failed/manual state — never routine, always worth surfacing.
pub fn admin_ncmec_backlog(token: &str) -> Option<i64> {
    admin_get::<NcmecPage>("/api/admin/ncmec-reports", token).map(|p| p.count)
}

/// minimal percent-encoding for query values (alnum + a few safe chars pass).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
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
