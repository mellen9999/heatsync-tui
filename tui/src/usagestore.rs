//! the emote usage file (`usage` next to the config): what this person sends,
//! so Tab can lead with it. local only — it is what they typed, so it is never
//! synced or sent anywhere. loading never fails (a missing or corrupt file is
//! empty); saving happens on its own thread, write-then-rename, so the UI never
//! waits on the disk and a crash never leaves half a file.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Sender};

use heatsync_core::usage::Usage;

/// a file bigger than this is not ours: read as empty.
const MAX_FILE: u64 = 64 * 1024;

fn path() -> Option<PathBuf> {
    Some(crate::config::dir()?.join("usage"))
}

pub(crate) fn read(p: &Path) -> Usage {
    let ok = fs::metadata(p).is_ok_and(|m| m.is_file() && m.len() <= MAX_FILE);
    if !ok {
        return Usage::default();
    }
    fs::read(p).ok().map_or_else(Usage::default, |b| {
        Usage::parse(&String::from_utf8_lossy(&b))
    })
}

pub(crate) fn load() -> Usage {
    path().map_or_else(Usage::default, |p| read(&p))
}

/// write `text` to `p` through a temp file in the same folder, then rename it in.
pub(crate) fn write_atomic(p: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = p.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = p.with_extension("tmp");
    let mut o = fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let mut f = o.open(&tmp)?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    drop(f);
    fs::rename(&tmp, p)
}

/// the saver: send it a whole file's text, it writes the newest one. a failure
/// is silent (usage is a nicety, never worth an error line).
pub(crate) fn spawn_writer() -> Sender<String> {
    let (tx, rx) = channel::<String>();
    if let Some(p) = path() {
        std::thread::spawn(move || {
            while let Ok(mut text) = rx.recv() {
                // only the newest snapshot matters
                while let Ok(newer) = rx.try_recv() {
                    text = newer;
                }
                let _ = write_atomic(&p, &text);
            }
        });
    }
    tx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hs-usage-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_saved_file_comes_back_and_leaves_no_temp_behind() {
        let d = dir("roundtrip");
        let p = d.join("usage");
        let mut u = Usage::default();
        u.note("7tv:a", 5);
        write_atomic(&p, &u.render()).unwrap();
        assert_eq!(read(&p).render(), u.render());
        assert!(!p.with_extension("tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o077, 0);
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_missing_corrupt_or_oversized_file_reads_as_empty() {
        let d = dir("bad");
        assert!(read(&d.join("nope")).is_empty());
        fs::write(d.join("junk"), [0xff, 0xfe, 0x00, b'\n', 0x80]).unwrap();
        assert!(read(&d.join("junk")).is_empty());
        fs::write(d.join("big"), vec![b'x'; (MAX_FILE + 1) as usize]).unwrap();
        assert!(read(&d.join("big")).is_empty());
        // a directory where the file should be is not a file either
        fs::create_dir_all(d.join("dir")).unwrap();
        assert!(read(&d.join("dir")).is_empty());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_later_save_replaces_the_earlier_whole() {
        let d = dir("replace");
        let p = d.join("usage");
        let mut u = Usage::default();
        u.note("7tv:a", 1);
        write_atomic(&p, &u.render()).unwrap();
        let mut v = Usage::default();
        v.note("7tv:b", 2);
        write_atomic(&p, &v.render()).unwrap();
        let got = read(&p);
        assert!(got.score("7tv:a", 2) == 0.0 && got.score("7tv:b", 2) > 0.9);
        let _ = fs::remove_dir_all(&d);
    }
}
