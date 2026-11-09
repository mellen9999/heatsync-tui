//! what an emote is filed under when blocked — the same rule the extension,
//! the server, desktop and chatterino use, so a block made in any client hides
//! the emote in every other. byte-for-byte parity is pinned by the vectors in
//! the tests (shared with desktop).
//!
//! provider urls (7tv/bttv/kick/twitch/ffz) file under the provider's own emote
//! id (ffz as `ffz-<id>`); everything else under sha256 of the normalised url.

/// where an emote came from. pooled globals (`/api/emotes`) are filed by sha256
/// — except twitch's own, by id; a channel's set by provider id.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    Hs,
    Channel,
    Global,
    Native,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Plain,
    Ffz,
    Twitch,
}

fn valid_id(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 255
        && h.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+/_-=".contains(&b))
}

fn id_after<'a>(url: &'a str, host: &str, marker: &str) -> Option<&'a str> {
    let rest = url.strip_prefix("https://")?;
    let (h, path) = rest.split_once('/')?;
    if h != host && !h.ends_with(&format!(".{host}")) {
        return None;
    }
    let id = path.strip_prefix(marker)?.split(['/', '?', '#']).next()?;
    valid_id(id).then_some(id)
}

fn provider_id(url: &str) -> Option<(Kind, &str)> {
    let p = |host, marker| id_after(url, host, marker);
    p("7tv.app", "emote/")
        .or_else(|| p("betterttv.net", "emote/"))
        .or_else(|| p("kick.com", "emotes/"))
        .map(|id| (Kind::Plain, id))
        .or_else(|| p("frankerfacez.com", "emote/").map(|id| (Kind::Ffz, id)))
        .or_else(|| p("jtvnw.net", "emoticons/v2/").map(|id| (Kind::Twitch, id)))
}

/// the server's url normalisation before it hashes: a 7tv avif is its webp,
/// first-party origins drop to a path.
fn normalized(url: &str) -> String {
    let mut u = url.to_owned();
    for pre in [
        "https://cdn.7tv.app/emote/",
        "http://cdn.7tv.app/emote/",
        "//cdn.7tv.app/emote/",
    ] {
        if let Some(rest) = u.strip_prefix(pre) {
            if let Some((id, file)) = rest.split_once('/') {
                if !id.is_empty() {
                    let (name, tail) = file
                        .find(['?', '#'])
                        .map_or((file, ""), |i| file.split_at(i));
                    if let Some(stem) = name.strip_suffix(".avif") {
                        let size = stem.strip_suffix("_static").unwrap_or(stem);
                        if matches!(size, "1x" | "2x" | "3x" | "4x") {
                            u = format!("{pre}{id}/{stem}.webp{tail}");
                        }
                    }
                    break;
                }
            }
        }
    }
    for origin in [
        "https://cdn.heatsync.org",
        "https://heatsync.org",
        "http://heatsync.org",
        "https://www.heatsync.org",
        "http://www.heatsync.org",
        "http://localhost:3001",
        "http://localhost:5173",
    ] {
        if u == origin {
            return "/".into();
        }
        if let Some(rest) = u.strip_prefix(origin) {
            if rest.starts_with('/') {
                return rest.to_owned();
            }
        }
    }
    u
}

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

fn sha256_hex(data: &[u8]) -> String {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for (i, b) in chunk.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [
                t1.wrapping_add(t2),
                v[0],
                v[1],
                v[2],
                v[3].wrapping_add(t1),
                v[4],
                v[5],
                v[6],
            ];
        }
        for (a, b) in h.iter_mut().zip(v) {
            *a = a.wrapping_add(b);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

fn sha_of_url(url: &str) -> String {
    sha256_hex(normalized(url).as_bytes())
}

/// the key a block of this emote is filed under.
pub fn emote_hash(url: &str, scope: Scope) -> String {
    match (provider_id(url), scope) {
        (Some((Kind::Twitch, id)), _) => id.to_owned(),
        (Some((Kind::Plain, id)), Scope::Channel | Scope::Native) => id.to_owned(),
        (Some((Kind::Ffz, id)), Scope::Channel | Scope::Native) => format!("ffz-{id}"),
        _ => sha_of_url(url),
    }
}

/// every key a block of this emote might be filed under by any client.
pub fn all_forms(url: &str) -> Vec<String> {
    let mut v = Vec::with_capacity(3);
    if let Some((k, id)) = provider_id(url) {
        v.push(id.to_owned());
        if k == Kind::Ffz {
            v.push(format!("ffz-{id}"));
        }
        if k == Kind::Twitch {
            return v;
        }
    }
    v.push(sha_of_url(url));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    // shared with desktop (heatsync-chat/src/emotes/block.rs): same vectors, same bytes
    const SEVENTV: &str = "https://cdn.7tv.app/emote/01ABC/1x.webp";
    const SEVENTV_SHA: &str = "b77bff3d9068dbd6ac26734d183ecb9480e63250de106a79ab1c317d0360dc65";

    #[test]
    fn sha256_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // multi-block
        assert_eq!(
            sha256_hex(&[b'a'; 1000]),
            "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
        );
    }

    #[test]
    fn hash_parity_per_source() {
        assert_eq!(emote_hash(SEVENTV, Scope::Channel), "01ABC");
        assert_eq!(
            emote_hash(
                "https://cdn.betterttv.net/emote/5f1b/1x.webp",
                Scope::Channel
            ),
            "5f1b"
        );
        assert_eq!(
            emote_hash(
                "https://cdn.frankerfacez.com/emote/381875/1",
                Scope::Channel
            ),
            "ffz-381875"
        );
        assert_eq!(
            emote_hash("https://files.kick.com/emotes/777/fullsize", Scope::Channel),
            "777"
        );
        let tw = "https://static-cdn.jtvnw.net/emoticons/v2/25/default/dark/1.0";
        for sc in [Scope::Hs, Scope::Channel, Scope::Global, Scope::Native] {
            assert_eq!(emote_hash(tw, sc), "25");
        }
        assert_eq!(emote_hash(SEVENTV, Scope::Hs), SEVENTV_SHA);
        assert_eq!(emote_hash(SEVENTV, Scope::Global), SEVENTV_SHA);
        assert_eq!(
            emote_hash("https://cdn.betterttv.net/emote/5f1b/1x.webp", Scope::Hs),
            "710451d50450b9b8da58fb1f07c244b16ef0b0c4c66c24ba632fb1dd77725fb1"
        );
        assert_eq!(
            emote_hash("https://cdn.frankerfacez.com/emote/381875/1", Scope::Global),
            "83c8be4f483c6ac93edbac7f132bdec8685e0a4c670c79b7d8d18cd0ffa97b2e"
        );
    }

    #[test]
    fn sha_normalises_like_the_server() {
        let want = "4cd6546ec9fb7e1ac16b8d09c7ebfd662df89c94ec09e723dff5c5abc516d91f";
        for u in [
            "https://cdn.heatsync.org/uploads/a.webp",
            "https://heatsync.org/uploads/a.webp",
            "/uploads/a.webp",
        ] {
            assert_eq!(emote_hash(u, Scope::Hs), want, "{u}");
        }
        assert_ne!(
            emote_hash(
                "https://cdn.heatsync.org.evil.com/uploads/a.webp",
                Scope::Hs
            ),
            want
        );
        assert_eq!(
            emote_hash("https://cdn.7tv.app/emote/01ABC/1x.avif", Scope::Hs),
            SEVENTV_SHA
        );
        assert_eq!(
            normalized("https://cdn.7tv.app/emote/01ABC/2x_static.avif?v=1"),
            "https://cdn.7tv.app/emote/01ABC/2x_static.webp?v=1"
        );
        assert_eq!(
            normalized("https://cdn.7tv.app/emote/01ABC/1x.gif"),
            "https://cdn.7tv.app/emote/01ABC/1x.gif"
        );
    }

    #[test]
    fn every_form_is_listed() {
        assert_eq!(
            all_forms(SEVENTV),
            vec!["01ABC".to_owned(), SEVENTV_SHA.to_owned()]
        );
        let ffz = all_forms("https://cdn.frankerfacez.com/emote/381875/1");
        assert_eq!(&ffz[..2], ["381875", "ffz-381875"]);
        assert_eq!(ffz.len(), 3);
        assert_eq!(
            all_forms("https://static-cdn.jtvnw.net/emoticons/v2/25/default/dark/1.0"),
            vec!["25".to_owned()]
        );
    }
}
