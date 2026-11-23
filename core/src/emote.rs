//! emote resolution. same order as the extension: a channel's set is
//! ffz > 7tv > bttv (the server's /api/channel/:c/emotes list puts the winner
//! LAST, so from_list is last-wins), globals are 7tv > bttv > ffz > twitch
//! (from_global_list ranks them), and a twitch/kick token always beats a
//! same-name set emote. message text is word-matched against the result.
//! modifiers (BTTV `w!`-style prefixes / FFZ `ffzX`-style effect words) are
//! parsed here into per-layer [`Effect`] lists; the pixel work happens at render.

use serde::Deserialize;

use crate::complete::Ranked;
use crate::law::{self, Index, Own, Platform, Ref, Room, RoomLists};
use std::collections::HashMap;

/// one emote as the HeatSync emote API returns it.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Emote {
    pub name: String,
    pub url: String,
    pub provider: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub animated: bool,
    #[serde(rename = "zeroWidth", default)]
    pub zero_width: bool,
    /// came from the pooled global set (/api/emotes) rather than a channel's
    /// own set — channel emotes of the same name override it. never serialized.
    #[serde(skip)]
    pub global: bool,
    /// what kind of per-message ref this is (see [`RefKind`]); a set's own
    /// emotes are the default kind and it means nothing for them. never serialized.
    #[serde(skip)]
    pub kind: RefKind,
}

/// what a line's own emote ref is. the law reads them in different tiers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RefKind {
    /// the sender's inventory emote the relay matched to a word (`hsEmotes`) —
    /// a server guess: it sits below the room's own emote of that name.
    #[default]
    Sender,
    /// what the line itself carries: a platform's own mark (twitch's tag,
    /// kick's `[emote:…]`) or a pick the sender's app shipped (`emote_refs`).
    /// it owns its word, so nothing the room or a global has outranks it.
    Pick,
    /// a pick that was taken down: the word is plain text, never a look-alike.
    Gone,
}

impl Emote {
    /// mark as what the line itself carries (see [`RefKind::Pick`]).
    pub fn into_pick(mut self) -> Emote {
        self.kind = RefKind::Pick;
        self
    }

    /// a taken-down pick of `name`: nothing to draw, and nothing else may.
    pub fn gone(name: &str) -> Option<Emote> {
        let mut e = msg_ref(name, "https://gone.invalid/", "", false)?;
        e.url.clear();
        e.kind = RefKind::Gone;
        Some(e)
    }
}

/// the law's view of an emote. the picture is `provider:id`; an emote with no
/// id of its own (a platform token, an owner upload) is its url.
impl crate::law::Pic for Emote {
    fn provider(&self) -> &str {
        &self.provider
    }
    fn id(&self) -> &str {
        if self.id.is_empty() {
            &self.url
        } else {
            &self.id
        }
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn zero_width(&self) -> bool {
        self.zero_width
    }
}

/// the /api/channel/:c/emotes envelope (only the bits we need).
#[derive(Debug, Deserialize)]
pub struct EmoteSetResponse {
    #[serde(default)]
    pub emotes: Vec<Emote>,
    /// the server could not reach a provider and sent what it had: the list is
    /// missing that provider's emotes, so it must not replace a good set.
    /// anything else the server adds to the answer is ignored.
    #[serde(default)]
    pub degraded: bool,
    /// providers (`7tv`, `bttv`, `ffz`) the server answered from a kept copy,
    /// with when that copy was last fresh (ISO). absent when all are fresh.
    #[serde(default, rename = "lastFresh")]
    pub last_fresh: std::collections::BTreeMap<String, String>,
}

/// a degraded answer never removes what a room already holds: per provider
/// (7tv, bttv, ffz), take the answer's emotes when it has any for it (fresh or
/// a kept copy), else keep the held ones; what the answer has for a provider
/// the room holds nothing of is taken as it came. everything else (the owner's,
/// the platform's own) is the answer's.
pub fn degraded_merge(answer: Vec<Emote>, held: Option<&EmoteSet>) -> Vec<Emote> {
    let mut out = answer;
    let Some(held) = held else { return out };
    for p in ["7tv", "bttv", "ffz"] {
        if !out.iter().any(|e| e.provider == p) {
            out.extend(held.channel.iter().filter(|e| e.provider == p).cloned());
        }
    }
    out
}

/// a room's emotes and the pooled globals, each one emote per exact name.
/// the two stay apart because the law puts the sender's own emotes BETWEEN
/// them: the room first, then the sender's, then a global.
#[derive(Clone, Debug)]
pub struct EmoteSet {
    room: Room<Emote>,
    globals: Index<Emote>,
    /// the rows the owner route gave, as given: the owner tier is "came from the
    /// owner route", whatever the emote's own source says.
    owner: Vec<Emote>,
    /// the rows the channel route gave, as given: what a degraded answer keeps a
    /// provider's slice from (including a copy that lost a name collision).
    channel: Vec<Emote>,
}

impl Default for EmoteSet {
    fn default() -> Self {
        EmoteSet {
            // the platform is only read by the send and native-tier rules, which
            // a pooled set never uses; completion and drawing look at names.
            room: Room {
                platform: Platform::Twitch,
                channel: String::new(),
                loaded: true,
                names: Index::new(),
            },
            globals: Index::new(),
            owner: Vec::new(),
            channel: Vec::new(),
        }
    }
}

/// a row on the channel route that is a third party's or the platform's own.
fn is_channel_source(e: &Emote) -> bool {
    matches!(
        e.provider.as_str(),
        "ffz" | "7tv" | "bttv" | "twitch" | "kick"
    )
}

/// the owner rows as the law builds them: one emote per name, sorted.
fn owner_sig(rows: &[Emote]) -> Vec<(String, String)> {
    let lists = RoomLists {
        owner: rows,
        third: &[],
        native: &[],
    };
    let mut v: Vec<(String, String)> = law::build_room(Platform::Twitch, "", lists, true)
        .names
        .into_values()
        .map(|e| (e.name, e.url))
        .collect();
    v.sort_unstable();
    v
}

/// the channel owner's own heatsync emotes (`/api/emotes/user/<channel>`) →
/// emotes. lenient per row: a row with no name or url is dropped, a missing
/// provider is an upload, a root-relative url is anchored on `base`.
pub fn owner_list(v: &serde_json::Value, base: &str) -> Vec<Emote> {
    let Some(rows) = v.get("emotes").and_then(|e| e.as_array()) else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|r| {
            let s = |k: &str| r.get(k).and_then(|x| x.as_str()).filter(|x| !x.is_empty());
            let name = s("name")?;
            let url = s("url")?;
            let url = if url.starts_with('/') && !url.starts_with("//") {
                format!("{base}{url}")
            } else if url.starts_with("https://") {
                url.to_string()
            } else {
                return None;
            };
            Some(Emote {
                name: name.to_string(),
                url,
                provider: s("provider").unwrap_or("upload").to_string(),
                id: s("hash").unwrap_or_default().to_string(),
                animated: false,
                zero_width: false,
                global: false,
                kind: Default::default(),
            })
        })
        .collect()
}

impl EmoteSet {
    pub fn new() -> EmoteSet {
        EmoteSet::default()
    }

    /// the shared empty set — "no per-message refs" without allocating one.
    pub fn none() -> &'static EmoteSet {
        static NONE: std::sync::OnceLock<EmoteSet> = std::sync::OnceLock::new();
        NONE.get_or_init(EmoteSet::new)
    }

    /// build from a channel API list. the law decides every collision, so the
    /// order of the list never matters: the owner's own emotes, then ffz, 7tv,
    /// bttv, then the platform's channel emotes; two of a name in one source
    /// keep the lower id.
    pub fn from_list(list: impl IntoIterator<Item = Emote>) -> EmoteSet {
        let (owner, channel): (Vec<Emote>, Vec<Emote>) =
            list.into_iter().partition(|e| !is_channel_source(e));
        EmoteSet::build(owner, channel, None)
    }

    /// build from the two routes. the owner route's rows are the owner tier
    /// whatever their source (a streamer's 7tv-sourced heatsync emote is still
    /// theirs); the channel route's rows rank by source.
    pub fn from_parts(owner: Vec<Emote>, channel: Vec<Emote>) -> EmoteSet {
        let given = owner.clone();
        let mut owner = owner;
        let mut rest = Vec::new();
        for e in channel {
            // a stray non-third-party row on the channel route is the owner tier too
            if is_channel_source(&e) {
                rest.push(e)
            } else {
                owner.push(e)
            }
        }
        EmoteSet::build(owner, rest, Some(given))
    }

    fn build(owner: Vec<Emote>, channel: Vec<Emote>, given: Option<Vec<Emote>>) -> EmoteSet {
        let (mut third, mut native) = (Vec::new(), Vec::new());
        for e in &channel {
            match e.provider.as_str() {
                "ffz" | "7tv" | "bttv" => third.push(e.clone()),
                _ => native.push(e.clone()),
            }
        }
        let lists = RoomLists {
            owner: &owner,
            third: &third,
            native: &native,
        };
        EmoteSet {
            room: law::build_room(Platform::Twitch, "", lists, true),
            globals: Index::new(),
            owner: given.unwrap_or(owner),
            channel,
        }
    }

    /// build from the pooled global list (`/api/emotes`), stamped global. its
    /// row order says nothing about providers, so the law ranks them: 7tv >
    /// bttv > ffz > twitch > heatsync.
    pub fn from_global_list(list: impl IntoIterator<Item = Emote>) -> EmoteSet {
        let list: Vec<Emote> = list.into_iter().collect();
        let mut set = EmoteSet::new();
        set.globals = law::build_globals(&list);
        set.mark_global();
        set
    }

    /// fold another set in, first-wins — a merged (multi-platform) tab pools
    /// its sources' emotes into one lookup. rooms and globals stay in their own
    /// tiers, so a channel emote beats a pooled global of the same name
    /// whichever arrived first — globals fetch in parallel with channel sets,
    /// so arrival order can't be the precedence rule.
    pub fn merge(&mut self, other: EmoteSet) {
        for (k, v) in other.room.names {
            self.room.names.entry(k).or_insert(v);
        }
        for (k, v) in other.globals {
            self.globals.entry(k).or_insert(v);
        }
    }

    /// stamp every entry as coming from the pooled global set, and file the
    /// room's entries under the globals tier.
    pub fn mark_global(&mut self) {
        for (k, mut e) in std::mem::take(&mut self.room.names) {
            e.global = true;
            self.globals.entry(k).or_insert(e);
        }
        for e in self.globals.values_mut() {
            e.global = true;
        }
    }

    /// does the streamer's own list differ from the one this set was built from?
    /// compared as built (one emote per name), so a duplicate row is not a change.
    pub fn owner_differs(&self, owner: &[Emote]) -> bool {
        owner_sig(&self.owner) != owner_sig(owner)
    }

    /// the emote a word means in this set: the room's, else a global.
    pub fn get(&self, name: &str) -> Option<&Emote> {
        self.room.names.get(name).or_else(|| self.globals.get(name))
    }

    /// the room's own emote of this name (never a global).
    pub fn room_get(&self, name: &str) -> Option<&Emote> {
        self.room.names.get(name)
    }

    /// the pooled global of this name (never the room's).
    pub fn global_get(&self, name: &str) -> Option<&Emote> {
        self.globals.get(name)
    }

    /// what tab offers for `typed`, in the law's order: the exact name and the
    /// room's emotes (`first` — the people go after these), then globals and the
    /// rest. inside each, starts-with, word-start, inside, then shorter, then
    /// a-z. blocked emotes are left out.
    pub fn offer(&self, typed: &str) -> Ranked {
        self.offer_with(typed, None, &[])
    }

    /// [`offer`] with this person's own usage (`(usage, now)`): a name they use
    /// jumps ahead of the room only when what was typed STARTS the name — typing
    /// `pog` does not pull a used `MegaPog` over the room's `PogU`. inside its own
    /// tier a used name still leads. usage is by picture, so it follows the
    /// emote the name means here.
    pub fn offer_with(
        &self,
        typed: &str,
        usage: Option<(&crate::usage::Usage, u64)>,
        mine: &[Emote],
    ) -> Ranked {
        let own = law::build_own(mine, &[], &[]);
        let used = |name: &str| -> f64 {
            usage.map_or(0.0, |(u, now)| {
                self.get(name)
                    .map_or(0.0, |e| u.score(&law::key_of(e), now))
            })
        };
        let usage_fn: &dyn Fn(&str) -> f64 = &used;
        let ctx = law::OfferCtx {
            room: Some(&self.room),
            own: &own,
            globals: &self.globals,
            catalog: &[],
            emoji: &[],
            usage: usage.map(|_| usage_fn),
            pop: None,
            blocked: None,
            suggest_own: true,
        };
        let mut out = Ranked::default();
        for c in law::offer(typed, &ctx, None) {
            if crate::block::is_blocked(c.emote) {
                continue;
            }
            // the people go after what leads: the exact name, a used name that
            // starts with the typed word, and the room's own emotes
            let jumped = c.matched == law::BUCKET_PREFIX && used(&c.emote.name) > 0.0;
            let side = if c.matched == law::BUCKET_EXACT || c.tier == law::Tier::Room || jumped {
                &mut out.first
            } else {
                &mut out.rest
            };
            side.push(c.emote.name.clone());
        }
        out
    }

    /// the text kick receives for `text`: kick draws its own emotes only from
    /// `[emote:id:name]`, so a word that means one of this room's kick emotes
    /// goes out in that form. a line that would not fit with them (kick's
    /// 500-character cap) goes out as the plain words it was typed as. a kick
    /// emote with no numeric id (or a name with `]`, `:` or a space in it) can't
    /// be written that way and stays a word.
    pub fn kick_wire(&self, text: &str) -> String {
        let names: Index<Emote> = self
            .room
            .names
            .iter()
            .filter(|(n, e)| {
                // a token is `[emote:id:name]`: the id must be kick's numeric one
                // and the name must not be able to end or split it
                e.provider != "kick"
                    || (!e.id.is_empty()
                        && e.id.bytes().all(|b| b.is_ascii_digit())
                        && !n.contains([']', ':'])
                        && !n.chars().any(char::is_whitespace))
            })
            .map(|(n, e)| (n.clone(), e.clone()))
            .collect();
        let room = Room {
            platform: Platform::Kick,
            channel: String::new(),
            loaded: true,
            names,
        };
        let own = Own::default();
        let ctx = law::SendCtx {
            room: Some(&room),
            legs: &[],
            own: &own,
            globals: &self.globals,
            picks: None,
            fold: false,
        };
        law::send_line(text, &ctx)
            .wire
            .remove(&Platform::Kick)
            .unwrap_or_else(|| text.to_string())
    }

    pub fn len(&self) -> usize {
        self.room.names.len()
            + self
                .globals
                .keys()
                .filter(|k| !self.room.names.contains_key(*k))
                .count()
    }

    pub fn is_empty(&self) -> bool {
        self.room.names.is_empty() && self.globals.is_empty()
    }
}

/// a pixel effect on an emote layer. BTTV prefix modifiers and FFZ effect
/// words share this vocabulary; the render layer applies them in enum order
/// (geometry → color → motion), so derived Ord IS the application order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Effect {
    RotateL, // l!
    RotateR, // r!
    FlipX,   // h! x! / ffzX
    FlipY,   // v! y! / ffzY
    Wide,    // w! / ffzW ffzWide — double width
    Tall,    // ffzTall — double height
    Cursed,  // c! / ffzCursed — grayscale + darken
    Hyper,   // ffzHyper — oversaturated + shake
    Party,   // p! — saturated hue cycle
    Rainbow, // ffzRainbow — hue cycle (animates even static emotes)
    Shake,   // s! — jitter (animates even static emotes)
    Bounce,  // ffzBounce
    Jam,     // ffzJam — squash/stretch pulse
    Slide,   // ffzSlide — in from the left
    Arrive,  // ffzArrive — grow in
    Leave,   // ffzLeave — shrink out
    Spin,    // ffzSpin
}

/// one-letter stack-key codes (stable — keys are cache identities).
const CODES: [(Effect, char); 17] = [
    (Effect::RotateL, 'l'),
    (Effect::RotateR, 'r'),
    (Effect::FlipX, 'x'),
    (Effect::FlipY, 'y'),
    (Effect::Wide, 'w'),
    (Effect::Tall, 't'),
    (Effect::Cursed, 'c'),
    (Effect::Hyper, 'H'),
    (Effect::Party, 'p'),
    (Effect::Rainbow, 'R'),
    (Effect::Shake, 's'),
    (Effect::Bounce, 'b'),
    (Effect::Jam, 'j'),
    (Effect::Slide, 'd'),
    (Effect::Arrive, 'a'),
    (Effect::Leave, 'e'),
    (Effect::Spin, 'o'),
];

/// a modifier word's meaning: a pixel effect, or `z!` (changes stacking only).
#[derive(Clone, Copy)]
enum Tok {
    Fx(Effect),
    Zero,
}

/// the shared word table, same as the extension's HS_MOD_TOKENS. a key ending
/// in `!` is BTTV-shaped (binds forward); the rest are FFZ (bind backward).
const TOKENS: [(&str, Tok); 25] = [
    ("w!", Tok::Fx(Effect::Wide)),
    ("h!", Tok::Fx(Effect::FlipX)),
    ("v!", Tok::Fx(Effect::FlipY)),
    ("z!", Tok::Zero),
    ("c!", Tok::Fx(Effect::Cursed)),
    ("l!", Tok::Fx(Effect::RotateL)),
    ("r!", Tok::Fx(Effect::RotateR)),
    ("p!", Tok::Fx(Effect::Party)),
    ("s!", Tok::Fx(Effect::Shake)),
    ("x!", Tok::Fx(Effect::FlipX)),
    ("y!", Tok::Fx(Effect::FlipY)),
    ("ffzX", Tok::Fx(Effect::FlipX)),
    ("ffzY", Tok::Fx(Effect::FlipY)),
    ("ffzW", Tok::Fx(Effect::Wide)),
    ("ffzWide", Tok::Fx(Effect::Wide)),
    ("ffzTall", Tok::Fx(Effect::Tall)),
    ("ffzCursed", Tok::Fx(Effect::Cursed)),
    ("ffzHyper", Tok::Fx(Effect::Hyper)),
    ("ffzRainbow", Tok::Fx(Effect::Rainbow)),
    ("ffzBounce", Tok::Fx(Effect::Bounce)),
    ("ffzJam", Tok::Fx(Effect::Jam)),
    ("ffzSlide", Tok::Fx(Effect::Slide)),
    ("ffzArrive", Tok::Fx(Effect::Arrive)),
    ("ffzLeave", Tok::Fx(Effect::Leave)),
    ("ffzSpin", Tok::Fx(Effect::Spin)),
];

/// scales clamp to ±this (extension HS_MOD_MAX_SCALE).
const MAX_SCALE: i32 = 4;
/// geometry repeats kept per layer — past this nothing changes (clamped) and
/// keys stay bounded.
const MAX_REPEAT: usize = 4;

impl Effect {
    /// one-letter code used in stack keys.
    pub fn code(self) -> char {
        CODES.iter().find(|(e, _)| *e == self).map_or('?', |c| c.1)
    }

    fn from_code(c: char) -> Option<Effect> {
        CODES.iter().find(|(_, k)| *k == c).map(|c| c.0)
    }

    /// geometry composes (scales multiply, rotations add) so repeats count;
    /// everything else dedupes.
    fn composes(self) -> bool {
        self <= Effect::Tall
    }

    /// motion effects synthesize frames over time, after color.
    pub fn is_motion(self) -> bool {
        self >= Effect::Shake
    }
}

/// composed geometry of a layer's effects, per hsModComposeAll: scale factors
/// multiply and clamp to ±4 (negative = mirrored), rotations add.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub sx: i32,
    pub sy: i32,
    /// clockwise degrees, always a multiple of 90 in 0..360.
    pub rot: u32,
}

pub fn compose(fx: &[Effect]) -> Geometry {
    let (mut sx, mut sy, mut rot) = (1i32, 1i32, 0i32);
    for f in fx {
        match f {
            Effect::Wide => sx = sx.saturating_mul(2),
            Effect::Tall => sy = sy.saturating_mul(2),
            Effect::FlipX => sx = -sx,
            Effect::FlipY => sy = -sy,
            Effect::RotateL => rot -= 90,
            Effect::RotateR => rot += 90,
            _ => {}
        }
    }
    Geometry {
        sx: sx.clamp(-MAX_SCALE, MAX_SCALE),
        sy: sy.clamp(-MAX_SCALE, MAX_SCALE),
        rot: rot.rem_euclid(360) as u32,
    }
}

/// hue (degrees) of a 3- or 6-digit hex color, rounded — hsModHexToHue.
fn hex_hue(hex: &str) -> Option<u16> {
    let v: Vec<f64> = match hex.len() {
        3 => hex
            .chars()
            .map(|c| c.to_digit(16).map(|d| (d * 17) as f64 / 255.0))
            .collect::<Option<_>>()?,
        6 => (0..3)
            .map(|i| {
                u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16)
                    .ok()
                    .map(|b| b as f64 / 255.0)
            })
            .collect::<Option<_>>()?,
        _ => return None,
    };
    let (r, g, b) = (v[0], v[1], v[2]);
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let d = max - min;
    let mut h = 0.0;
    if d > 0.0 {
        h = if max == r {
            ((g - b) / d) % 6.0
        } else if max == g {
            (b - r) / d + 2.0
        } else {
            (r - g) / d + 4.0
        } * 60.0;
        if h < 0.0 {
            h += 360.0;
        }
    }
    Some(h.round() as u16 % 360)
}

/// a `c!#hex` color token at the start of `s`: (hue, bytes consumed). 6 digits
/// preferred over 3, the `#` optional — the extension's peel regex.
fn color_prefix(s: &str) -> Option<(u16, usize)> {
    let rest = s.strip_prefix("c!")?;
    let (hash, rest) = match rest.strip_prefix('#') {
        Some(r) => (1, r),
        None => (0, rest),
    };
    for n in [6, 3] {
        if let Some(h) = rest
            .get(..n)
            .filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Some((hex_hue(h)?, 2 + hash + n));
        }
    }
    None
}

/// what a modifier word contributes.
#[derive(Clone, Debug, Default, PartialEq)]
struct Mods {
    fx: Vec<Effect>,
    zero: bool,
    hue: Option<u16>,
}

impl Mods {
    fn is_empty(&self) -> bool {
        self.fx.is_empty() && !self.zero && self.hue.is_none()
    }

    fn push_tok(&mut self, t: Tok) {
        match t {
            Tok::Fx(f) => self.fx.push(f),
            Tok::Zero => self.zero = true,
        }
    }

    /// fold later mods in; a later hue wins.
    fn merge(&mut self, o: Mods) {
        self.fx.extend(o.fx);
        self.zero |= o.zero;
        self.hue = o.hue.or(self.hue);
    }
}

fn exact_tok(word: &str) -> Option<Tok> {
    TOKENS.iter().find(|(k, _)| *k == word).map(|t| t.1)
}

/// longest token that starts `s`, optionally limited to BTTV-shaped (`!`) ones.
fn tok_prefix(s: &str, bttv_only: bool) -> Option<(&'static str, Tok)> {
    TOKENS
        .iter()
        .filter(|(k, _)| s.starts_with(k) && (!bttv_only || k.ends_with('!')))
        .max_by_key(|(k, _)| k.len())
        .map(|t| (t.0, t.1))
}

/// peel a word made ENTIRELY of modifier tokens (`w!h!ffzX`, `w!c!#888h!`).
fn peel_chain(word: &str) -> Option<Mods> {
    let mut m = Mods::default();
    let mut rem = word;
    while !rem.is_empty() {
        if let Some((hue, n)) = color_prefix(rem) {
            m.hue = Some(hue);
            rem = &rem[n..];
        } else if let Some((k, t)) = tok_prefix(rem, false) {
            m.push_tok(t);
            rem = &rem[k.len()..];
        } else {
            return None;
        }
    }
    (!m.is_empty()).then_some(m)
}

/// a classified modifier word.
struct Modifier {
    mods: Mods,
    /// binds to the NEXT emote first (BTTV-shaped), else the previous.
    forward: bool,
    /// a single table token / color (vs a peeled chain). only chains defer to
    /// a real emote of the same name.
    exact: bool,
}

fn classify(word: &str) -> Option<Modifier> {
    if let Some(t) = exact_tok(word) {
        let mut mods = Mods::default();
        mods.push_tok(t);
        return Some(Modifier {
            mods,
            forward: word.ends_with('!'),
            exact: true,
        });
    }
    if let Some((hue, n)) = color_prefix(word) {
        if n == word.len() {
            let mods = Mods {
                hue: Some(hue),
                ..Mods::default()
            };
            // c!#hex is BTTV-shaped — forward first like its siblings.
            return Some(Modifier {
                mods,
                forward: true,
                exact: true,
            });
        }
    }
    // chains mirror the extension: direction by the word's trailing `!`.
    peel_chain(word).map(|mods| Modifier {
        mods,
        forward: word.ends_with('!'),
        exact: false,
    })
}

/// one drawable layer of a stack: an emote image plus ITS OWN effects — a
/// modifier binds to one emote, so `Kappa RainTime w!` widens RainTime only.
#[derive(Clone, Debug, PartialEq)]
pub struct Layer {
    pub url: String,
    /// sorted (= application order); geometry may repeat, the rest dedupes.
    pub fx: Vec<Effect>,
    /// `c!#hex` tint as a hue-rotate in degrees.
    pub hue: Option<u16>,
}

impl Layer {
    fn new(url: impl Into<String>) -> Layer {
        Layer {
            url: url.into(),
            fx: Vec::new(),
            hue: None,
        }
    }

    fn add_fx(&mut self, fx: Effect) {
        if fx.composes() {
            if self.fx.iter().filter(|e| **e == fx).count() >= MAX_REPEAT {
                return;
            }
        } else if self.fx.contains(&fx) {
            return;
        }
        let i = self.fx.partition_point(|e| *e <= fx);
        self.fx.insert(i, fx);
    }

    fn apply(&mut self, m: &Mods) {
        for &f in &m.fx {
            self.add_fx(f);
        }
        if m.hue.is_some() {
            self.hue = m.hue;
        }
    }

    /// `#codes@hue` key line, if the layer has any effect.
    fn code_line(&self) -> Option<String> {
        if self.fx.is_empty() && self.hue.is_none() {
            return None;
        }
        let mut s = String::from("#");
        s.extend(self.fx.iter().map(|f| f.code()));
        if let Some(h) = self.hue {
            s.push('@');
            s.push_str(&h.to_string());
        }
        Some(s)
    }

    fn set_codes(&mut self, line: &str) {
        let (codes, hue) = match line.split_once('@') {
            Some((c, h)) => (c, h.parse::<u16>().ok().map(|h| h % 360)),
            None => (line, None),
        };
        for f in codes.chars().filter_map(Effect::from_code) {
            self.add_fx(f);
        }
        self.hue = hue;
    }
}

/// a renderable emote stack: base emote + zero-width overlays, each with its
/// own effects. the key doubles as the render cache identity — each layer's
/// url on its own line, followed by an optional `#codes@hue` line ('\n' and
/// '#'-first lines can't appear in a real url, so the parse back is
/// unambiguous).
#[derive(Clone, Debug, PartialEq)]
pub struct Stack {
    pub base: String, // base emote name — the text fallback while loading
    pub layers: Vec<Layer>,
}

impl Stack {
    fn new(base: &str, layer: Layer) -> Stack {
        Stack {
            base: base.to_string(),
            layers: vec![layer],
        }
    }

    /// the stack renders at least double width (overlays resize onto the base,
    /// so only the base layer's geometry sets the footprint).
    pub fn wide(&self) -> bool {
        self.layers
            .first()
            .is_some_and(|l| compose(&l.fx).sx.abs() >= 2)
    }

    pub fn key(&self) -> String {
        let mut k = String::new();
        for (i, l) in self.layers.iter().enumerate() {
            if i > 0 {
                k.push('\n');
            }
            k.push_str(&l.url);
            if let Some(c) = l.code_line() {
                k.push('\n');
                k.push_str(&c);
            }
        }
        k
    }
}

/// split a stack key back into its layers (render side).
pub fn split_key(key: &str) -> Vec<Layer> {
    let mut layers: Vec<Layer> = Vec::new();
    for line in key.split('\n') {
        match line.strip_prefix('#') {
            Some(codes) => {
                if let Some(l) = layers.last_mut() {
                    l.set_codes(codes);
                }
            }
            None => layers.push(Layer::new(line)),
        }
    }
    layers
}

/// the emote refs one message carries: its own name → url bindings, built from
/// the relay's `hsEmotes`, twitch's `emotes` tag and kick's `[emote:id:name]`
/// tokens. outrank the channel set when parsed (see [`segments_with`]).
pub fn ref_set(refs: &[Emote]) -> EmoteSet {
    EmoteSet::from_list(refs.iter().cloned())
}

/// one per-message ref, or None when the name can't be a chat word or the url
/// isn't a plain https link — this is the trust boundary for relay-supplied
/// emotes (a url ends up fetched by the renderer).
pub fn msg_ref(name: &str, url: &str, provider: &str, zero_width: bool) -> Option<Emote> {
    let name = crate::sanitize::clean(name);
    let ok_url = url.len() <= 512
        && url.starts_with("https://")
        && !url.chars().any(|c| c.is_whitespace() || c.is_control());
    if name.is_empty() || name.chars().any(char::is_whitespace) || !ok_url {
        return None;
    }
    Some(Emote {
        name,
        url: url.to_string(),
        provider: crate::sanitize::clean(provider),
        id: String::new(),
        animated: false,
        zero_width,
        global: false,
        kind: Default::default(),
    })
}

fn push_ref(out: &mut Vec<Emote>, e: Option<Emote>) {
    if let Some(e) = e {
        if !out.iter().any(|o| o.name == e.name) {
            out.push(e);
        }
    }
}

/// twitch's IRC `emotes` tag (`id:start-end,start-end/id2:…`) → refs, naming
/// each emote by the text it covers. positions count unicode code points, not
/// bytes or utf-16 units. a span that is out of range or crosses a space (the
/// relay altered the text under the positions) is skipped, never guessed.
pub fn twitch_native(tag: &str, text: &str) -> Vec<Emote> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    for part in tag.split('/') {
        let Some((id, pos)) = part.split_once(':') else {
            continue;
        };
        // ids are alnum / `_` (v2 ids like `emotesv2_abc`) — anything else
        // would smuggle path or query bits into the cdn url.
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let Some((s, e)) = pos
            .split(',')
            .next()
            .and_then(|p| p.split_once('-'))
            .and_then(|(s, e)| Some((s.parse::<usize>().ok()?, e.parse::<usize>().ok()?)))
        else {
            continue;
        };
        if s > e || e >= chars.len() {
            continue;
        }
        let name: String = chars[s..=e].iter().collect();
        let url = format!("https://static-cdn.jtvnw.net/emoticons/v2/{id}/default/dark/2.0");
        push_ref(
            &mut out,
            msg_ref(&name, &url, "twitch", false).map(Emote::into_pick),
        );
    }
    out
}

/// kick's inline `[emote:ID:name]` tokens → the text with each token replaced
/// by its name (spaced off from neighbors so they read as words) + refs to
/// kick's cdn. a malformed token is left as typed.
pub fn kick_native(text: &str) -> (String, Vec<Emote>) {
    const OPEN: &str = "[emote:";
    let mut out = String::with_capacity(text.len());
    let mut refs = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find(OPEN) {
        out.push_str(&rest[..i]);
        let after = &rest[i + OPEN.len()..];
        let token = after.split_once(']').and_then(|(body, tail)| {
            let (id, name) = body.split_once(':')?;
            let url = format!("https://files.kick.com/emotes/{id}/fullsize");
            let ok = !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit());
            ok.then(|| {
                msg_ref(name, &url, "kick", false).map(|mut e| {
                    // the id is what a send writes back into the token
                    e.id = id.to_string();
                    (e.into_pick(), tail)
                })
            })?
        });
        match token {
            Some((e, tail)) => {
                if !out.is_empty() && !out.ends_with(char::is_whitespace) {
                    out.push(' ');
                }
                out.push_str(&e.name);
                if tail.chars().next().is_some_and(|c| !c.is_whitespace()) {
                    out.push(' ');
                }
                push_ref(&mut refs, Some(e));
                rest = tail;
            }
            None => {
                out.push_str(OPEN);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    (out, refs)
}

/// a laid-out chunk of a message: literal text, or an emote stack.
#[derive(Clone, Debug, PartialEq)]
pub enum Segment {
    Text(String),
    Stack(Stack),
}

/// a line's own refs, sorted into the law's tiers: what the line carries (a
/// platform's mark, a shipped pick, a taken-down pick) and what the relay
/// matched from the sender's inventory.
struct LineView<'a> {
    picks: HashMap<String, Ref<'a, Emote>>,
    sender: Own<Emote>,
}

impl<'a> LineView<'a> {
    fn new(refs: &'a EmoteSet) -> LineView<'a> {
        let mut picks = HashMap::new();
        let mut sender = Own::default();
        for e in refs.room.names.values().chain(refs.globals.values()) {
            match e.kind {
                RefKind::Pick => {
                    picks.insert(e.name.clone(), Ref::Pic(e));
                }
                RefKind::Gone => {
                    picks.insert(e.name.clone(), Ref::Gone);
                }
                RefKind::Sender => {
                    sender.inventory.insert(e.name.clone(), e.clone());
                }
            }
        }
        LineView { picks, sender }
    }
}

/// name → emote by the emote law, for one line: its own picks and platform
/// marks, then the room's emote, then the sender's inventory emote, then a
/// global. a loading room is empty and borrows nothing. every word goes
/// through `law::read_word`; there is no second order here.
struct Lookup<'a> {
    set: &'a EmoteSet,
    view: &'a LineView<'a>,
}

impl<'a> Lookup<'a> {
    fn ctx(&self) -> law::ReadCtx<'a, 'a, Emote> {
        let (set, view): (&'a EmoteSet, &'a LineView<'a>) = (self.set, self.view);
        law::ReadCtx {
            room: Some(&set.room),
            refs: Some(&view.picks),
            sender: Some(&view.sender),
            globals: &set.globals,
            fold: false,
        }
    }

    /// one exact spelling (no overlay reading): what the modifier scans and the
    /// block keys ask. a taken-down pick has no emote to give.
    fn get(&self, name: &str) -> Option<&'a Emote> {
        law::read_exact(name, &self.ctx(), false).and_then(|(e, _)| e)
    }
}

/// an emote word resolved to a layer's worth of data.
struct Resolved<'a> {
    emote: &'a Emote,
    /// stacks onto the previous emote instead of starting a new one.
    overlay: bool,
    /// effects glued onto the word (`Kappaw!`, `w!Kappa`).
    mods: Mods,
}

/// word → emote. a literal name always wins; then the 7tv `Name0` overlay
/// convention (only when no emote is literally named `Name0`), then glued
/// modifier suffixes (`Kappaw!`, `KappaffzX`, `Kappac!#888`, chains), then
/// glued BTTV prefixes (`w!h!Kappa`).
fn resolve<'a>(word: &str, look: &Lookup<'a>) -> Option<Resolved<'a>> {
    // the word as typed, then minus a trailing 0 (the overlay reading) — the
    // law's own two steps. a taken-down pick is plain text and nothing else.
    if let Some(hit) = law::read_word(word, &look.ctx()) {
        return hit.emote.map(|emote| Resolved {
            emote,
            overlay: hit.overlay,
            mods: Mods::default(),
        });
    }
    // every modifier spelling has a `!` or starts `ffz` — skip the peel scans
    // for the (vast majority) plain words.
    if !word.contains('!') && !word.contains("ffz") {
        return None;
    }
    // longest base first, at least two chars
    for (i, _) in word.char_indices().rev() {
        if i < 2 {
            break;
        }
        let (base, rest) = word.split_at(i);
        if let Some(mods) = peel_chain(rest) {
            if let Some(e) = look.get(base) {
                return Some(Resolved {
                    emote: e,
                    overlay: e.zero_width || mods.zero,
                    mods,
                });
            }
        }
    }
    let mut mods = Mods::default();
    let mut rem = word;
    while let Some((k, t)) = tok_prefix(rem, true).filter(|(k, _)| rem.len() > k.len()) {
        mods.push_tok(t);
        rem = &rem[k.len()..];
    }
    if rem.len() < word.len() {
        if let Some(e) = look.get(rem) {
            return Some(Resolved {
                emote: e,
                overlay: e.zero_width || mods.zero,
                mods,
            });
        }
    }
    None
}

/// is the next non-modifier word an emote? modifiers in between are skipped so
/// `c! ffzX Kappa` lands both on Kappa; a plain word stops the scan.
fn emote_ahead(words: &[&str], from: usize, look: &Lookup<'_>) -> bool {
    for w in &words[from.min(words.len())..] {
        if is_modifier(w, look) {
            continue;
        }
        return resolve(w, look).is_some();
    }
    false
}

/// a modifier word — unless it is exactly an emote's name (only chains yield
/// to a same-named emote; the shared table tokens win, as in the extension).
fn is_modifier(word: &str, look: &Lookup<'_>) -> bool {
    classify(word).is_some_and(|c| c.exact || look.get(word).is_none())
}

/// the emotes a line names by their literal word (refs first, then the set) —
/// blocked ones included, so a block key can still be aimed at them. in order,
/// each exact emote once.
pub fn emotes_in(content: &str, set: &EmoteSet, refs: &EmoteSet) -> Vec<Emote> {
    let view = LineView::new(refs);
    let look = Lookup { set, view: &view };
    let mut out: Vec<Emote> = Vec::new();
    for w in content.split_whitespace() {
        if let Some(e) = look.get(w) {
            if !out.iter().any(|o| o.url == e.url) {
                out.push(e.clone());
            }
        }
    }
    out
}

/// parse message text into text + emote-stack segments. handles:
///   - zero-width emotes overlaying the preceding base (`GAMBA notL`) and the
///     `Name0` overlay convention
///   - BTTV modifiers (`w! h! v! z! c! l! r! p! s! x! y!`, `c!#hex`) binding
///     FORWARD to the next emote, bare (`w! KEKW`) or attached (`w!KEKW`)
///   - FFZ words (`ffzX ffzW ffzRainbow …`) binding BACKWARD to the last
///     layer of the stack (an overlay, not the base)
///   - the opposite direction as fallback when the canonical side has no emote
///   - glued suffixes (`Kappaw!`, `KappaffzX`, `Kappac!#888`) and chains
///     (`w!h!ffzX`)
///
/// a modifier with no emote on either side stays literal text. adjacent text
/// words coalesce single-spaced. input is assumed already sanitized.
pub fn segments(content: &str, set: &EmoteSet) -> Vec<Segment> {
    segments_with(content, set, &EmoteSet::new())
}

/// [`segments`] with a per-message ref set that outranks `set` — see [`Lookup`].
pub fn segments_with(content: &str, set: &EmoteSet, refs: &EmoteSet) -> Vec<Segment> {
    let view = LineView::new(refs);
    let look = Lookup { set, view: &view };
    let words: Vec<&str> = content.split_whitespace().collect();
    let mut out: Vec<Segment> = Vec::new();
    let mut text = String::new();
    // forward modifiers waiting for their emote, plus their literal words to
    // restore if it never comes.
    let mut pending = Mods::default();
    let mut pending_words: Vec<&str> = Vec::new();

    fn push_text(text: &mut String, w: &str) {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(w);
    }

    for (i, &word) in words.iter().enumerate() {
        if let Some(c) = classify(word).filter(|c| c.exact || look.get(word).is_none()) {
            let ahead = emote_ahead(&words, i + 1, &look);
            // the stack this word could modify: nothing may sit between them.
            let back = text.is_empty() && matches!(out.last(), Some(Segment::Stack(_)));
            // a bare `z!` means "overlay the next emote" — nothing to give a
            // previous one.
            let back_ok = back && (!c.mods.fx.is_empty() || c.mods.hue.is_some());
            let fwd = if c.forward { ahead } else { !back_ok && ahead };
            if fwd {
                pending.merge(c.mods);
                pending_words.push(word);
                continue;
            }
            if back_ok {
                if let Some(Segment::Stack(s)) = out.last_mut() {
                    if let Some(l) = s.layers.last_mut() {
                        l.apply(&c.mods);
                        continue;
                    }
                }
            }
            // no emote either side — literal.
            for w in pending_words.drain(..) {
                push_text(&mut text, w);
            }
            pending = Mods::default();
            push_text(&mut text, word);
            continue;
        }

        let Some(r) = resolve(word, &look) else {
            for w in pending_words.drain(..) {
                push_text(&mut text, w);
            }
            pending = Mods::default();
            push_text(&mut text, word);
            continue;
        };
        // a blocked emote is hidden outright: its word, and any modifiers
        // waiting for it, vanish — the exact emote only, never the name.
        if crate::block::is_blocked(r.emote) {
            pending = Mods::default();
            pending_words.clear();
            continue;
        }
        let mut mods = std::mem::take(&mut pending);
        pending_words.clear();
        mods.merge(r.mods);
        let overlay = r.overlay || mods.zero;
        let mut layer = Layer::new(r.emote.url.clone());
        layer.apply(&mods);
        if overlay && text.is_empty() {
            if let Some(Segment::Stack(s)) = out.last_mut() {
                s.layers.push(layer);
                continue;
            }
        }
        if !text.is_empty() {
            out.push(Segment::Text(std::mem::take(&mut text)));
        }
        out.push(Segment::Stack(Stack::new(&r.emote.name, layer)));
    }
    for w in pending_words.drain(..) {
        push_text(&mut text, w);
    }
    if !text.is_empty() {
        out.push(Segment::Text(text));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(name: &str) -> Emote {
        Emote {
            name: name.into(),
            url: format!("https://cdn/{name}.webp"),
            provider: "7tv".into(),
            id: name.into(),
            animated: false,
            zero_width: false,
            global: false,
            kind: Default::default(),
        }
    }

    fn zw(name: &str) -> Emote {
        Emote {
            zero_width: true,
            ..e(name)
        }
    }

    fn native(name: &str, provider: &str) -> Emote {
        Emote {
            url: format!("https://native/{name}"),
            provider: provider.into(),
            ..e(name)
        }
        .into_pick()
    }

    /// the standard cast: bases, an overlay, a literal `Name0`.
    fn cast() -> EmoteSet {
        EmoteSet::from_list([
            e("KEKW"),
            e("GAMBA"),
            e("Kappa"),
            e("Keepo"),
            e("lerolero"),
            zw("notL"),
        ])
    }

    /// compact view of a parse: stacks as `base+overlay` with `[codes@hue]`
    /// per layer, text in quotes, segments space-joined.
    fn show_with(msg: &str, set: &EmoteSet, refs: &EmoteSet) -> String {
        segments_with(msg, set, refs)
            .iter()
            .map(|s| match s {
                Segment::Text(t) => format!("'{t}'"),
                Segment::Stack(st) => st
                    .layers
                    .iter()
                    .map(|l| {
                        let name = l.url.rsplit('/').next().unwrap();
                        let name = name.strip_suffix(".webp").unwrap_or(name);
                        let codes = l.code_line().unwrap_or_default();
                        match codes.strip_prefix('#') {
                            Some(c) => format!("{name}[{c}]"),
                            None => name.to_string(),
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("+"),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn show(msg: &str) -> String {
        show_with(msg, &cast(), &EmoteSet::new())
    }

    #[track_caller]
    fn table(cases: &[(&str, &str)]) {
        for (input, want) in cases {
            assert_eq!(show(input), *want, "input: {input}");
        }
    }

    #[test]
    fn channel_emote_overrides_global_regardless_of_arrival_order() {
        // globals landed first
        let mut set = EmoteSet::new();
        let mut globals = EmoteSet::from_list([e("Kappa")]);
        globals.mark_global();
        set.merge(globals.clone());
        set.merge(EmoteSet::from_list([Emote {
            provider: "bttv".into(),
            ..e("Kappa")
        }]));
        assert_eq!(set.get("Kappa").unwrap().provider, "bttv");
        // channel set landed first
        let mut set = EmoteSet::from_list([Emote {
            provider: "bttv".into(),
            ..e("Kappa")
        }]);
        set.merge(globals);
        assert_eq!(set.get("Kappa").unwrap().provider, "bttv");
        assert_eq!(set.len(), 1);
    }

    /// an emote the sender's inventory brought along, not a platform token.
    fn sender(name: &str) -> Emote {
        Emote {
            url: format!("https://sender/Sender{name}.webp"),
            provider: "heatsync".into(),
            ..e(name)
        }
    }

    #[test]
    fn a_senders_inventory_emote_gives_way_to_the_room() {
        // the room says Pog is the 7tv one; the sender's own Pog is only the
        // tier below the room (law: room, then the sender's, then globals).
        let room = EmoteSet::from_list([e("Pog")]);
        let refs = ref_set(&[sender("Pog")]);
        assert_eq!(show_with("Pog", &room, &refs), "Pog");
    }

    #[test]
    fn a_platform_token_still_beats_the_room() {
        let room = EmoteSet::from_list([e("Pog")]);
        let refs = ref_set(&[sender("Pog").into_pick()]);
        assert_eq!(show_with("Pog", &room, &refs), "SenderPog");
    }

    /// what a line draws, as a list of the picture each word shows (`name` of
    /// the url's last segment) or the word itself in quotes when it stays text.
    fn drawn(line: &str, room: &EmoteSet, refs: &[Emote]) -> String {
        show_with(line, room, &ref_set(refs))
    }

    fn picked(name: &str, tag: &str) -> Emote {
        Emote {
            url: format!("https://pick/{tag}.webp"),
            provider: "7tv".into(),
            ..e(name)
        }
        .into_pick()
    }

    #[test]
    fn a_pick_the_sender_shipped_beats_the_room_and_a_global() {
        // vector: "a line's own pick beats the room" — read through the real
        // drawing path, not the law helper
        let room = EmoteSet::from_list([e("Pog")]);
        assert_eq!(
            drawn("Pog", &room, &[picked("Pog", "TheirPog")]),
            "TheirPog"
        );
        let mut globals = EmoteSet::from_global_list([e("Pog")]);
        assert_eq!(
            drawn("Pog", &globals, &[picked("Pog", "TheirPog")]),
            "TheirPog"
        );
        // the same name from the relay's inventory match gives way to the room
        assert_eq!(drawn("Pog", &room, &[sender("Pog")]), "Pog");
        globals.merge(room);
        assert_eq!(drawn("Pog", &globals, &[sender("Pog")]), "Pog");
    }

    #[test]
    fn a_taken_down_pick_is_text_never_a_look_alike() {
        let room = EmoteSet::from_list([e("Pog"), e("Kappa")]);
        let gone = Emote::gone("Pog").unwrap();
        assert_eq!(
            drawn("Pog Kappa", &room, std::slice::from_ref(&gone)),
            "'Pog' Kappa"
        );
        // the overlay reading and a modifier do not bring the room's back
        assert_eq!(drawn("Pog0", &room, std::slice::from_ref(&gone)), "'Pog0'");
        assert_eq!(drawn("w!Pog", &room, &[gone]), "'w!Pog'");
    }

    #[test]
    fn a_pick_can_be_an_overlay_name_zero_as_the_law_says() {
        let room = EmoteSet::from_list([e("GAMBA")]);
        // picked Pog drawn as `Pog0` over the emote before it
        assert_eq!(
            drawn("GAMBA Pog0", &room, &[picked("Pog", "TheirPog")]),
            "GAMBA+TheirPog"
        );
    }

    #[test]
    fn the_senders_emote_beats_a_global_and_the_room_beats_the_sender() {
        let mut globals = EmoteSet::from_list([e("Pog")]);
        globals.mark_global();
        let refs = ref_set(&[sender("Pog")]);
        assert_eq!(show_with("Pog", &globals, &refs), "SenderPog");
        let mut both = EmoteSet::from_list([Emote {
            url: "https://cdn/RoomPog.webp".into(),
            ..e("Pog")
        }]);
        both.merge(globals);
        assert_eq!(show_with("Pog", &both, &refs), "RoomPog");
    }

    #[test]
    fn a_room_set_does_not_depend_on_the_order_it_arrived_in() {
        let p = |prov: &str, id: &str| Emote {
            provider: prov.into(),
            id: id.into(),
            ..e("Kappa")
        };
        // two of one name from one provider: the lower id, whichever came first
        for rows in [
            vec![p("7tv", "b"), p("7tv", "a")],
            vec![p("7tv", "a"), p("7tv", "b")],
        ] {
            assert_eq!(EmoteSet::from_list(rows).get("Kappa").unwrap().id, "a");
        }
        // ffz over 7tv over bttv, and the owner's own upload over all three
        for rows in [
            vec![p("ffz", "1"), p("7tv", "1")],
            vec![p("7tv", "1"), p("ffz", "1")],
        ] {
            assert_eq!(
                EmoteSet::from_list(rows).get("Kappa").unwrap().provider,
                "ffz"
            );
        }
        for rows in [
            vec![p("upload", "1"), p("ffz", "1")],
            vec![p("ffz", "1"), p("upload", "1")],
        ] {
            assert_eq!(
                EmoteSet::from_list(rows).get("Kappa").unwrap().provider,
                "upload"
            );
        }
    }

    #[test]
    fn what_a_platform_marks_is_a_token_and_what_the_relay_sends_is_not() {
        let tw = twitch_native("25:0-4", "Kappa hi");
        assert!(tw.len() == 1 && tw.iter().all(|e| e.kind == RefKind::Pick));
        let (_, kick) = kick_native("[emote:37226:KEKW] hi");
        assert!(kick.len() == 1 && kick.iter().all(|e| e.kind == RefKind::Pick));
        assert_eq!(
            msg_ref("Pog", "https://x/y", "heatsync", false)
                .unwrap()
                .kind,
            RefKind::Sender
        );
    }

    fn kick_emote(name: &str, id: &str) -> Emote {
        Emote {
            provider: "kick".into(),
            id: id.into(),
            url: format!("https://files.kick.com/emotes/{id}/fullsize"),
            ..e(name)
        }
    }

    fn pog_world(champ_id: &str) -> EmoteSet {
        let mut set = EmoteSet::from_list([e("PogU")]);
        set.merge(EmoteSet::from_global_list([
            e("MegaPog"),
            Emote {
                id: champ_id.into(),
                ..e("PogChamp")
            },
        ]));
        set
    }

    #[test]
    fn a_used_name_jumps_the_room_only_when_the_typed_word_starts_it() {
        use crate::usage::Usage;
        let set = pog_world("PogChamp");
        let mut u = Usage::default();
        for _ in 0..9 {
            u.note("7tv:MegaPog", 10); // heavily used, but `pog` only sits inside it
        }
        u.note("7tv:PogChamp", 10); // lightly used, and `pog` starts it
        let got = set.offer_with("pog", Some((&u, 10)), &[]);
        // the used one that starts with `pog` leads the room; MegaPog stays behind it
        assert_eq!(got.first, ["PogChamp", "PogU"]);
        assert_eq!(got.rest, ["MegaPog"]);
        // no usage, no jump
        let plain = set.offer("pog");
        assert_eq!(plain.first, ["PogU"]);
        assert_eq!(plain.rest, ["PogChamp", "MegaPog"]);
    }

    #[test]
    fn your_own_emotes_come_after_the_room_and_before_globals() {
        let mine = |n: &str| Emote {
            provider: "hs".into(),
            url: format!("https://hs/{n}.webp"),
            ..e(n)
        };
        let mut set = EmoteSet::from_list([e("PogU")]);
        set.merge(EmoteSet::from_global_list([e("PogChamp")]));
        let got = set.offer_with("pog", None, &[mine("PogMine"), mine("PogU")]);
        // the room leads; yours then sit ahead of the globals, in the law's order
        assert_eq!(got.first[0], "PogU");
        let rest: Vec<&str> = got.rest.iter().map(String::as_str).collect();
        let (own_at, global_at) = (
            rest.iter().position(|n| *n == "PogMine").unwrap(),
            rest.iter().position(|n| *n == "PogChamp").unwrap(),
        );
        assert!(own_at < global_at, "{rest:?}");
        // none of yours: just the room and the globals, as before
        assert_eq!(set.offer("pog").first, ["PogU"]);
    }

    #[test]
    fn usage_belongs_to_the_picture_not_the_name() {
        use crate::usage::Usage;
        let mut u = Usage::default();
        u.note("7tv:PogChamp", 10);
        // another emote that is now called PogChamp is not the one that was used
        let set = pog_world("a-different-id");
        let got = set.offer_with("pog", Some((&u, 10)), &[]);
        assert_eq!(got.first, ["PogU"]);
    }

    #[test]
    fn last_months_use_no_longer_jumps_the_room_much_but_a_fresh_one_does() {
        use crate::usage::Usage;
        let set = pog_world("PogChamp");
        let mut u = Usage::default();
        u.note("7tv:PogChamp", 0);
        // a still-positive but faded score still counts as "used": the order
        // inside the group is what decays, so a fresher one beats it
        let day = 86_400;
        let mut both = u.clone();
        both.note("7tv:PogU", 90 * day);
        let got = set.offer_with("pog", Some((&both, 90 * day)), &[]);
        assert_eq!(got.first[0], "PogU");
    }

    #[test]
    fn kick_wire_writes_a_rooms_kick_emote_as_kicks_token_and_nothing_else() {
        let set = EmoteSet::from_list([kick_emote("KEKW", "37226"), e("GAMBA")]);
        // a 7tv emote and a plain word stay words; an overlay stays a word too
        assert_eq!(
            set.kick_wire("KEKW GAMBA hi KEKW0  KEKW"),
            "[emote:37226:KEKW] GAMBA hi KEKW0  [emote:37226:KEKW]"
        );
        assert_eq!(EmoteSet::new().kick_wire("KEKW"), "KEKW");
    }

    #[test]
    fn a_kick_name_that_could_end_the_token_early_stays_a_word() {
        for name in ["a]b", "a:b"] {
            let set = EmoteSet::from_list([kick_emote(name, "1")]);
            assert_eq!(set.kick_wire(name), name);
        }
    }

    #[test]
    fn a_higher_room_source_of_the_same_name_keeps_the_word_plain_on_kick() {
        // the room says KEKW is the 7tv one: kick would draw nothing for a token of the other
        let set = EmoteSet::from_list([kick_emote("KEKW", "37226"), e("KEKW")]);
        assert_eq!(set.kick_wire("KEKW"), "KEKW");
    }

    #[test]
    fn collisions_go_to_the_higher_provider_whatever_the_order() {
        // ffz > 7tv > bttv, however the list is ordered
        let p = |n: &str| Emote {
            provider: n.into(),
            ..e("Kappa")
        };
        let set = EmoteSet::from_list([p("bttv"), p("7tv"), p("ffz")]);
        assert_eq!(set.get("Kappa").unwrap().provider, "ffz");
        assert_eq!(set.len(), 1);
        let set = EmoteSet::from_list([p("bttv"), p("7tv")]);
        assert_eq!(set.get("Kappa").unwrap().provider, "7tv");
    }

    #[test]
    fn globals_rank_7tv_bttv_ffz_whatever_the_row_order() {
        let p = |n: &str| Emote {
            provider: n.into(),
            ..e("Kappa")
        };
        for rows in [
            vec![p("7tv"), p("bttv"), p("ffz"), p("twitch")],
            vec![p("twitch"), p("ffz"), p("bttv"), p("7tv")],
            vec![p("ffz"), p("7tv"), p("twitch"), p("bttv")],
        ] {
            let set = EmoteSet::from_global_list(rows);
            let k = set.get("Kappa").unwrap();
            assert_eq!(k.provider, "7tv");
            assert!(k.global);
        }
        let set = EmoteSet::from_global_list([p("twitch"), p("ffz")]);
        assert_eq!(set.get("Kappa").unwrap().provider, "ffz");
    }

    #[test]
    fn text_and_emotes_coalesce() {
        table(&[
            (
                "lol KEKW that GAMBA moment",
                "'lol' KEKW 'that' GAMBA 'moment'",
            ),
            ("a  b\tc", "'a b c'"),
            ("", ""),
        ]);
    }

    #[test]
    fn bttv_binds_forward() {
        table(&[
            ("w!KEKW", "KEKW[w]"),
            ("w! KEKW", "KEKW[w]"),
            ("w!h!KEKW", "KEKW[xw]"), // sorted = application order
            ("x! KEKW", "KEKW[x]"),
            ("y! KEKW", "KEKW[y]"),
            ("v! KEKW", "KEKW[y]"),
            ("l! KEKW", "KEKW[l]"),
            ("r! KEKW", "KEKW[r]"),
            ("c! KEKW", "KEKW[c]"),
            ("p! KEKW", "KEKW[p]"),
            ("s! KEKW", "KEKW[s]"),
            // canonical side first: c! belongs to Keepo, not Kappa
            ("Kappa c! Keepo", "Kappa Keepo[c]"),
            // a run of modifiers all land on the emote ahead
            ("c! ffzX Kappa", "Kappa[xc]"),
            ("w! h! KEKW", "KEKW[xw]"),
        ]);
    }

    #[test]
    fn ffz_binds_backward() {
        table(&[
            ("KEKW ffzX", "KEKW[x]"),
            ("KEKW ffzY", "KEKW[y]"),
            ("KEKW ffzW", "KEKW[w]"),
            ("KEKW ffzWide", "KEKW[w]"),
            ("KEKW ffzTall", "KEKW[t]"),
            ("KEKW ffzCursed", "KEKW[c]"),
            ("KEKW ffzHyper", "KEKW[H]"),
            ("KEKW ffzRainbow", "KEKW[R]"),
            ("KEKW ffzBounce", "KEKW[b]"),
            ("KEKW ffzJam", "KEKW[j]"),
            ("KEKW ffzSlide", "KEKW[d]"),
            ("KEKW ffzArrive", "KEKW[a]"),
            ("KEKW ffzLeave", "KEKW[e]"),
            ("KEKW ffzSpin", "KEKW[o]"),
            ("KEKW ffzX ffzRainbow", "KEKW[xR]"),
            // canonical side first: ffzX belongs to Kappa, not Keepo
            ("Kappa ffzX Keepo", "Kappa[x] Keepo"),
            // the LAST layer, not the base
            ("GAMBA notL ffzX", "GAMBA+notL[x]"),
        ]);
    }

    #[test]
    fn fallback_direction_when_canonical_side_has_no_emote() {
        table(&[
            ("Kappa w!", "Kappa[w]"),   // bttv, nothing ahead → previous
            ("ffzX Kappa", "Kappa[x]"), // ffz, nothing behind → next
            ("Kappa c!#00f", "Kappa[@240]"),
        ]);
    }

    #[test]
    fn orphan_modifier_stays_literal() {
        table(&[
            ("w! nothing here", "'w! nothing here'"),
            ("lol ffzX", "'lol ffzX'"),
            ("KEKW lol ffzX", "KEKW 'lol ffzX'"), // adjacency required
            ("w! ffzX", "'w! ffzX'"),
            ("c!#00f", "'c!#00f'"),
            ("z!", "'z!'"),
            ("hi w!h!ffzX", "'hi w!h!ffzX'"),
        ]);
    }

    #[test]
    fn glued_suffixes() {
        table(&[
            ("Kappaw!", "Kappa[w]"),
            ("KappaffzX", "Kappa[x]"),
            ("KappaffzRainbow", "Kappa[R]"),
            ("KappaffzLeave", "Kappa[e]"),
            ("Kappac!#0f0", "Kappa[@120]"),
            ("Kappac!888", "Kappa[@0]"),
            ("Kappaw!h!ffzX", "Kappa[xxw]"),
            ("Kappaw!c!#00f", "Kappa[w@240]"),
            // an overlay with a glued modifier still overlays
            ("GAMBA notLw!", "GAMBA+notL[w]"),
        ]);
    }

    #[test]
    fn glued_prefixes_still_work() {
        table(&[("w!h!KEKW", "KEKW[xw]"), ("z!KEKW", "KEKW")]);
    }

    #[test]
    fn chained_words_follow_the_trailing_bang() {
        table(&[
            // ends in a letter → ffz-shaped → previous emote
            ("Kappa w!h!ffzX", "Kappa[xxw]"),
            ("Kappa Keepo w!h!ffzX", "Kappa Keepo[xxw]"),
            // ends in `!` → bttv-shaped → next emote
            ("Kappa w!h! Keepo", "Kappa Keepo[xw]"),
            ("w!c!#00fh! Kappa", "Kappa[xw@240]"),
        ]);
    }

    #[test]
    fn hex_colors_tint_and_latest_wins() {
        table(&[
            ("c!#f00 Kappa", "Kappa[@0]"),
            ("c!#0f0 Kappa", "Kappa[@120]"),
            ("c!#0000ff Kappa", "Kappa[@240]"),
            ("c!00f Kappa", "Kappa[@240]"), // `#` optional
            ("c!#f00 c!#00f Kappa", "Kappa[@240]"),
        ]);
        assert_eq!(hex_hue("ff8700"), Some(32));
        assert_eq!(hex_hue("zzz"), None);
        assert_eq!(hex_hue("12"), None);
    }

    #[test]
    fn composition_per_hscomposeall() {
        // scales multiply and clamp to ±4, rotations add, effects dedupe
        table(&[
            ("w! w! KEKW", "KEKW[ww]"),
            ("KEKW ffzRainbow ffzRainbow", "KEKW[R]"),
            ("KEKW ffzHyper ffzHyper s!", "KEKW[Hs]"),
            ("w!w!w!w!w!w!KEKW", "KEKW[wwww]"), // capped, keys stay bounded
        ]);
        let g = |fx: &[Effect]| compose(fx);
        assert_eq!((g(&[]).sx, g(&[]).sy, g(&[]).rot), (1, 1, 0));
        let wide3 = g(&[Effect::Wide; 3]);
        assert_eq!(wide3.sx, 4); // 8 → clamped
        assert_eq!(g(&[Effect::FlipX, Effect::FlipX]).sx, 1); // mirrors cancel
        assert_eq!(g(&[Effect::FlipX, Effect::Wide]).sx, -2);
        assert_eq!(g(&[Effect::Tall]).sy, 2);
        assert_eq!(g(&[Effect::RotateL, Effect::RotateL]).rot, 180);
        assert_eq!(g(&[Effect::RotateL, Effect::RotateR]).rot, 0);
        assert_eq!(g(&[Effect::RotateL]).rot, 270);
        assert_eq!(g(&[Effect::RotateR; 4]).rot, 0);
    }

    #[test]
    fn wide_is_the_base_layers_footprint() {
        let set = cast();
        let wide = |m: &str| match &segments(m, &set)[0] {
            Segment::Stack(s) => s.wide(),
            _ => panic!("no stack"),
        };
        assert!(wide("w! KEKW"));
        assert!(wide("KEKW ffzWide"));
        assert!(!wide("KEKW ffzTall"));
        assert!(wide("w! w! w! KEKW")); // clamped, still wide
        assert!(wide("w! h! KEKW")); // mirrored 2x is still wide
        assert!(!wide("GAMBA notL ffzW")); // overlay resizes onto the base
        assert!(!wide("KEKW"));
    }

    #[test]
    fn zero_width_overlays_stack() {
        table(&[
            ("GAMBA notL", "GAMBA+notL"),
            ("GAMBA notL notL", "GAMBA+notL+notL"),
            ("notL", "notL"), // nothing to overlay → base
            ("GAMBA lol notL", "GAMBA 'lol' notL"),
            ("GAMBA w! notL", "GAMBA+notL[w]"),
            ("GAMBA notL w!", "GAMBA+notL[w]"),
        ]);
    }

    #[test]
    fn z_forces_overlay() {
        table(&[
            ("GAMBA z!KEKW", "GAMBA+KEKW"),
            ("GAMBA z! KEKW", "GAMBA+KEKW"),
            ("GAMBA z!w!KEKW", "GAMBA+KEKW[w]"),
            ("GAMBA z!", "GAMBA 'z!'"), // z! only looks forward
        ]);
    }

    #[test]
    fn name0_overlay_convention() {
        table(&[
            ("GAMBA lerolero0", "GAMBA+lerolero"),
            ("lerolero0", "lerolero"), // no previous stack → base
        ]);
        // an emote literally named lerolero0 is its own emote
        let set = EmoteSet::from_list([e("GAMBA"), e("lerolero"), e("lerolero0")]);
        assert_eq!(
            show_with("GAMBA lerolero0", &set, &EmoteSet::new()),
            "GAMBA lerolero0"
        );
        // not an emote at all → text
        assert_eq!(show("nope0"), "'nope0'");
        assert_eq!(show("0"), "'0'");
    }

    #[test]
    fn exact_emote_name_beats_chain_parse() {
        let set = EmoteSet::from_list([e("h!i"), e("w!h!")]);
        assert_eq!(show_with("h!i", &set, &EmoteSet::new()), "h!i");
        // an emote named like a chain wins over parsing it as modifiers
        assert_eq!(show_with("w!h!", &set, &EmoteSet::new()), "w!h!");
    }

    #[test]
    fn a_platform_token_outranks_the_set_and_the_senders_beat_a_global() {
        let set = EmoteSet::from_list([e("Kappa")]);
        let heat = Emote {
            url: "https://hs/Kappa".into(),
            provider: "heatsync".into(),
            ..e("Kappa")
        };
        // the room's emote beats the sender's inventory one of the same name
        let refs = EmoteSet::from_list([heat.clone()]);
        match &segments_with("Kappa", &set, &refs)[0] {
            Segment::Stack(s) => assert_eq!(s.layers[0].url, "https://cdn/Kappa.webp"),
            _ => panic!(),
        }
        // ...and outranks a global
        let mut globals = EmoteSet::from_list([e("Kappa")]);
        globals.mark_global();
        match &segments_with("Kappa", &globals, &refs)[0] {
            Segment::Stack(s) => assert_eq!(s.layers[0].url, "https://hs/Kappa"),
            _ => panic!(),
        }
        // a native twitch token beats the same-name set emote
        let refs = EmoteSet::from_list([native("Kappa", "twitch")]);
        match &segments_with("Kappa", &set, &refs)[0] {
            Segment::Stack(s) => assert_eq!(s.layers[0].url, "https://native/Kappa"),
            _ => panic!(),
        }
        // and resolves a name the set doesn't have, with modifiers
        let refs = EmoteSet::from_list([native("SubEmote", "twitch")]);
        match &segments_with("w! SubEmote ffzX", &set, &refs)[0] {
            Segment::Stack(s) => {
                assert_eq!(s.layers[0].url, "https://native/SubEmote");
                assert_eq!(s.layers[0].fx, vec![Effect::FlipX, Effect::Wide]);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn keys_are_unique_per_effect_and_roundtrip() {
        let set = cast();
        let mut keys = std::collections::HashSet::new();
        for (name, _) in TOKENS.iter().filter(|(k, _)| *k != "z!") {
            let msg = if name.ends_with('!') {
                format!("{name} KEKW")
            } else {
                format!("KEKW {name}")
            };
            match &segments(&msg, &set)[0] {
                Segment::Stack(s) => keys.insert(s.key()),
                _ => panic!("{msg}"),
            };
        }
        // 24 words alias into 17 effects (x!/h!/ffzX, y!/v!/ffzY, ffzW/ffzWide/w!)
        assert_eq!(keys.len(), 17);
        assert_eq!(CODES.len(), 17);
        let codes: std::collections::HashSet<_> = CODES.iter().map(|c| c.1).collect();
        assert_eq!(codes.len(), 17, "stack-key codes are unique");
    }

    #[test]
    fn key_roundtrips_through_split_key() {
        let set = cast();
        let s = match &segments("w!GAMBA notL ffzCursed c!#0f0", &set)[0] {
            Segment::Stack(s) => s.clone(),
            _ => panic!(),
        };
        assert_eq!(
            s.key(),
            "https://cdn/GAMBA.webp\n#w\nhttps://cdn/notL.webp\n#c@120"
        );
        assert_eq!(split_key(&s.key()), s.layers);
        // plain stacks keep the old shape: urls only
        let plain = match &segments("GAMBA notL", &set)[0] {
            Segment::Stack(s) => s.key(),
            _ => panic!(),
        };
        assert_eq!(plain, "https://cdn/GAMBA.webp\nhttps://cdn/notL.webp");
        assert_eq!(
            split_key("https://x/a\n#xw@7")[0].fx,
            vec![Effect::FlipX, Effect::Wide]
        );
        assert_eq!(split_key("https://x/a\n#xw@7")[0].hue, Some(7));
        // a stray effect line with no layer is ignored, not a panic
        assert!(split_key("#xw").is_empty());
    }

    #[test]
    fn twitch_native_positions_name_the_covered_text() {
        let u =
            |id: &str| format!("https://static-cdn.jtvnw.net/emoticons/v2/{id}/default/dark/2.0");
        for (tag, text, want) in [
            ("25:0-4", "Kappa hi", vec![("Kappa", "25")]),
            ("25:0-4,12-16", "Kappa hi Kappa", vec![("Kappa", "25")]),
            (
                "25:0-4/1902:6-10",
                "Kappa Keepo",
                vec![("Kappa", "25"), ("Keepo", "1902")],
            ),
            (
                "emotesv2_ab12:3-5",
                "hi LUL",
                vec![("LUL", "emotesv2_ab12")],
            ),
            // positions count code points: each emoji is ONE position
            ("25:3-7", "\u{1f600}\u{1f600} Kappa", vec![("Kappa", "25")]),
            ("25:0-4/", "Kappa hi", vec![("Kappa", "25")]),
            ("", "Kappa", vec![]),
        ] {
            let got: Vec<(String, String)> = twitch_native(tag, text)
                .into_iter()
                .map(|e| (e.name, e.url))
                .collect();
            let want: Vec<(String, String)> =
                want.into_iter().map(|(n, i)| (n.into(), u(i))).collect();
            assert_eq!(got, want, "{tag} / {text}");
        }
    }

    #[test]
    fn twitch_native_rejects_bad_input() {
        for (tag, text) in [
            ("25:0-9", "Kappa"),     // past the end
            ("25:4-0", "Kappa"),     // reversed
            ("25:a-b", "Kappa"),     // not numbers
            ("25", "Kappa"),         // no positions
            ("25?x=1:0-4", "Kappa"), // query-ish id
            ("25:0-6", "Kappa hi"),  // crosses a space
        ] {
            let got = twitch_native(tag, text);
            assert!(got.is_empty(), "{tag}: {got:?}");
        }
    }

    #[test]
    fn kick_native_tokens_become_names_and_refs() {
        let (text, refs) = kick_native("hi [emote:37226:KEKW] there");
        assert_eq!(text, "hi KEKW there");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].name, "KEKW");
        assert_eq!(refs[0].url, "https://files.kick.com/emotes/37226/fullsize");
        assert_eq!(refs[0].provider, "kick");
        // glued neighbors get spaced so each is its own word
        assert_eq!(kick_native("a[emote:1:X]b").0, "a X b");
        assert_eq!(kick_native("[emote:1:X][emote:2:Y]").0, "X Y");
        assert_eq!(kick_native("[emote:1:X][emote:2:Y]").1.len(), 2);
        // repeats dedupe to one ref
        assert_eq!(kick_native("[emote:1:X] [emote:1:X]").1.len(), 1);
        // malformed tokens stay as typed
        for bad in [
            "[emote:abc:X]",
            "[emote::X]",
            "[emote:1:]",
            "[emote:1:X Y]",
            "[emote:1:X",
            "[emote:../x:X]",
            "[emote:12]",
        ] {
            let (t, r) = kick_native(bad);
            assert_eq!(t, bad, "{bad}");
            assert!(r.is_empty(), "{bad}");
        }
        assert_eq!(kick_native("plain").0, "plain");
    }

    #[test]
    fn msg_ref_guards_the_trust_boundary() {
        assert!(msg_ref("KEKW", "https://cdn/k.webp", "7tv", false).is_some());
        for (n, u) in [
            ("", "https://cdn/k"),
            ("a b", "https://cdn/k"),
            ("KEKW", "http://cdn/k"),
            ("KEKW", "javascript:x"),
            ("KEKW", "https://cdn/k k"),
            ("KEKW", "https://cdn/k\u{1b}[2J"),
        ] {
            assert!(msg_ref(n, u, "x", false).is_none(), "{n} {u}");
        }
        // control chars stripped from the name
        assert_eq!(
            msg_ref("KE\u{1b}KW", "https://cdn/k", "x", false)
                .unwrap()
                .name,
            "KEKW"
        );
    }

    #[test]
    fn native_refs_drive_a_full_parse() {
        let set = cast();
        let (text, refs) = kick_native("[emote:9:SubOnly] w! KEKW");
        let r = ref_set(&refs);
        let segs = segments_with(&text, &set, &r);
        let bases: Vec<&str> = segs
            .iter()
            .filter_map(|s| match s {
                Segment::Stack(st) => Some(st.base.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(bases, vec!["SubOnly", "KEKW"]);
        match &segs[0] {
            Segment::Stack(st) => {
                assert_eq!(st.layers[0].url, "https://files.kick.com/emotes/9/fullsize")
            }
            _ => panic!(),
        }
    }

    #[test]
    fn parses_real_api_json() {
        let json = r#"{"channel":"xqc","emotes":[
            {"name":"GAMBA","url":"https://cdn.7tv.app/emote/x/1x.webp","provider":"7tv","id":"x","animated":true}
        ],"count":1}"#;
        let resp: EmoteSetResponse = serde_json::from_str(json).unwrap();
        let set = EmoteSet::from_list(resp.emotes);
        assert!(set.get("GAMBA").unwrap().animated);
    }

    #[test]
    fn owner_emotes_are_lenient_per_row_and_https_only() {
        let v = serde_json::json!({"emotes": [
            {"name": "A", "url": "https://cdn.heatsync.org/a.webp", "provider": "upload", "hash": "h1"},
            {"name": "B", "url": "/emotes/b.webp"},
            {"name": "C", "url": "http://x/c.webp"},
            {"name": "D", "url": "//evil/d.webp"},
            {"url": "https://x/e.webp"},
            {"name": "F", "url": ""},
            "junk"]});
        let got = owner_list(&v, "https://heatsync.org");
        let names: Vec<_> = got.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["A", "B"]);
        assert_eq!(got[0].id, "h1");
        assert_eq!(got[1].url, "https://heatsync.org/emotes/b.webp");
        assert_eq!(got[1].provider, "upload");
        assert!(owner_list(&serde_json::json!({}), "x").is_empty());
        assert!(owner_list(&serde_json::json!({"emotes": 5}), "x").is_empty());
    }

    #[test]
    fn owner_emotes_listed_last_win_a_name() {
        let chan = vec![e("W")];
        let mut mine = e("W");
        mine.provider = "upload".into();
        mine.url = "https://heatsync.org/w.webp".into();
        let set = EmoteSet::from_list(chan.into_iter().chain([mine]));
        assert_eq!(set.get("W").unwrap().provider, "upload");
    }

    #[test]
    fn a_blocked_emote_vanishes_but_its_same_named_twin_stays() {
        let mut mine = e("BlockTwin");
        mine.url = "https://block.test/seg/mine.webp".into();
        let mut other = e("BlockTwin");
        other.url = "https://block.test/seg/other.webp".into();
        let set_a = EmoteSet::from_list([mine.clone()]);
        let set_b = EmoteSet::from_list([other.clone()]);
        let text = |s: &[Segment]| -> String {
            s.iter()
                .map(|g| match g {
                    Segment::Text(t) => t.clone(),
                    Segment::Stack(_) => "<emote>".into(),
                })
                .collect::<Vec<_>>()
                .join("|")
        };
        assert_eq!(text(&segments("hi BlockTwin yo", &set_a)), "hi|<emote>|yo");
        assert!(crate::block::block(&mine));
        assert_eq!(
            text(&segments("hi BlockTwin yo", &set_a)),
            "hi yo",
            "hidden"
        );
        assert_eq!(
            text(&segments("hi BlockTwin yo", &set_b)),
            "hi|<emote>|yo",
            "the other emote with that name is untouched"
        );
        // its pending modifiers go with it instead of turning into stray text
        assert_eq!(text(&segments("w! BlockTwin", &set_a)), "");
        // aimable while hidden
        assert_eq!(
            emotes_in("a BlockTwin BlockTwin", &set_a, EmoteSet::none()).len(),
            1
        );
        crate::block::unblock_key(&crate::block::key(&mine).unwrap());
        assert_eq!(text(&segments("BlockTwin", &set_a)), "<emote>");
    }
}
