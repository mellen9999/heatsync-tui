//! tiny persisted config at ~/.config/heatsync/config (KEY=value lines). just
//! the tab-bar position for now. no toml dep — one key, hand-parsed.

use std::fs;
use std::path::PathBuf;

use heatsync_core::Platform;

/// where the channel tab bar lives. left/right are vertical (tabs stacked).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TabPos {
    Top,
    Bottom,
    Left,
    Right,
}

impl TabPos {
    pub fn is_vertical(self) -> bool {
        matches!(self, TabPos::Left | TabPos::Right)
    }

    /// cycle order for the toggle key: top → right → bottom → left → top.
    pub fn next(self) -> TabPos {
        match self {
            TabPos::Top => TabPos::Right,
            TabPos::Right => TabPos::Bottom,
            TabPos::Bottom => TabPos::Left,
            TabPos::Left => TabPos::Top,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            TabPos::Top => "top",
            TabPos::Bottom => "bottom",
            TabPos::Left => "left",
            TabPos::Right => "right",
        }
    }

    fn parse(s: &str) -> Option<TabPos> {
        match s.trim() {
            "top" => Some(TabPos::Top),
            "bottom" => Some(TabPos::Bottom),
            "left" => Some(TabPos::Left),
            "right" => Some(TabPos::Right),
            _ => None,
        }
    }
}

pub struct Config {
    pub tab_pos: TabPos,
    /// open tabs, in order — restored on next launch (unless CLI args
    /// override). each tab is one or more `+`-merged (platform, channel) subs.
    pub channels: Vec<Vec<(Platform, String)>>,
}

/// serialize a channel as `twitch:name` / `kick:name` / `yt:videoid`.
fn chan_str(p: Platform, name: &str) -> String {
    let pfx = match p {
        Platform::Twitch => "twitch",
        Platform::Kick => "kick",
        Platform::Youtube => "yt",
    };
    format!("{pfx}:{name}")
}

/// comma-separated tabs; within a tab, '+' or whitespace both separate subs —
/// heals configs saved from a space-typed join prompt.
fn parse_channels(v: &str) -> Vec<Vec<(Platform, String)>> {
    v.split(',')
        .map(|tab| {
            tab.split(|c: char| c == '+' || c.is_whitespace())
                .filter_map(parse_chan)
                .collect()
        })
        .filter(|t: &Vec<_>| !t.is_empty())
        .collect()
}

/// parse one `twitch:name` / `kick:name` / `yt:id` / `name` token (bare = twitch).
fn parse_chan(s: &str) -> Option<(Platform, String)> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(r) = s.strip_prefix("kick:") {
        Some((Platform::Kick, r.trim().to_string()))
    } else if let Some(r) = s.strip_prefix("yt:").or_else(|| s.strip_prefix("youtube:")) {
        Some((Platform::Youtube, r.trim().to_string()))
    } else if let Some(r) = s.strip_prefix("twitch:") {
        Some((Platform::Twitch, r.trim().to_string()))
    } else {
        Some((Platform::Twitch, s.to_string()))
    }
}

/// optional own-token fallbacks for sending without a heatsync login: twitch
/// direct IRC (TWITCH_USER / TWITCH_OAUTH, or ~/.config/heatsync/token) and a
/// kick token (KICK_TOKEN). `heatsync-tui login` is the way in; these stay for
/// people who already have them.
pub struct Auth {
    pub twitch_user: Option<String>,
    pub twitch_oauth: Option<String>,
    pub kick_token: Option<String>,
    /// admin session JWT for `heatsync status` (mellen's own account only).
    pub admin_token: Option<String>,
}

pub fn dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("heatsync"))
}

fn path() -> Option<PathBuf> {
    Some(dir()?.join("config"))
}

fn token_path() -> Option<PathBuf> {
    Some(dir()?.join("token"))
}

/// load twitch creds: env wins, then the token file. oauth `oauth:`/`#` prefixes
/// are stripped so the user can paste whatever a generator gives them.
pub fn load_auth() -> Auth {
    let mut user = std::env::var("TWITCH_USER").ok().filter(|s| !s.is_empty());
    let mut oauth = std::env::var("TWITCH_OAUTH").ok().filter(|s| !s.is_empty());
    let mut kick = std::env::var("KICK_TOKEN").ok().filter(|s| !s.is_empty());
    let mut admin = std::env::var("HEATSYNC_ADMIN_TOKEN")
        .ok()
        .filter(|s| !s.is_empty());
    if let Some(p) = token_path() {
        if let Ok(text) = fs::read_to_string(&p) {
            for line in text.lines() {
                if let Some((k, v)) = line.split_once('=') {
                    let v = v.trim().to_string();
                    match k.trim() {
                        "twitch_user" if user.is_none() && !v.is_empty() => user = Some(v),
                        "twitch_oauth" if oauth.is_none() && !v.is_empty() => oauth = Some(v),
                        "kick_token" if kick.is_none() && !v.is_empty() => kick = Some(v),
                        "admin_token" if admin.is_none() && !v.is_empty() => admin = Some(v),
                        _ => {}
                    }
                }
            }
        }
    }
    let oauth = oauth.map(|o| {
        o.trim_start_matches("oauth:")
            .trim_start_matches('#')
            .to_string()
    });
    Auth {
        twitch_user: user,
        twitch_oauth: oauth,
        kick_token: kick,
        admin_token: admin,
    }
}

pub fn load() -> Config {
    let mut cfg = Config {
        tab_pos: TabPos::Top,
        channels: Vec::new(),
    };
    if let Some(p) = path() {
        if let Ok(text) = fs::read_to_string(&p) {
            for line in text.lines() {
                if let Some((k, v)) = line.split_once('=') {
                    match k.trim() {
                        "tab_pos" => {
                            if let Some(tp) = TabPos::parse(v) {
                                cfg.tab_pos = tp;
                            }
                        }
                        "channels" => cfg.channels = parse_channels(v),
                        _ => {}
                    }
                }
            }
        }
    }
    cfg
}

/// best-effort persist (creates the dir). failures are non-fatal.
pub fn save(cfg: &Config) {
    if let Some(p) = path() {
        if let Some(dir) = p.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let mut out = format!("tab_pos={}\n", cfg.tab_pos.as_str());
        if !cfg.channels.is_empty() {
            let joined = cfg
                .channels
                .iter()
                .map(|tab| {
                    tab.iter()
                        .map(|(p, n)| chan_str(*p, n))
                        .collect::<Vec<_>>()
                        .join("+")
                })
                .collect::<Vec<_>>()
                .join(",");
            out.push_str(&format!("channels={joined}\n"));
        }
        let _ = fs::write(&p, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn space_typed_spec_heals_into_merge() {
        let got =
            parse_channels("kick:xqc+twitch:xqc,twitch:nl_kripp kick:nl_kripp yt:4tDC0sKhTnA");
        assert_eq!(
            got,
            vec![
                vec![
                    (Platform::Kick, "xqc".to_string()),
                    (Platform::Twitch, "xqc".to_string()),
                ],
                vec![
                    (Platform::Twitch, "nl_kripp".to_string()),
                    (Platform::Kick, "nl_kripp".to_string()),
                    (Platform::Youtube, "4tDC0sKhTnA".to_string()),
                ],
            ]
        );
    }
}
