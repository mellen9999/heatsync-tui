//! blocking HTTP against the public HeatSync API (no auth). emote sets + image
//! bytes for the TUI, and the archive/corpus reads for the CLI subcommands.

use std::io::Read;
use std::time::Duration;

use heatsync_core::emote::{EmoteSet, EmoteSetResponse};
use heatsync_core::Platform;
use serde::Deserialize;

const BASE: &str = "https://heatsync.org";
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

/// channel emote set (7tv/bttv/ffz precedence, deduped server-side). urls are
/// upgraded to 2x at INGEST — before EmoteSet::from_list — so every render
/// cache key downstream is already the 2x identity.
pub fn emote_set(channel: &str, platform: Platform) -> Option<EmoteSet> {
    let url = format!(
        "{BASE}/api/channel/{channel}/emotes?platform={}",
        platform_q(platform)
    );
    let mut resp: EmoteSetResponse = agent().get(&url).call().ok()?.into_json().ok()?;
    for e in &mut resp.emotes {
        e.url = upgrade_2x(&e.url);
    }
    Some(EmoteSet::from_list(resp.emotes))
}

/// host-gated, monotonic upgrade of an emote url to its 2x asset. terminal
/// emote cells run ~60x40px+, so 1x (28-32px) upscales blurry while 2x
/// downscales crisp. unknown hosts and already-≥2x urls pass through.
pub fn upgrade_2x(url: &str) -> String {
    let Some((host_part, _)) = url.strip_prefix("https://").and_then(|r| r.split_once('/')) else {
        return url.to_string();
    };
    let Some((path, last)) = url.rsplit_once('/') else {
        return url.to_string();
    };
    match host_part {
        // cdn.7tv.app/emote/<id>/1x.webp → 2x.webp (also .gif/.png/.avif)
        "cdn.7tv.app" => {
            if let Some((n, ext)) = last.split_once('x') {
                if n == "1" && !ext.is_empty() {
                    return format!("{path}/2x{ext}");
                }
            }
            url.to_string()
        }
        // cdn.betterttv.net/emote/<id>/1x[.webp] → 2x[.webp]
        "cdn.betterttv.net" => {
            if let Some(rest) = last.strip_prefix("1x") {
                return format!("{path}/2x{rest}");
            }
            url.to_string()
        }
        // cdn.frankerfacez.com/emote/<id>/<n> → max(n, 2); channel sets are
        // often already /2, and /4 404s for many emotes — 2 is the safe target.
        "cdn.frankerfacez.com" => {
            if last == "1" {
                return format!("{path}/2");
            }
            url.to_string()
        }
        _ => url.to_string(),
    }
}

/// raw image bytes for an emote, capped so a hostile CDN can't balloon memory.
/// disk-cached under XDG cache — cold starts stop refetching every emote.
pub fn image_bytes(url: &str) -> Option<Vec<u8>> {
    let cache_path = cache::path_for(url);
    if let Some(p) = &cache_path {
        if let Ok(bytes) = std::fs::read(p) {
            if !bytes.is_empty() {
                return Some(bytes);
            }
        }
    }
    let mut buf = Vec::new();
    agent()
        .get(url)
        .call()
        .ok()?
        .into_reader()
        .take(MAX_IMAGE_BYTES)
        .read_to_end(&mut buf)
        .ok()?;
    if buf.is_empty() {
        return None;
    }
    if let Some(p) = &cache_path {
        cache::store(p, &buf);
    }
    Some(buf)
}

/// tiny disk cache for fetched emote bytes. best-effort: any io error just
/// falls back to network. sweep_cache() bounds total size at startup.
mod cache {
    use std::path::{Path, PathBuf};

    const MAX_CACHE_BYTES: u64 = 64 * 1024 * 1024;

    fn dir() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
        Some(base.join("heatsync").join("img"))
    }

    /// cache file for a url — fnv-1a 64 over the url, hex. collision odds are
    /// negligible at emote-cache scale and a collision only mis-renders.
    pub fn path_for(url: &str) -> Option<PathBuf> {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in url.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        Some(dir()?.join(format!("{h:016x}")))
    }

    /// atomic-enough store: write sibling tmp, rename over. rename is atomic on
    /// the same fs, so readers never see a torn file.
    pub fn store(path: &Path, bytes: &[u8]) {
        let Some(parent) = path.parent() else { return };
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }

    /// bound the cache: if the total exceeds the cap, delete oldest-mtime files
    /// until under. called once at startup, off the hot path.
    pub fn sweep() {
        let Some(d) = dir() else { return };
        let Ok(rd) = std::fs::read_dir(&d) else {
            return;
        };
        let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = rd
            .flatten()
            .filter_map(|e| {
                let md = e.metadata().ok()?;
                if !md.is_file() {
                    return None;
                }
                Some((md.modified().ok()?, md.len(), e.path()))
            })
            .collect();
        let mut total: u64 = files.iter().map(|f| f.1).sum();
        if total <= MAX_CACHE_BYTES {
            return;
        }
        files.sort_by_key(|f| f.0); // oldest first
        for (_, len, path) in files {
            if total <= MAX_CACHE_BYTES {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(len);
            }
        }
    }
}

/// startup hook: bound the image disk cache.
pub fn sweep_cache() {
    cache::sweep();
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

#[cfg(test)]
mod tests {
    use super::upgrade_2x;

    #[test]
    fn upgrades_each_provider() {
        assert_eq!(
            upgrade_2x("https://cdn.7tv.app/emote/abc/1x.webp"),
            "https://cdn.7tv.app/emote/abc/2x.webp"
        );
        assert_eq!(
            upgrade_2x("https://cdn.7tv.app/emote/abc/1x.gif"),
            "https://cdn.7tv.app/emote/abc/2x.gif"
        );
        assert_eq!(
            upgrade_2x("https://cdn.betterttv.net/emote/abc/1x"),
            "https://cdn.betterttv.net/emote/abc/2x"
        );
        assert_eq!(
            upgrade_2x("https://cdn.betterttv.net/emote/abc/1x.webp"),
            "https://cdn.betterttv.net/emote/abc/2x.webp"
        );
        assert_eq!(
            upgrade_2x("https://cdn.frankerfacez.com/emote/123/1"),
            "https://cdn.frankerfacez.com/emote/123/2"
        );
    }

    #[test]
    fn monotonic_and_host_gated() {
        // already 2x+ → untouched
        assert_eq!(
            upgrade_2x("https://cdn.7tv.app/emote/abc/2x.webp"),
            "https://cdn.7tv.app/emote/abc/2x.webp"
        );
        assert_eq!(
            upgrade_2x("https://cdn.frankerfacez.com/emote/123/2"),
            "https://cdn.frankerfacez.com/emote/123/2"
        );
        assert_eq!(
            upgrade_2x("https://cdn.frankerfacez.com/emote/123/4"),
            "https://cdn.frankerfacez.com/emote/123/4"
        );
        // unknown hosts pass through untouched
        assert_eq!(
            upgrade_2x("https://files.kick.com/emotes/1/fullsize"),
            "https://files.kick.com/emotes/1/fullsize"
        );
        assert_eq!(upgrade_2x("not a url"), "not a url");
    }
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
