//! blocking HTTP against the public HeatSync API (no auth). emote sets + image
//! bytes for the TUI, and the archive/corpus reads for the CLI subcommands.

use std::io::Read;
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

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(12))
        // the archive search endpoint gates non-browser UAs (anti-scrape); we're
        // first-party, so identify as heatsync-tui behind a Mozilla token so the
        // guard passes. proper fix later: server-side allowlist this UA.
        .user_agent("Mozilla/5.0 (heatsync-tui/0.1; +https://heatsync.org)")
        .build()
}

/// channel emote set (7tv/bttv/ffz precedence, deduped server-side).
pub fn emote_set(channel: &str, platform: Platform) -> Option<EmoteSet> {
    let url = format!("{BASE}/api/channel/{channel}/emotes?platform={}", platform_q(platform));
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
    let url = format!(
        "{BASE}/api/archive/channel/{channel}/messages?from={from}&to={to}&limit={limit}"
    );
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
