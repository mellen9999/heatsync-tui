//! who sees a tile's picture if you send it from this room — the one answer
//! behind a tile's corner mark and whether the tile is offered at all.
//!
//! a port of the site's `shared/emote-mark.ts` (MARK_VERSION 1). the contract is
//! `core/tests/emote-mark.vectors.json`, vendored byte for byte from the site;
//! `core/tests/emote_mark_vectors.rs` runs every case against this file.
//!
//!   mark    none = everyone · T K Y = that platform's viewers · 7 B F = that
//!           addon's users · H = heatsync users
//!   offer   a room answers a typed word with ONE picture: every other picture
//!           under that name is not a tile
//!
//! the mark describes what SENDING the tile from this room ships, as the law's
//! send answers it: a click pins the tile's own picture, so your copy of the
//! room's own 7tv picture goes out as the room's (7) and a different picture
//! under the room's name is no tile at all.
//!
//! pure: no clock, no io, no state. it reads the law's own send context.

use crate::law::{send_word, Pic, Room, SendCtx, SendFrom};

pub const MARK_VERSION: u32 = 1;

/// the corner letter. `Plain` = everyone sees it, so there is no letter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    Plain,
    T,
    K,
    Y,
    SevenTv,
    Bttv,
    Ffz,
    H,
}

impl Mark {
    pub fn letter(self) -> Option<char> {
        match self {
            Mark::Plain => None,
            Mark::T => Some('T'),
            Mark::K => Some('K'),
            Mark::Y => Some('Y'),
            Mark::SevenTv => Some('7'),
            Mark::Bttv => Some('B'),
            Mark::Ffz => Some('F'),
            Mark::H => Some('H'),
        }
    }

    /// the letter as text; "" for no mark.
    pub fn as_str(self) -> &'static str {
        match self {
            Mark::Plain => "",
            Mark::T => "T",
            Mark::K => "K",
            Mark::Y => "Y",
            Mark::SevenTv => "7",
            Mark::Bttv => "B",
            Mark::Ffz => "F",
            Mark::H => "H",
        }
    }
}

/// everyone: a platform's own emote, emoji, chatters · ext: an addon's users ·
/// heatsync: heatsync users only
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Everyone,
    Ext,
    Heatsync,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Everyone => "everyone",
            Kind::Ext => "ext",
            Kind::Heatsync => "heatsync",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Audience {
    pub kind: Kind,
    pub mark: Mark,
    /// whose picture it goes out as: the provider the words name ("all twitch",
    /// "7tv users"); `hs` = heatsync users only
    pub as_: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TileSays {
    /// false = this room answers the tile's word with another picture: the tile
    /// is not drawn, in a picker or in Tab
    pub offered: bool,
    /// where what it ships sits (room, own, global, catalog, emoji, people): the
    /// answer's tier, the tile's own when its word has no answer
    pub tier: String,
    pub kind: Kind,
    pub mark: Mark,
    pub as_: String,
}

impl TileSays {
    fn of(offered: bool, tier: &str, a: Audience) -> TileSays {
        TileSays {
            offered,
            tier: tier.to_string(),
            kind: a.kind,
            mark: a.mark,
            as_: a.as_,
        }
    }
}

/// a client's own names for our uploads and your saved copies
fn canon(provider: &str) -> &str {
    match provider {
        "heatsync" | "inventory" => "hs",
        p => p,
    }
}

fn silent(p: &str) -> bool {
    p == "emoji" || p == "chatter"
}

fn native(p: &str) -> Option<Mark> {
    match p {
        "twitch" => Some(Mark::T),
        "kick" => Some(Mark::K),
        "youtube" => Some(Mark::Y),
        _ => None,
    }
}

fn addon(p: &str) -> Option<Mark> {
    match p {
        "7tv" => Some(Mark::SevenTv),
        "bttv" => Some(Mark::Bttv),
        "ffz" => Some(Mark::Ffz),
        _ => None,
    }
}

fn heatsync() -> Audience {
    Audience {
        kind: Kind::Heatsync,
        mark: Mark::H,
        as_: "hs".into(),
    }
}

/// who sees a picture of `provider` that sits in `tier` (where the law's offer
/// found it), with no room to answer for it. a tile asks [`tile_says`], which
/// first finds what the room would really send.
///   emoji, chatters           everyone, no mark
///   not loaded here           heatsync users only (a search answer, a favorite from another room)
///   a platform's own emote    that platform's viewers
///   yours, or a heatsync one  heatsync users only
///   an addon's emote          that addon's users (an unknown provider: an addon's, with no letter)
pub fn audience(provider: &str, tier: Option<&str>) -> Audience {
    let p = canon(provider);
    if silent(p) {
        return Audience {
            kind: Kind::Everyone,
            mark: Mark::Plain,
            as_: p.to_string(),
        };
    }
    if tier == Some("catalog") {
        return heatsync();
    }
    if let Some(mark) = native(p) {
        return Audience {
            kind: Kind::Everyone,
            mark,
            as_: p.to_string(),
        };
    }
    if tier == Some("own") || p == "hs" {
        return heatsync();
    }
    Audience {
        kind: Kind::Ext,
        mark: addon(p).unwrap_or(Mark::Plain),
        as_: if p.is_empty() { "ext" } else { p }.to_string(),
    }
}

/// one picture: the same `pic` (default `provider:id`), or the same upload by its hash
pub fn same_picture<E: Pic>(a: &E, b: &E) -> bool {
    a.pic() == b.pic()
        || a.hash()
            .is_some_and(|h| !h.is_empty() && Some(h) == b.hash())
}

/// where the answer to a typed word sits, as a tier. a platform's own emote
/// you may use (native) and this draft's pick sit where the tile does, except
/// a tile this room does not hold (catalog: a favorite from elsewhere, a search
/// answer): the room answers it from your set, so it is no longer "not loaded
/// here".
fn sits(from: SendFrom, tier: &str) -> &str {
    match from {
        SendFrom::Room => "room",
        SendFrom::Global => "global",
        SendFrom::Own | SendFrom::Personal => "own",
        SendFrom::Native if tier == "catalog" => "own",
        SendFrom::Native | SendFrom::Pick => tier,
    }
}

/// what a tile says. `ctx` is the law's send context for the box the line is
/// written in (its room, every leg, your sets, the globals; picks are never
/// read: a tile is judged by what its word sends). `tier` is where the law's
/// offer found the tile.
///   offered   no room, or the room's answer for the word is this very picture
///             (or the word has no answer)
///   mark      that answer's class, never the copy the tile happens to be: your
///             copy of the room's own picture is the room's; an answer from your
///             own set ships as yours (H)
///   a platform's own emote is drawn only on that platform: where the send
///   reaches none of its legs it travels as a picture and only heatsync users
///   see it
pub fn tile_says<E: Pic>(ctx: &SendCtx<E>, e: &E, tier: &str) -> TileSays {
    if silent(canon(e.provider())) {
        return TileSays::of(true, tier, audience(e.provider(), None));
    }
    let one: [&Room<E>; 1];
    let legs: &[&Room<E>] = match ctx.room {
        _ if !ctx.legs.is_empty() => ctx.legs,
        Some(room) => {
            one = [room];
            &one
        }
        None => &[],
    };
    let blind = SendCtx {
        picks: None,
        ..ctx.clone()
    };
    let hit = ctx
        .room
        .filter(|_| !e.name().is_empty())
        .and_then(|_| send_word(e.name(), &blind));
    let same = hit.as_ref().is_some_and(|h| same_picture(h.emote, e));
    let (mut says, at) = match &hit {
        Some(h) if same => {
            let at = sits(h.from, tier);
            let says = if matches!(h.from, SendFrom::Own | SendFrom::Personal) {
                heatsync()
            } else {
                audience(h.emote.provider(), Some(at))
            };
            (says, at)
        }
        _ => (audience(e.provider(), Some(tier)), tier),
    };
    if says.kind == Kind::Everyone
        && native(&says.as_).is_some()
        && !legs.is_empty()
        && !legs.iter().any(|r| r.platform.as_str() == says.as_)
    {
        says = heatsync();
    }
    TileSays::of(hit.is_none() || same, at, says)
}
