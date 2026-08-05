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
    /// open channel tabs, in order — restored on next launch (unless CLI args
    /// override). empty = fall back to the built-in demo set.
    pub channels: Vec<(Platform, String)>,
}

/// serialize a channel as `twitch:name` / `kick:name` for the config line.
fn chan_str(p: Platform, name: &str) -> String {
    let pfx = match p {
        Platform::Twitch => "twitch",
        Platform::Kick => "kick",
    };
    format!("{pfx}:{name}")
}

/// parse one `twitch:name` / `kick:name` / `name` token (bare = twitch).
fn parse_chan(s: &str) -> Option<(Platform, String)> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(r) = s.strip_prefix("kick:") {
        Some((Platform::Kick, r.trim().to_string()))
    } else if let Some(r) = s.strip_prefix("twitch:") {
        Some((Platform::Twitch, r.trim().to_string()))
    } else {
        Some((Platform::Twitch, s.to_string()))
    }
}

/// the user's own twitch credentials for direct-to-platform sending (chatterino
/// model). read from ~/.config/heatsync/token or the TWITCH_USER / TWITCH_OAUTH
/// env. absent → twitch send is disabled and the client says so.
pub struct Auth {
    pub twitch_user: Option<String>,
    pub twitch_oauth: Option<String>,
    pub kick_token: Option<String>,
}

fn dir() -> Option<PathBuf> {
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
    if let Some(p) = token_path() {
        if let Ok(text) = fs::read_to_string(&p) {
            for line in text.lines() {
                if let Some((k, v)) = line.split_once('=') {
                    let v = v.trim().to_string();
                    match k.trim() {
                        "twitch_user" if user.is_none() && !v.is_empty() => user = Some(v),
                        "twitch_oauth" if oauth.is_none() && !v.is_empty() => oauth = Some(v),
                        "kick_token" if kick.is_none() && !v.is_empty() => kick = Some(v),
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
    }
}

/// persist a `kick_token=` line into the token file, preserving other keys.
pub fn save_kick_token(token: &str) {
    let Some(p) = token_path() else { return };
    if let Some(d) = p.parent() {
        let _ = fs::create_dir_all(d);
    }
    let mut lines: Vec<String> = fs::read_to_string(&p)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim_start().starts_with("kick_token="))
        .map(str::to_string)
        .collect();
    lines.push(format!("kick_token={token}"));
    if fs::write(&p, lines.join("\n") + "\n").is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&p, fs::Permissions::from_mode(0o600));
        }
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
                        "channels" => {
                            cfg.channels = v.split(',').filter_map(parse_chan).collect();
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    cfg
}

/// ensure the token file exists (with a template + tight perms) and return its
/// path. the file holds a secret, so it's created mode 0600.
pub fn ensure_token_file() -> Option<PathBuf> {
    let p = token_path()?;
    if !p.exists() {
        if let Some(d) = p.parent() {
            let _ = fs::create_dir_all(d);
        }
        let tmpl = "# heatsync twitch sending (chatterino-style, sends go direct to twitch)\n\
                    # get a token with the 'chat:edit' scope, e.g. https://twitchtokengenerator.com\n\
                    twitch_user=\n\
                    twitch_oauth=\n";
        if fs::write(&p, tmpl).is_ok() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&p, fs::Permissions::from_mode(0o600));
            }
        }
    }
    Some(p)
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
                .map(|(p, n)| chan_str(*p, n))
                .collect::<Vec<_>>()
                .join(",");
            out.push_str(&format!("channels={joined}\n"));
        }
        let _ = fs::write(&p, out);
    }
}
