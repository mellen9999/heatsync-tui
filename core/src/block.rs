//! per-emote blocks: hide one exact emote, never a name.
//!
//! a block is keyed by the emote's identity (see `blockid`): the provider's own
//! id for 7tv/bttv/kick/ffz/twitch urls, else sha256 of the normalised url — the
//! same key the extension, desktop and chatterino file, so a block made in any
//! client hides the emote in every other. a block an older build filed as
//! btoa(url) is still honoured. two emotes sharing a name (7tv `W`, bttv `W`)
//! are two keys; blocking never touches a name.
//!
//! the set is process-global: layout and completion consult it without every
//! caller threading it through. the face loads it from config at start and
//! persists after every change.

use crate::blockid::{all_forms, emote_hash, Scope};
use crate::emote::Emote;
use std::collections::HashMap;
use std::sync::RwLock;

/// the server's block-hash column is varchar(255): longer keys can't be stored
/// or synced, so they are refused rather than truncated into a key that matches
/// nothing and can never be unblocked.
pub const KEY_MAX: usize = 255;
/// blocks kept — a runaway sync can't grow the set without bound.
pub const CAP: usize = 5000;

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                out.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn unb64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for b in s.trim_end_matches('=').bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// the old key shape (btoa of the url), kept so existing blocks still match.
/// None when it won't fit the server.
fn legacy_key_of_url(url: &str) -> Option<String> {
    let k = b64(url.as_bytes());
    (!url.is_empty() && k.len() <= KEY_MAX).then_some(k)
}

/// the key a new block of this emote is filed under. None when it has no url.
pub fn key(e: &Emote) -> Option<String> {
    if e.url.is_empty() {
        return None;
    }
    let scope = if e.global {
        Scope::Global
    } else {
        Scope::Channel
    };
    Some(emote_hash(&e.url, scope))
}

/// every key this emote could be blocked under, any client or build.
fn forms(e: &Emote) -> Vec<String> {
    let mut v = all_forms(&e.url);
    v.extend(legacy_key_of_url(&e.url));
    if e.id.len() >= 8 {
        v.push(e.id.clone());
    }
    v
}

/// what a key points at, for display: the url's last two path parts.
pub fn describe(key: &str) -> String {
    let url = unb64(key)
        .and_then(|b| String::from_utf8(b).ok())
        .filter(|u| u.starts_with("http"));
    match url {
        Some(u) => {
            let tail: Vec<&str> = u.rsplit('/').take(2).collect();
            format!(
                "{}/{}",
                tail.get(1).unwrap_or(&""),
                tail.first().unwrap_or(&"")
            )
        }
        None => key.chars().take(12).collect(),
    }
}

/// the blocked emotes: key → the name it was blocked under ("" when learned
/// from the account and never seen here yet), in the order they were blocked.
#[derive(Default, Debug, Clone, PartialEq)]
pub struct Blocks {
    names: HashMap<String, String>,
    order: Vec<String>,
}

impl Blocks {
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn has(&self, key: &str) -> bool {
        self.names.contains_key(key)
    }

    /// true when newly added. a known key learns its name if it had none.
    pub fn add(&mut self, key: String, name: &str) -> bool {
        if key.len() > KEY_MAX || key.is_empty() {
            return false;
        }
        if let Some(n) = self.names.get_mut(&key) {
            if n.is_empty() {
                *n = name.to_string();
            }
            return false;
        }
        if self.order.len() >= CAP {
            return false;
        }
        self.order.push(key.clone());
        self.names.insert(key, name.to_string());
        true
    }

    /// the name it was filed under, when it was blocked.
    pub fn remove(&mut self, key: &str) -> Option<String> {
        let n = self.names.remove(key)?;
        self.order.retain(|k| k != key);
        Some(n)
    }

    /// (key, name) in block order.
    pub fn list(&self) -> Vec<(String, String)> {
        self.order
            .iter()
            .map(|k| (k.clone(), self.names[k].clone()))
            .collect()
    }

    /// keys blocked under this name (exact, case-sensitive).
    pub fn keys_named(&self, name: &str) -> Vec<String> {
        self.order
            .iter()
            .filter(|k| self.names[*k] == name)
            .cloned()
            .collect()
    }

    /// is this exact emote blocked — under any key any client files it by.
    pub fn hides(&self, e: &Emote) -> bool {
        !self.order.is_empty() && forms(e).iter().any(|k| self.has(k))
    }

    /// the keys held for this exact emote (to unblock every form).
    pub fn held(&self, e: &Emote) -> Vec<String> {
        forms(e).into_iter().filter(|k| self.has(k)).collect()
    }
}

static STORE: RwLock<Option<Blocks>> = RwLock::new(None);

fn read<R>(f: impl FnOnce(&Blocks) -> R) -> R {
    match STORE.read() {
        Ok(g) => match g.as_ref() {
            Some(b) => f(b),
            None => f(&Blocks::default()),
        },
        Err(p) => match p.into_inner().as_ref() {
            Some(b) => f(b),
            None => f(&Blocks::default()),
        },
    }
}

fn write<R>(f: impl FnOnce(&mut Blocks) -> R) -> R {
    let mut g = STORE.write().unwrap_or_else(|p| p.into_inner());
    f(g.get_or_insert_with(Blocks::default))
}

/// is this exact emote hidden right now.
pub fn is_blocked(e: &Emote) -> bool {
    read(|b| b.hides(e))
}

/// block this exact emote. false when already blocked or unkeyable.
pub fn block(e: &Emote) -> bool {
    match key(e) {
        Some(k) => write(|b| b.add(k, &e.name)),
        None => false,
    }
}

/// the keys held for this exact emote.
pub fn held(e: &Emote) -> Vec<String> {
    read(|b| b.held(e))
}

/// unblock by key; the name it was filed under.
pub fn unblock_key(key: &str) -> Option<String> {
    write(|b| b.remove(key))
}

pub fn list() -> Vec<(String, String)> {
    read(Blocks::list)
}

pub fn keys_named(name: &str) -> Vec<String> {
    read(|b| b.keys_named(name))
}

/// replace the whole set (startup, from config).
pub fn load(entries: impl IntoIterator<Item = (String, String)>) {
    write(|b| {
        *b = Blocks::default();
        for (k, n) in entries {
            b.add(k, &n);
        }
    })
}

/// fold in the account's blocks (keys only): anything new is added, nothing
/// local is dropped. returns how many were new.
pub fn merge_keys(keys: impl IntoIterator<Item = String>) -> usize {
    write(|b| keys.into_iter().filter(|k| b.add(k.clone(), "")).count())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn em(name: &str, url: &str, id: &str) -> Emote {
        Emote {
            name: name.into(),
            url: url.into(),
            provider: "7tv".into(),
            id: id.into(),
            animated: false,
            zero_width: false,
            global: false,
        }
    }

    #[test]
    fn legacy_key_is_btoa_of_the_url() {
        // known btoa vectors
        assert_eq!(b64(b""), "");
        assert_eq!(b64(b"f"), "Zg==");
        assert_eq!(b64(b"fo"), "Zm8=");
        assert_eq!(b64(b"foo"), "Zm9v");
        assert_eq!(b64(b"foobar"), "Zm9vYmFy");
        let u = "https://cdn.7tv.app/emote/01AAA/2x.webp";
        assert_eq!(
            legacy_key_of_url(u).unwrap(),
            "aHR0cHM6Ly9jZG4uN3R2LmFwcC9lbW90ZS8wMUFBQS8yeC53ZWJw"
        );
        assert_eq!(unb64(&legacy_key_of_url(u).unwrap()).unwrap(), u.as_bytes());
        assert!(legacy_key_of_url("").is_none());
        assert!(
            legacy_key_of_url(&"x".repeat(200)).is_none(),
            "won't fit 255 once encoded"
        );
    }

    #[test]
    fn blocks_the_exact_emote_never_the_name() {
        let a = em("W", "https://cdn.7tv.app/emote/A/2x.webp", "");
        let b = em("W", "https://cdn.betterttv.net/emote/B/2x.webp", "");
        let mut s = Blocks::default();
        assert!(s.add(key(&a).unwrap(), "W"));
        assert!(s.hides(&a));
        assert!(!s.hides(&b), "same name, other emote: still shown");
        assert!(!s.add(key(&a).unwrap(), "W"), "second block is a no-op");
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn a_block_from_any_client_hides_it_here() {
        let e = em("W", "https://cdn.7tv.app/emote/01ABC/1x.webp", "01ABC");
        assert_eq!(key(&e).unwrap(), "01ABC", "new blocks file by provider id");
        let sha = "b77bff3d9068dbd6ac26734d183ecb9480e63250de106a79ab1c317d0360dc65";
        let old = legacy_key_of_url(&e.url).unwrap();
        for k in ["01ABC", sha, old.as_str()] {
            let mut s = Blocks::default();
            s.add(k.into(), "W");
            assert!(s.hides(&e), "{k}");
            assert_eq!(s.held(&e), vec![k.to_owned()]);
        }
        let mut s = Blocks::default();
        s.add("01ZZZ".into(), "W");
        assert!(!s.hides(&e));
    }

    #[test]
    fn ids_match_only_when_long_enough_to_be_unique() {
        let long = em("X", "https://c/x", "01HABCDEFGHJKMNPQRSTVWXYZ0");
        let short = em("K", "https://c/k", "25");
        let mut s = Blocks::default();
        s.add("01HABCDEFGHJKMNPQRSTVWXYZ0".into(), "X");
        s.add("25".into(), "K");
        assert!(s.hides(&long), "a long id the extension filed");
        assert!(
            !s.hides(&short),
            "short numeric ids collide across providers"
        );
    }

    #[test]
    fn unblock_list_names_and_sync_merge() {
        let a = em("W", "https://c/a", "");
        let mut s = Blocks::default();
        s.add(key(&a).unwrap(), "W");
        s.add("c3luY2Vk".into(), ""); // learned from the account: no name yet
        assert_eq!(s.keys_named("W"), vec![key(&a).unwrap()]);
        assert_eq!(s.list().len(), 2);
        // seeing the emote teaches the nameless key its name
        s.add("c3luY2Vk".into(), "Seen");
        assert_eq!(s.list()[1].1, "Seen");
        assert_eq!(s.remove(&key(&a).unwrap()).as_deref(), Some("W"));
        assert!(s.remove(&key(&a).unwrap()).is_none());
        assert!(!s.hides(&a));
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn bounded_and_junk_refused() {
        let mut s = Blocks::default();
        assert!(!s.add(String::new(), "x"));
        assert!(!s.add("k".repeat(KEY_MAX + 1), "x"));
        for i in 0..CAP + 10 {
            s.add(format!("k{i}"), "n");
        }
        assert_eq!(s.len(), CAP);
    }

    #[test]
    fn describe_shows_where_a_key_points() {
        let k = legacy_key_of_url("https://cdn.7tv.app/emote/01AAA/2x.webp").unwrap();
        assert_eq!(describe(&k), "01AAA/2x.webp");
        assert_eq!(
            describe("!!notbase64!!"),
            "!!notbase64!",
            "not base64: the first 12 chars"
        );
    }

    #[test]
    fn the_global_set_hides_and_unhides() {
        // unique url: other tests in this binary never touch it
        let e = em("GlobalBlockTest", "https://block.test/global/1x.webp", "");
        assert!(!is_blocked(&e));
        assert!(block(&e));
        assert!(is_blocked(&e));
        assert!(!block(&e));
        assert_eq!(
            unblock_key(&key(&e).unwrap()).as_deref(),
            Some("GlobalBlockTest")
        );
        assert!(!is_blocked(&e));
    }
}
