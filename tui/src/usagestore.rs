//! the emote usage file (`usage` next to the config): what this person sends,
//! so Tab can lead with it. local only — it is what they typed, so it is never
//! synced or sent anywhere. loading never fails (a missing or corrupt file is
//! empty). saving happens on its own thread so the UI never waits on the disk:
//! the thread is sent each send's pictures, re-reads the file, folds them in
//! (so two running terminals add to each other instead of erasing each other)
//! and writes through a uniquely named temp file then a rename, so a crash
//! never leaves half a file. a send queued in the last instant before exit may
//! be lost; that costs one use, never the file.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};

use heatsync_core::usage::Usage;

/// the biggest legal file (400 rows of the longest keys) is ~62 KB; anything
/// over twice that is not ours and reads as empty.
const MAX_FILE: u64 = 128 * 1024;

/// the pictures one send used, with when: what the saver is handed.
pub(crate) type Uses = Vec<(String, u64)>;

fn path() -> Option<PathBuf> {
    Some(crate::config::dir()?.join("usage"))
}

pub(crate) fn read(p: &Path, now: u64) -> Usage {
    let ok = fs::metadata(p).is_ok_and(|m| m.is_file() && m.len() <= MAX_FILE);
    if !ok {
        return Usage::default();
    }
    fs::read(p).ok().map_or_else(Usage::default, |b| {
        Usage::parse(&String::from_utf8_lossy(&b), now)
    })
}

pub(crate) fn load() -> Usage {
    path().map_or_else(Usage::default, |p| read(&p, crate::clock::now_secs()))
}

/// write `text` to `p` through a temp file in the same folder (a name no other
/// process or write shares, never followed through a planted link), then
/// rename it in. the temp file is removed if anything fails.
pub(crate) fn write_atomic(p: &Path, text: &str) -> std::io::Result<()> {
    static N: AtomicU64 = AtomicU64::new(0);
    if let Some(dir) = p.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = p.with_extension(format!(
        "{}.{}.tmp",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let mut o = fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let done = (|| {
        let mut f = o.open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, p)
    })();
    if done.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    done
}

/// fold `uses` into whatever is in the file now, and write it back. what
/// another terminal wrote a moment ago is kept; the merge is per picture
/// (latest time, decayed counts added).
pub(crate) fn merge_write(p: &Path, uses: &[(String, u64)], now: u64) -> std::io::Result<()> {
    let mut u = read(p, now);
    for (k, at) in uses {
        u.note(k, *at);
    }
    u.prune(now);
    write_atomic(p, &u.render())
}

/// the saver: send it the pictures of each accepted send; it folds them in and
/// writes. a failure is silent (usage is a nicety, never worth an error line).
pub(crate) fn spawn_writer() -> Sender<Uses> {
    let (tx, rx) = channel::<Uses>();
    if let Some(p) = path() {
        std::thread::spawn(move || {
            while let Ok(mut uses) = rx.recv() {
                // everything queued since goes in one write
                while let Ok(more) = rx.try_recv() {
                    uses.extend(more);
                }
                let _ = merge_write(&p, &uses, crate::clock::now_secs());
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

    fn uses(keys: &[&str], at: u64) -> Uses {
        keys.iter().map(|k| (k.to_string(), at)).collect()
    }

    #[test]
    fn a_saved_file_comes_back_and_leaves_no_temp_behind() {
        let d = dir("roundtrip");
        let p = d.join("usage");
        merge_write(&p, &uses(&["7tv:a"], 5), 5).unwrap();
        assert!(read(&p, 5).score("7tv:a", 5) > 0.9);
        let names: Vec<_> = fs::read_dir(&d).unwrap().flatten().collect();
        assert_eq!(names.len(), 1, "only the file: {names:?}");
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
        assert!(read(&d.join("nope"), 9).is_empty());
        fs::write(d.join("junk"), [0xff, 0xfe, 0x00, b'\n', 0x80]).unwrap();
        assert!(read(&d.join("junk"), 9).is_empty());
        fs::write(d.join("big"), vec![b'x'; (MAX_FILE + 1) as usize]).unwrap();
        assert!(read(&d.join("big"), 9).is_empty());
        // a directory where the file should be is not a file either
        fs::create_dir_all(d.join("dir")).unwrap();
        assert!(read(&d.join("dir"), 9).is_empty());
        // and a corrupt file is replaced, not an error, by the next save
        merge_write(&d.join("junk"), &uses(&["7tv:a"], 9), 9).unwrap();
        assert_eq!(read(&d.join("junk"), 9).len(), 1);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_full_legal_file_never_reads_as_empty() {
        let d = dir("full");
        let p = d.join("usage");
        let long = "x".repeat(110);
        let mut u = Usage::default();
        for i in 0..heatsync_core::usage::CAP {
            u.note(&format!("{long}{i:04}"), 5);
        }
        let text = u.render();
        assert!((text.len() as u64) < MAX_FILE, "{}", text.len());
        write_atomic(&p, &text).unwrap();
        assert_eq!(read(&p, 5).len(), heatsync_core::usage::CAP);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn two_terminals_add_to_each_other_instead_of_erasing_each_other() {
        let d = dir("two");
        let p = d.join("usage");
        // terminal A and terminal B each started with the same (empty) file and
        // each sent something different; both writes happen, in either order
        merge_write(&p, &uses(&["7tv:a"], 10), 10).unwrap();
        merge_write(&p, &uses(&["7tv:b", "7tv:a"], 11), 11).unwrap();
        let got = read(&p, 11);
        assert!(got.score("7tv:a", 11) > 1.9, "A's use and B's add up");
        assert!(got.score("7tv:b", 11) > 0.9);
        // and two threads writing at once never leave a mixed or missing file
        let hs: Vec<_> = (0..4)
            .map(|t| {
                let p = p.clone();
                std::thread::spawn(move || {
                    for i in 0..20 {
                        let _ = merge_write(&p, &uses(&[&format!("7tv:t{t}")], 12 + i), 12 + i);
                    }
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        assert!(read(&p, 40).len() >= 2, "a whole, readable file");
        let left: Vec<_> = fs::read_dir(&d)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(left.is_empty(), "no temp files left: {left:?}");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_planted_link_at_a_temp_name_is_never_written_through() {
        #[cfg(unix)]
        {
            let d = dir("link");
            let p = d.join("usage");
            let victim = d.join("victim");
            fs::write(&victim, "keep").unwrap();
            // plant links at the names a write could pick (this pid, small counters)
            for n in 0..64 {
                let t = d.join(format!("usage.{}.{n}.tmp", std::process::id()));
                let _ = std::os::unix::fs::symlink(&victim, t);
            }
            for _ in 0..8 {
                let _ = merge_write(&p, &uses(&["7tv:a"], 1), 1);
            }
            assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
            let _ = fs::remove_dir_all(&d);
        }
    }
}
