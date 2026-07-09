//! tiny persisted config at ~/.config/heatsync/config (KEY=value lines). just
//! the tab-bar position for now. no toml dep — one key, hand-parsed.

use std::fs;
use std::path::PathBuf;

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
}

/// the user's own twitch credentials for direct-to-platform sending (chatterino
/// model). read from ~/.config/heatsync/token or the TWITCH_USER / TWITCH_OAUTH
/// env. absent → twitch send is disabled and the client says so.
pub struct Auth {
    pub twitch_user: Option<String>,
    pub twitch_oauth: Option<String>,
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
    if let Some(p) = token_path() {
        if let Ok(text) = fs::read_to_string(&p) {
            for line in text.lines() {
                if let Some((k, v)) = line.split_once('=') {
                    let v = v.trim().to_string();
                    match k.trim() {
                        "twitch_user" if user.is_none() && !v.is_empty() => user = Some(v),
                        "twitch_oauth" if oauth.is_none() && !v.is_empty() => oauth = Some(v),
                        _ => {}
                    }
                }
            }
        }
    }
    let oauth = oauth.map(|o| o.trim_start_matches("oauth:").trim_start_matches('#').to_string());
    Auth { twitch_user: user, twitch_oauth: oauth }
}

pub fn load() -> Config {
    let mut cfg = Config { tab_pos: TabPos::Top };
    if let Some(p) = path() {
        if let Ok(text) = fs::read_to_string(&p) {
            for line in text.lines() {
                if let Some((k, v)) = line.split_once('=') {
                    if k.trim() == "tab_pos" {
                        if let Some(tp) = TabPos::parse(v) {
                            cfg.tab_pos = tp;
                        }
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
        let _ = fs::write(&p, format!("tab_pos={}\n", cfg.tab_pos.as_str()));
    }
}
