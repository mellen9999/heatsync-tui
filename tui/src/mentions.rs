//! the mention log: every line that pinged you, kept across restarts at
//! ~/.config/heatsync/mentions (jsonl, mode 0600 — it holds other people's
//! chat). capped at the newest [`CAP`]; the file is appended per mention and
//! rewritten whole every [`CAP`] appends so it can't grow without bound.

use std::collections::VecDeque;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

use heatsync_core::{sanitize, Platform};
use serde_json::{json, Value};

use crate::config;

pub const CAP: usize = 500;

#[derive(Clone, Debug, PartialEq)]
pub struct Mention {
    pub platform: Platform,
    pub channel: String,
    pub user: String,
    pub color: Option<String>,
    pub text: String,
}

pub struct Log {
    rows: VecDeque<Mention>,
    path: Option<PathBuf>,
    /// appends since the file was last rewritten whole.
    dirty: usize,
}

fn plat_str(p: Platform) -> &'static str {
    match p {
        Platform::Twitch => "twitch",
        Platform::Kick => "kick",
        Platform::Youtube => "yt",
    }
}

fn plat_of(s: &str) -> Platform {
    match s {
        "kick" => Platform::Kick,
        "yt" | "youtube" => Platform::Youtube,
        _ => Platform::Twitch,
    }
}

fn to_line(m: &Mention) -> String {
    json!({
        "p": plat_str(m.platform),
        "c": m.channel,
        "u": m.user,
        "k": m.color,
        "t": m.text,
    })
    .to_string()
}

/// one jsonl row back into a mention. the file is ours but still untrusted
/// input (hand-edited, truncated by a crash): bad rows are skipped and text is
/// re-sanitized.
fn from_line(line: &str) -> Option<Mention> {
    let v: Value = serde_json::from_str(line).ok()?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(sanitize::clean);
    let text = s("t").filter(|t| !t.is_empty())?;
    Some(Mention {
        platform: plat_of(v.get("p").and_then(Value::as_str).unwrap_or("")),
        channel: s("c").unwrap_or_default(),
        user: s("u").unwrap_or_default(),
        color: s("k").filter(|c| c.starts_with('#')),
        text,
    })
}

#[cfg(unix)]
fn private(opts: &mut fs::OpenOptions) -> &mut fs::OpenOptions {
    use std::os::unix::fs::OpenOptionsExt;
    opts.mode(0o600)
}

#[cfg(not(unix))]
fn private(opts: &mut fs::OpenOptions) -> &mut fs::OpenOptions {
    opts
}

impl Log {
    /// read the newest [`CAP`] rows from `path` (None = in-memory only).
    pub fn open(path: Option<PathBuf>) -> Log {
        let mut rows: VecDeque<Mention> = path
            .as_ref()
            .and_then(|p| fs::read_to_string(p).ok())
            .map(|t| t.lines().filter_map(from_line).collect())
            .unwrap_or_default();
        while rows.len() > CAP {
            rows.pop_front();
        }
        Log {
            rows,
            path,
            dirty: 0,
        }
    }

    pub fn load() -> Log {
        Log::open(config::dir().map(|d| d.join("mentions")))
    }

    pub fn rows(&self) -> impl Iterator<Item = &Mention> {
        self.rows.iter()
    }

    /// record one mention: memory, then disk (best effort — a full disk must
    /// not cost the live line).
    pub fn push(&mut self, m: Mention) {
        let line = to_line(&m);
        self.rows.push_back(m);
        let over = self.rows.len() > CAP;
        if over {
            self.rows.pop_front();
        }
        self.dirty += 1;
        let Some(p) = self.path.clone() else { return };
        if let Some(dir) = p.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if self.dirty >= CAP {
            self.rewrite(&p);
        } else if let Ok(mut f) = private(fs::OpenOptions::new().create(true).append(true)).open(&p)
        {
            let _ = writeln!(f, "{line}");
        }
    }

    /// atomically replace the file with just the retained rows.
    fn rewrite(&mut self, p: &PathBuf) {
        let tmp = p.with_extension("tmp");
        let body: String = self.rows.iter().map(|m| to_line(m) + "\n").collect();
        let ok = private(
            fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true),
        )
        .open(&tmp)
        .and_then(|mut f| f.write_all(body.as_bytes()))
        .is_ok();
        if ok && fs::rename(&tmp, p).is_ok() {
            self.dirty = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(i: usize) -> Mention {
        Mention {
            platform: Platform::Kick,
            channel: "xqc".into(),
            user: format!("u{i}"),
            color: Some("#ff0000".into()),
            text: format!("hey {i}"),
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hs-mentions-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d.join("mentions")
    }

    #[test]
    fn survives_a_restart() {
        let p = scratch("restart");
        let mut log = Log::open(Some(p.clone()));
        log.push(m(1));
        log.push(m(2));
        let back = Log::open(Some(p.clone()));
        assert_eq!(back.rows().cloned().collect::<Vec<_>>(), vec![m(1), m(2)]);
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn keeps_only_the_newest_500_on_disk_and_in_memory() {
        let p = scratch("cap");
        let mut log = Log::open(Some(p.clone()));
        for i in 0..CAP + 120 {
            log.push(m(i));
        }
        assert_eq!(log.rows().count(), CAP);
        assert_eq!(log.rows().next().unwrap().user, "u120");
        let back = Log::open(Some(p.clone()));
        assert_eq!(back.rows().count(), CAP);
        assert_eq!(back.rows().last().unwrap().user, format!("u{}", CAP + 119));
        // the rewrite bounded the file (≤ CAP + the appends since)
        let lines = fs::read_to_string(&p).unwrap().lines().count();
        assert!(lines <= CAP + CAP, "{lines}");
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let p = scratch("mode");
        let mut log = Log::open(Some(p.clone()));
        log.push(m(1));
        let mode = fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn corrupt_and_hostile_rows_are_skipped_or_cleaned() {
        let p = scratch("bad");
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(
            &p,
            format!(
                "not json\n{}\n{{\"t\":\"\"}}\n{{\"p\":\"kick\",\"c\":\"c\",\"u\":\"u\",\"t\":\"hi\\u001b[2J\"}}\n",
                to_line(&m(1))
            ),
        )
        .unwrap();
        let log = Log::open(Some(p.clone()));
        let rows: Vec<_> = log.rows().collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].text, "hi[2J"); // ESC stripped
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn in_memory_log_never_touches_disk() {
        let mut log = Log::open(None);
        log.push(m(1));
        assert_eq!(log.rows().count(), 1);
    }
}
