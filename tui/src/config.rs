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

fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("heatsync").join("config"))
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

/// best-effort persist (creates the dir). failures are non-fatal.
pub fn save(cfg: &Config) {
    if let Some(p) = path() {
        if let Some(dir) = p.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let _ = fs::write(&p, format!("tab_pos={}\n", cfg.tab_pos.as_str()));
    }
}
