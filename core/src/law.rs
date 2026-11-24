//! the emote law — the one answer to "which emote is this word".
//!
//! a port of the site's `shared/emote-law.ts` (LAW_VERSION 1). the contract is
//! `core/tests/emote-law.vectors.json`, vendored byte for byte from the site;
//! `core/tests/emote_law_vectors.rs` runs every case against this file.
//!
//! three jobs, all pure (no io, no clock, no state):
//!   read   what a word draws on a line           read_word / read_line
//!   send   what a hand-typed word ships           send_word / send_line / edit_line
//!   offer  what tab and completion list           offer / extend_offer / browse
//!
//! an emote is its picture (`provider:id`); a name is only a label. every
//! builder sorts before it indexes and every tie ends in code point order, so
//! the answer never depends on load order or hash-map order. a viewer's own
//! emotes are not an input to reading someone else's line: there is no
//! parameter to pass them in.
//!
//! offsets are chars (code points), the unit the vectors use. a line's
//! length for the kick cap and a name's length for ranking are utf-16 units,
//! because that is the unit the law counts in.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};

pub const LAW_VERSION: u32 = 1;

/// what the law needs to know about one emote. the picture is `provider:id`.
pub trait Pic {
    fn provider(&self) -> &str;
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn zero_width(&self) -> bool {
        false
    }
    /// channels carrying it (catalog hits only)
    fn pop(&self) -> u64 {
        0
    }
}

/// `provider:id` — the picture
pub fn key_of<E: Pic>(e: &E) -> String {
    format!("{}:{}", e.provider(), e.id())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Platform {
    Twitch,
    Kick,
    Youtube,
}

impl Platform {
    pub fn parse(s: &str) -> Option<Platform> {
        match s {
            "twitch" => Some(Platform::Twitch),
            "kick" => Some(Platform::Kick),
            "youtube" => Some(Platform::Youtube),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Platform::Twitch => "twitch",
            Platform::Kick => "kick",
            Platform::Youtube => "youtube",
        }
    }
}

/// exact name -> the one emote that name means in this set
pub type Index<E> = HashMap<String, E>;

/// a line's room: the row's own platform and channel, never the open tab
#[derive(Clone, Debug)]
pub struct Room<E> {
    pub platform: Platform,
    pub channel: String,
    /// false while the set is loading: the row draws without it and repaints, it never borrows another room
    pub loaded: bool,
    /// owner's heatsync set, then ffz, 7tv, bttv, then the platform's channel emotes — one emote per name
    pub names: Index<E>,
}

#[derive(Clone, Debug)]
pub struct Own<E> {
    /// heatsync inventory
    pub inventory: Index<E>,
    /// 7tv personal set
    pub personal: Index<E>,
    /// platform emotes this person can use natively there
    pub native: HashMap<Platform, Index<E>>,
}

impl<E> Default for Own<E> {
    fn default() -> Self {
        Own {
            inventory: HashMap::new(),
            personal: HashMap::new(),
            native: HashMap::new(),
        }
    }
}

/// a withdrawn pick (take-down) reads as text, never as a look-alike
pub enum Ref<'a, E> {
    Pic(&'a E),
    Gone,
}
impl<E> Clone for Ref<'_, E> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<E> Copy for Ref<'_, E> {}

// ── order ────────────────────────────────────────────────────────────────────

/// code point order. rust's str order is bytewise utf-8, which is code point order.
pub fn cmp_cp(a: &str, b: &str) -> Ordering {
    a.cmp(b)
}

const CHANNEL_ORDER: [&str; 3] = ["ffz", "7tv", "bttv"];
const GLOBAL_ORDER: [&str; 5] = ["7tv", "bttv", "ffz", "twitch", "hs"];

fn rank_in(order: &[&str], p: &str) -> usize {
    order.iter().position(|o| *o == p).unwrap_or(order.len())
}

/// one emote per exact name. two of one name in the same list: the lower id wins, whatever order they arrived in.
pub fn build_index<E: Pic + Clone>(list: &[E], order: Option<&[&str]>) -> Index<E> {
    let mut sorted: Vec<&E> = list.iter().filter(|e| !e.name().is_empty()).collect();
    sorted.sort_by(|a, b| {
        let by_provider = match order {
            Some(o) => {
                let (ra, rb) = (rank_in(o, a.provider()), rank_in(o, b.provider()));
                ra.cmp(&rb).then_with(|| {
                    if ra == o.len() {
                        cmp_cp(a.provider(), b.provider())
                    } else {
                        Ordering::Equal
                    }
                })
            }
            None => Ordering::Equal,
        };
        by_provider
            .then_with(|| cmp_cp(a.name(), b.name()))
            .then_with(|| cmp_cp(a.id(), b.id()))
    });
    let mut out = HashMap::with_capacity(sorted.len());
    for e in sorted {
        out.entry(e.name().to_string()).or_insert_with(|| e.clone());
    }
    out
}

#[derive(Default)]
pub struct RoomLists<'a, E> {
    pub owner: &'a [E],
    pub third: &'a [E],
    pub native: &'a [E],
}

pub fn build_room<E: Pic + Clone>(
    platform: Platform,
    channel: &str,
    lists: RoomLists<E>,
    loaded: bool,
) -> Room<E> {
    let mut names: Index<E> = HashMap::new();
    for idx in [
        build_index(lists.owner, None),
        build_index(lists.third, Some(&CHANNEL_ORDER)),
        build_index(lists.native, None),
    ] {
        for (n, e) in idx {
            names.entry(n).or_insert(e);
        }
    }
    Room {
        platform,
        channel: channel.to_string(),
        loaded,
        names,
    }
}

pub fn build_globals<E: Pic + Clone>(list: &[E]) -> Index<E> {
    build_index(list, Some(&GLOBAL_ORDER))
}

pub fn build_own<E: Pic + Clone>(
    inventory: &[E],
    personal: &[E],
    native: &[(Platform, Vec<E>)],
) -> Own<E> {
    let mut n = HashMap::new();
    for (p, list) in native {
        if !list.is_empty() {
            n.insert(*p, build_index(list, None));
        }
    }
    Own {
        inventory: build_index(inventory, None),
        personal: build_index(personal, None),
        native: n,
    }
}

// ── lookup ───────────────────────────────────────────────────────────────────

/// lowercase twin whose spelling sorts first; only consulted after an exact-case miss in every tier
fn fold_get<'a, E>(idx: &'a Index<E>, word: &str) -> Option<&'a E> {
    let l = word.to_lowercase();
    idx.iter()
        .filter(|(n, _)| n.to_lowercase() == l)
        .min_by(|a, b| cmp_cp(a.0, b.0))
        .map(|(_, e)| e)
}

fn pick<'a, E>(idx: Option<&'a Index<E>>, word: &str, fold: bool) -> Option<&'a E> {
    let idx = idx?;
    if idx.is_empty() {
        return None;
    }
    if fold {
        fold_get(idx, word)
    } else {
        idx.get(word)
    }
}

fn ref_for<'a, E>(
    refs: Option<&HashMap<String, Ref<'a, E>>>,
    word: &str,
    fold: bool,
) -> Option<Ref<'a, E>> {
    let refs = refs?;
    if !fold {
        return refs.get(word).copied();
    }
    let l = word.to_lowercase();
    refs.iter()
        .filter(|(k, _)| k.to_lowercase() == l)
        .min_by(|a, b| cmp_cp(a.0, b.0))
        .map(|(_, r)| *r)
}

/// exact literal, then the overlay reading (strip one trailing 0), then — only with the case setting on — both again folded
fn passes<T>(
    word: &str,
    fold: bool,
    mut chain: impl FnMut(&str, bool, bool) -> Option<T>,
) -> Option<(T, bool, String)> {
    let base = if word.chars().count() > 1 && word.ends_with('0') {
        Some(&word[..word.len() - 1])
    } else {
        None
    };
    let rounds: &[bool] = if fold { &[false, true] } else { &[false] };
    for &f in rounds {
        if let Some(a) = chain(word, f, false) {
            return Some((a, false, word.to_string()));
        }
        if let Some(b) = base {
            if let Some(h) = chain(b, f, true) {
                return Some((h, true, b.to_string()));
            }
        }
    }
    None
}

// ── words ────────────────────────────────────────────────────────────────────

/// javascript's `\s`: the law splits on it, so the port must too
pub fn is_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

/// (start, end) char ranges of every run of non-space in `chars[from..to]`
fn word_spans(chars: &[char], from: usize, to: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = from;
    while i < to {
        while i < to && is_space(chars[i]) {
            i += 1;
        }
        let s = i;
        while i < to && !is_space(chars[i]) {
            i += 1;
        }
        if i > s {
            out.push((s, i));
        }
    }
    out
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

// ── read ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum From {
    Token,
    Pick,
    Personal,
    Room,
    Sender,
    Global,
}

impl From {
    pub fn as_str(self) -> &'static str {
        match self {
            From::Token => "token",
            From::Pick => "pick",
            From::Personal => "personal",
            From::Room => "room",
            From::Sender => "sender",
            From::Global => "global",
        }
    }
}

pub struct ReadCtx<'a, 'r, E> {
    /// the row's own room; None on roomless surfaces (dms, posts, replies)
    pub room: Option<&'a Room<E>>,
    /// what the sender's app shipped with the line
    pub refs: Option<&'r HashMap<String, Ref<'a, E>>>,
    /// the sender's sets (the viewer's own only when the viewer is the sender)
    pub sender: Option<&'a Own<E>>,
    pub globals: &'a Index<E>,
    /// the viewer's case-insensitive setting
    pub fold: bool,
}
impl<E> Clone for ReadCtx<'_, '_, E> {
    fn clone(&self) -> Self {
        ReadCtx {
            room: self.room,
            refs: self.refs,
            sender: self.sender,
            globals: self.globals,
            fold: self.fold,
        }
    }
}

#[derive(Clone, Copy)]
pub struct WordHit<'a, E> {
    /// None = a withdrawn pick: draw the word as text
    pub emote: Option<&'a E>,
    pub from: From,
    pub overlay: bool,
}

/// what one exact spelling means: the line's own pick, the sender's personal
/// set, the room, the sender's inventory, then a global. `Some((None, _))` is a
/// withdrawn pick. the overlay reading is [`read_word`]'s, which asks this once
/// for the word and once for the word minus a trailing 0.
pub fn read_exact<'a, E: Pic>(
    word: &str,
    ctx: &ReadCtx<'a, '_, E>,
    fold: bool,
) -> Option<(Option<&'a E>, From)> {
    if let Some(r) = ref_for(ctx.refs, word, fold) {
        return Some(match r {
            Ref::Gone => (None, From::Pick),
            Ref::Pic(e) => (Some(e), From::Pick),
        });
    }
    if let Some(e) = pick(ctx.sender.map(|s| &s.personal), word, fold) {
        return Some((Some(e), From::Personal));
    }
    if let Some(e) = pick(ctx.room.map(|r| &r.names), word, fold) {
        return Some((Some(e), From::Room));
    }
    if let Some(e) = pick(ctx.sender.map(|s| &s.inventory), word, fold) {
        return Some((Some(e), From::Sender));
    }
    pick(Some(ctx.globals), word, fold).map(|e| (Some(e), From::Global))
}

/// what one word draws. None = not an emote.
pub fn read_word<'a, E: Pic>(word: &str, ctx: &ReadCtx<'a, '_, E>) -> Option<WordHit<'a, E>> {
    let ((emote, from), stripped, _) = passes(word, ctx.fold, |w, f, _| read_exact(w, ctx, f))?;
    Some(WordHit {
        emote,
        from,
        overlay: emote.is_some_and(|e| stripped || e.zero_width()),
    })
}

/// a platform-marked span (twitch tag range, kick token, youtube emoji). char offsets, end exclusive.
pub struct Token<'a, E> {
    pub start: usize,
    pub end: usize,
    pub emote: &'a E,
}
impl<E> Clone for Token<'_, E> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<E> Copy for Token<'_, E> {}

#[derive(Clone)]
pub struct Drawn<'a, E> {
    pub start: usize,
    pub end: usize,
    pub word: String,
    pub emote: Option<&'a E>,
    pub from: From,
    pub overlay: bool,
}

/// the pen remembers where the last word ended and whether it drew an emote:
/// an overlay stacks only on an emote directly before it
struct Pen<'a, E> {
    out: Vec<Drawn<'a, E>>,
    prev_end: usize,
    prev_emote: bool,
}

impl<'a, E> Pen<'a, E> {
    fn push(&mut self, chars: &[char], mut d: Drawn<'a, E>) {
        if d.overlay
            && !(self.prev_emote && chars[self.prev_end..d.start].iter().all(|c| is_space(*c)))
        {
            d.overlay = false;
        }
        self.prev_end = d.end;
        self.prev_emote = d.emote.is_some();
        self.out.push(d);
    }
    fn miss(&mut self, end: usize) {
        self.prev_emote = false;
        self.prev_end = end;
    }
}

/// every emote on a line, in order. a platform token owns its span — nothing with
/// the same name replaces it and its text is never read again. an overlay only
/// stacks on an emote directly before it; alone it draws inline.
pub fn read_line<'a, E: Pic>(
    text: &str,
    tokens: &[Token<'a, E>],
    ctx: &ReadCtx<'a, '_, E>,
) -> Vec<Drawn<'a, E>> {
    let chars: Vec<char> = text.chars().collect();
    let mut spans: Vec<Token<E>> = tokens
        .iter()
        .filter(|t| t.end > t.start && t.end <= chars.len())
        .copied()
        .collect();
    spans.sort_by_key(|t| (t.start, t.end));
    let mut pen = Pen {
        out: Vec::new(),
        prev_end: 0,
        prev_emote: false,
    };
    let words = |pen: &mut Pen<'a, E>, from: usize, to: usize| {
        for (s, e) in word_spans(&chars, from, to) {
            let w: String = chars[s..e].iter().collect();
            match read_word(&w, ctx) {
                Some(h) => pen.push(
                    &chars,
                    Drawn {
                        start: s,
                        end: e,
                        word: w,
                        emote: h.emote,
                        from: h.from,
                        overlay: h.overlay,
                    },
                ),
                None => pen.miss(e),
            }
        }
    };
    let mut at = 0usize;
    for t in spans {
        if t.start < at {
            continue;
        }
        words(&mut pen, at, t.start);
        let word: String = chars[t.start..t.end].iter().collect();
        pen.push(
            &chars,
            Drawn {
                start: t.start,
                end: t.end,
                word,
                emote: Some(t.emote),
                from: From::Token,
                overlay: false,
            },
        );
        at = t.end;
    }
    words(&mut pen, at, chars.len());
    pen.out
}

// ── send ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendFrom {
    Pick,
    Native,
    Personal,
    Room,
    Own,
    Global,
}

impl SendFrom {
    pub fn as_str(self) -> &'static str {
        match self {
            SendFrom::Pick => "pick",
            SendFrom::Native => "native",
            SendFrom::Personal => "personal",
            SendFrom::Room => "room",
            SendFrom::Own => "own",
            SendFrom::Global => "global",
        }
    }
}

pub struct SendCtx<'a, E> {
    /// the room the line goes to; for a fan-out, the first leg (twitch, else kick, else youtube). None = roomless.
    pub room: Option<&'a Room<E>>,
    /// every leg of a fan-out, for the per-platform wire text
    pub legs: &'a [&'a Room<E>],
    pub own: &'a Own<E>,
    pub globals: &'a Index<E>,
    /// picks made in this draft only (tab, picker, chip), one per name. gone once sent.
    pub picks: Option<&'a Index<E>>,
    pub fold: bool,
}
impl<E> Clone for SendCtx<'_, E> {
    fn clone(&self) -> Self {
        SendCtx {
            room: self.room,
            legs: self.legs,
            own: self.own,
            globals: self.globals,
            picks: self.picks,
            fold: self.fold,
        }
    }
}

pub struct SendHit<'a, E> {
    pub emote: &'a E,
    pub from: SendFrom,
    pub overlay: bool,
    /// the name the ref is filed under: the word as typed, minus an overlay 0
    pub key: String,
}

/// kick's chat cap, in utf-16 units
pub const KICK_MAX: usize = 500;

/// the platform marks this one itself: it needs no ref, and the echo predicts the token
fn platform_marks<E: Pic>(emote: &E, from: SendFrom, room: Option<&Room<E>>) -> bool {
    from == SendFrom::Native
        || (room.is_some_and(|r| r.platform == Platform::Kick)
            && emote.provider() == "kick"
            && from == SendFrom::Room)
}

/// what a hand-typed word ships: the platform's own emote where the platform
/// marks it by itself (twitch, youtube — a pick of a same-name copy could never
/// show there), this draft's pick, kick's own emote, your 7tv personal emote,
/// the room's, yours, a global.
pub fn send_word<'a, E: Pic>(word: &str, ctx: &SendCtx<'a, E>) -> Option<SendHit<'a, E>> {
    resolve(word, ctx, true)
}

fn resolve<'a, E: Pic>(word: &str, ctx: &SendCtx<'a, E>, native: bool) -> Option<SendHit<'a, E>> {
    let room = ctx.room;
    let auto = room.is_some_and(|r| r.platform != Platform::Kick);
    let ((emote, from), stripped, base) = passes(word, ctx.fold, |w, f, stripped| {
        // a platform marks its emote on the exact word only (a plain get, so never folded) and never on `name0`
        let nat = if native && !stripped {
            room.and_then(|r| ctx.own.native.get(&r.platform))
                .and_then(|i| i.get(w))
        } else {
            None
        };
        if let (Some(n), true) = (nat, auto) {
            return Some((n, SendFrom::Native));
        }
        if let Some(e) = pick(ctx.picks, w, f) {
            return Some((e, SendFrom::Pick));
        }
        if let Some(n) = nat {
            return Some((n, SendFrom::Native));
        }
        if let Some(e) = pick(Some(&ctx.own.personal), w, f) {
            return Some((e, SendFrom::Personal));
        }
        if let Some(e) = pick(room.map(|r| &r.names), w, f) {
            return Some((e, SendFrom::Room));
        }
        if let Some(e) = pick(Some(&ctx.own.inventory), w, f) {
            return Some((e, SendFrom::Own));
        }
        pick(Some(ctx.globals), w, f).map(|e| (e, SendFrom::Global))
    })?;
    Some(SendHit {
        emote,
        from,
        overlay: stripped || emote.zero_width(),
        key: base,
    })
}

pub struct Shipped<'a, E> {
    /// what travels with the line, filed by name
    pub refs: HashMap<String, Ref<'a, E>>,
    /// spans the platform will mark itself — the echo draws them before the platform's answer arrives
    pub tokens: Vec<Token<'a, E>>,
    /// the text each platform receives
    pub wire: BTreeMap<Platform, String>,
    /// what the composer shows; equal to what your own row draws
    pub chips: Vec<Drawn<'a, E>>,
}

/// replace every run of non-space with `f(run)`, keeping the spaces as they were
fn map_words(text: &str, mut f: impl FnMut(&str) -> String) -> String {
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    for c in text.chars() {
        if is_space(c) {
            if !word.is_empty() {
                out.push_str(&f(&word));
                word.clear();
            }
            out.push(c);
        } else {
            word.push(c);
        }
    }
    if !word.is_empty() {
        out.push_str(&f(&word));
    }
    out
}

/// kick only draws its emotes from `[emote:id:name]`; a line that would not fit
/// with them goes out as plain words. on a leg that is not the line's own room,
/// a word the line already answered stays a bare word — its ref carries the one
/// meaning — and only a word that meant nothing there can be kick's.
fn kick_wire<E: Pic>(text: &str, leg: &Room<E>, ctx: &SendCtx<E>) -> String {
    let mut leg_ctx = ctx.clone();
    leg_ctx.room = Some(leg);
    let side = !ctx.room.is_some_and(|r| std::ptr::eq(r, leg));
    let out = map_words(text, |w| {
        if side && resolve(w, ctx, true).is_some() {
            return w.to_string();
        }
        match resolve(w, &leg_ctx, true) {
            Some(h) if platform_marks(h.emote, h.from, Some(leg)) && !h.overlay => {
                format!("[emote:{}:{}]", h.emote.id(), h.emote.name())
            }
            _ => w.to_string(),
        }
    });
    if utf16_len(&out) > KICK_MAX {
        text.to_string()
    } else {
        out
    }
}

/// chips for a draft: what the composer shows is what your own row draws
fn chips_of<'a, E: Pic>(
    text: &str,
    tokens: &[Token<'a, E>],
    refs: &HashMap<String, Ref<'a, E>>,
    ctx: &SendCtx<'a, E>,
) -> Vec<Drawn<'a, E>> {
    let rc = ReadCtx {
        room: ctx.room,
        refs: Some(refs),
        sender: Some(ctx.own),
        globals: ctx.globals,
        fold: ctx.fold,
    };
    read_line(text, tokens, &rc)
}

fn wire_of<E: Pic>(text: &str, ctx: &SendCtx<E>) -> BTreeMap<Platform, String> {
    let mut wire = BTreeMap::new();
    let legs: Vec<&Room<E>> = if !ctx.legs.is_empty() {
        ctx.legs.to_vec()
    } else {
        ctx.room.into_iter().collect()
    };
    for leg in legs {
        wire.entry(leg.platform).or_insert_with(|| {
            if leg.platform == Platform::Kick {
                kick_wire(text, leg, ctx)
            } else {
                text.to_string()
            }
        });
    }
    wire
}

/// one draft in, everything the send needs out. the chip, the shipped ref and your own row are this one answer.
pub fn send_line<'a, E: Pic>(text: &str, ctx: &SendCtx<'a, E>) -> Shipped<'a, E> {
    let wire = wire_of(text, ctx);
    // kick got plain words (nothing of its own in the line, or it would not fit): nothing is marked there, so nothing is kick's to answer
    let marked = !(ctx.room.is_some_and(|r| r.platform == Platform::Kick)
        && wire.get(&Platform::Kick).is_some_and(|w| w == text));
    let mut refs: HashMap<String, Ref<E>> = HashMap::new();
    let mut tokens = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    for (s, e) in word_spans(&chars, 0, chars.len()) {
        let w: String = chars[s..e].iter().collect();
        let Some(hit) = resolve(&w, ctx, marked) else {
            continue;
        };
        if marked && platform_marks(hit.emote, hit.from, ctx.room) && !hit.overlay {
            tokens.push(Token {
                start: s,
                end: e,
                emote: hit.emote,
            });
        } else {
            refs.entry(hit.key).or_insert(Ref::Pic(hit.emote));
        }
    }
    let chips = chips_of(text, &tokens, &refs, ctx);
    Shipped {
        refs,
        tokens,
        wire,
        chips,
    }
}

/// an edit keeps the picture of every word still in the text — stored refs
/// (take-downs included) win over a fresh resolve. only words new to the text
/// resolve, and refs for words that left are dropped.
pub fn edit_line<'a, E: Pic>(
    stored: Option<&HashMap<String, Ref<'a, E>>>,
    text: &str,
    ctx: &SendCtx<'a, E>,
) -> Shipped<'a, E> {
    let fresh = send_line(text, ctx);
    let Some(stored) = stored else { return fresh };
    let mut refs: HashMap<String, Ref<E>> = HashMap::new();
    let mut kept: HashSet<String> = HashSet::new();
    let chars: Vec<char> = text.chars().collect();
    for (s, e) in word_spans(&chars, 0, chars.len()) {
        let w: String = chars[s..e].iter().collect();
        // a pick made in this edit answers its word however the word reaches it (`name0`, the case setting)
        if ctx.picks.is_some_and(|p| !p.is_empty())
            && resolve(&w, ctx, true).is_some_and(|h| h.from == SendFrom::Pick)
        {
            continue;
        }
        let base = if w.chars().count() > 1 && w.ends_with('0') {
            Some(&w[..w.len() - 1])
        } else {
            None
        };
        let k = if stored.contains_key(&w) {
            Some(w.as_str())
        } else {
            base.filter(|b| stored.contains_key(*b))
        };
        if let Some(k) = k {
            refs.insert(k.to_string(), stored[k]);
            kept.insert(w.clone());
        }
    }
    for (k, v) in &fresh.refs {
        refs.entry(k.clone()).or_insert(*v);
    }
    let tokens: Vec<Token<E>> = fresh
        .tokens
        .iter()
        .filter(|t| !kept.contains(&chars[t.start..t.end].iter().collect::<String>()))
        .copied()
        .collect();
    let chips = chips_of(text, &tokens, &refs, ctx);
    Shipped {
        refs,
        tokens,
        wire: fresh.wire,
        chips,
    }
}

// ── offer ────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    Room,
    Own,
    Global,
    Catalog,
    Emoji,
}

impl Tier {
    /// the tiers a hand-typed word can send: an exact name leads the offer only from these
    pub fn local(self) -> bool {
        matches!(self, Tier::Room | Tier::Own | Tier::Global)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Room => "room",
            Tier::Own => "own",
            Tier::Global => "global",
            Tier::Catalog => "catalog",
            Tier::Emoji => "emoji",
        }
    }
    fn rank(self) -> u8 {
        self as u8
    }
}

pub const BUCKET_EXACT: u8 = 0;
pub const BUCKET_PREFIX: u8 = 1;
pub const BUCKET_BOUNDARY: u8 = 2;
pub const BUCKET_SUBSTRING: u8 = 3;
pub const BUCKET_NONE: u8 = 4;

/// 0 exact, 1 starts-with, 2 word-start, 3 inside, 4 none. `q` must already be lowercase.
/// a match is a word start where the char before it is not a letter, at a camel
/// hump (uppercase after lowercase), or where it runs to the end of the name.
pub fn match_bucket(name: &str, q: &str) -> u8 {
    let n = name.to_lowercase();
    if n == q {
        return BUCKET_EXACT;
    }
    if n.starts_with(q) {
        return BUCKET_PREFIX;
    }
    let nc: Vec<char> = n.chars().collect();
    let qc: Vec<char> = q.chars().collect();
    if qc.is_empty() || nc.len() < qc.len() {
        return BUCKET_NONE;
    }
    let oc: Vec<char> = name.chars().collect();
    // lowercasing can change a name's length (rare); the spelling is then read from the lowercase form
    let spelled = if oc.len() == nc.len() { &oc } else { &nc };
    let mut found = false;
    for pos in 0..=(nc.len() - qc.len()) {
        if nc[pos..pos + qc.len()] != qc[..] {
            continue;
        }
        found = true;
        let prev = if pos == 0 {
            None
        } else {
            Some(spelled[pos - 1])
        };
        let alpha = prev.is_some_and(|c| c.is_ascii_alphabetic());
        let hump =
            spelled[pos].is_ascii_uppercase() && prev.is_some_and(|c| c.is_ascii_lowercase());
        if !alpha || hump || pos + qc.len() == nc.len() {
            return BUCKET_BOUNDARY;
        }
    }
    if found {
        BUCKET_SUBSTRING
    } else {
        BUCKET_NONE
    }
}

/// channels carrying a name, by lowercase name; None = unknown
pub type PopFn<'a> = dyn Fn(&str) -> Option<f64> + 'a;

pub struct OfferCtx<'a, E> {
    pub room: Option<&'a Room<E>>,
    pub own: &'a Own<E>,
    pub globals: &'a Index<E>,
    /// emotes not loaded anywhere yet (server search hits)
    pub catalog: &'a [E],
    pub emoji: &'a [E],
    /// the person's own decayed usage of a name; a snapshot taken when the cycle starts
    pub usage: Option<&'a dyn Fn(&str) -> f64>,
    /// channels carrying a name, by lowercase name
    pub pop: Option<&'a PopFn<'a>>,
    pub blocked: Option<&'a dyn Fn(&str) -> bool>,
    /// false = the "suggest my emotes" setting is off
    pub suggest_own: bool,
    /// pictures the person pinned, in their order. whole records: a favorite goes where they go, loaded here or not
    pub favorites: &'a [E],
}
impl<E> Clone for OfferCtx<'_, E> {
    fn clone(&self) -> Self {
        OfferCtx {
            room: self.room,
            own: self.own,
            globals: self.globals,
            catalog: self.catalog,
            emoji: self.emoji,
            usage: self.usage,
            pop: self.pop,
            blocked: self.blocked,
            suggest_own: self.suggest_own,
            favorites: self.favorites,
        }
    }
}

pub struct Candidate<'a, E> {
    pub emote: &'a E,
    pub tier: Tier,
    /// 0 exact, 1 starts-with, 2 word-start, 3 inside, 4 no name match (catalog tag hit)
    pub matched: u8,
    /// a same-name copy that will not show where this is going, because the platform's own emote of that name wins there
    pub shadowed: bool,
}
impl<E> Clone for Candidate<'_, E> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<E> Copy for Candidate<'_, E> {}

struct Cand<'a, E> {
    emote: &'a E,
    key: String,
    tier: Tier,
    matched: u8,
    copy: u8,
    used: f64,
    pop: u64,
    len16: usize,
    /// place in the person's favorites
    pin: Option<usize>,
}

fn sorted_values<E: Pic>(idx: &Index<E>) -> Vec<&E> {
    let mut v: Vec<&E> = idx.values().collect();
    v.sort_by(|a, b| cmp_cp(a.name(), b.name()).then_with(|| cmp_cp(a.id(), b.id())));
    v
}

/// every reachable emote with its tier and send-order copy. `q` (lowercase) drops the ones that cannot match before they cost anything.
fn gather<'a, E: Pic>(ctx: &OfferCtx<'a, E>, q: Option<&str>) -> Vec<Cand<'a, E>> {
    let seen: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
    let names: RefCell<HashSet<&str>> = RefCell::new(HashSet::new());
    let mut out: Vec<Cand<E>> = Vec::new();
    let blocked = |k: &str| ctx.blocked.is_some_and(|f| f(k));
    let mut pins: HashMap<String, usize> = HashMap::new();
    for (i, e) in ctx.favorites.iter().enumerate() {
        if !e.name().is_empty() {
            pins.entry(key_of(e)).or_insert(i);
        }
    }
    // names a search answered with: its copy may match by tag, so it needs no name match
    let hits: RefCell<HashSet<&str>> = RefCell::new(HashSet::new());
    let add = |list: Vec<&'a E>, tier: Tier, copy: u8, local: bool, out: &mut Vec<Cand<'a, E>>| {
        for e in list {
            if e.name().is_empty() {
                continue;
            }
            let key = key_of(e);
            if !seen.borrow_mut().insert(key.clone()) {
                continue;
            }
            // a name you can reach here is held even when its emote is blocked: blocking hides, it never lets a search look-alike in
            if local {
                names.borrow_mut().insert(e.name());
            }
            if blocked(&key) {
                continue;
            }
            let matched = match q {
                Some(q) => match_bucket(e.name(), q),
                None => BUCKET_NONE,
            };
            // a favorite no search asked for needs its name to match, like anything loaded
            let hit = tier == Tier::Catalog && hits.borrow().contains(e.name());
            if q.is_some() && matched == BUCKET_NONE && !hit {
                continue;
            }
            let pin = pins.get(&key).copied();
            out.push(Cand {
                emote: e,
                key,
                tier,
                matched,
                copy,
                used: 0.0,
                pop: 0,
                len16: utf16_len(e.name()),
                pin,
            });
        }
    };
    // copy order = send order, so the copy a hand-typed word would ship is the one a name keeps first
    if let Some(idx) = ctx.room.and_then(|r| ctx.own.native.get(&r.platform)) {
        add(sorted_values(idx), Tier::Own, 0, true, &mut out);
    }
    add(
        sorted_values(&ctx.own.personal),
        Tier::Own,
        1,
        true,
        &mut out,
    );
    if let Some(r) = ctx.room {
        add(sorted_values(&r.names), Tier::Room, 2, true, &mut out);
    }
    if ctx.suggest_own {
        add(
            sorted_values(&ctx.own.inventory),
            Tier::Own,
            3,
            true,
            &mut out,
        );
    } else {
        // not suggested is not gone: typing the name still sends yours, so no search look-alike may take it
        for e in ctx.own.inventory.values() {
            if !e.name().is_empty() {
                names.borrow_mut().insert(e.name());
            }
        }
    }
    add(sorted_values(ctx.globals), Tier::Global, 4, true, &mut out);
    // one catalog copy per name (the most carried); a name already held locally keeps its local copy
    let mut best: HashMap<&str, &E> = HashMap::new();
    for e in ctx.catalog {
        if e.name().is_empty() || names.borrow().contains(e.name()) || blocked(&key_of(e)) {
            continue;
        }
        hits.borrow_mut().insert(e.name());
        match best.get(e.name()) {
            Some(p)
                if !(e.pop() > p.pop()
                    || (e.pop() == p.pop() && cmp_cp(e.id(), p.id()) == Ordering::Less)) => {}
            _ => {
                best.insert(e.name(), e);
            }
        }
    }
    // a favorite nothing here holds is offered like a search answer, and is the copy its name gets
    // (the first pinned, when two share a name). a name held here keeps its own: no look-alike
    let mut pinned_name: HashSet<&str> = HashSet::new();
    for e in ctx.favorites {
        let key = key_of(e);
        if e.name().is_empty()
            || seen.borrow().contains(&key)
            || names.borrow().contains(e.name())
            || pinned_name.contains(e.name())
            || blocked(&key)
        {
            continue;
        }
        pinned_name.insert(e.name());
        best.insert(e.name(), e);
    }
    let mut list: Vec<&E> = best.into_values().collect();
    list.sort_by(|a, b| cmp_cp(a.name(), b.name()));
    add(list, Tier::Catalog, 5, false, &mut out);
    add(ctx.emoji.iter().collect(), Tier::Emoji, 6, false, &mut out);
    out
}

fn total(a: f64, b: f64) -> Ordering {
    a.partial_cmp(&b).unwrap_or(Ordering::Equal)
}

/// tab, completion and picker search: one ranked list.
///   the exact name (the spelling typed leads its case twin)
///   your favorites, in your order, then names you use — only when the name starts with what you typed
///   the room · yours · globals · not loaded yet · emoji
/// inside a tier: starts-with, word-start, inside; then ones you use, more carried, shorter, a-z.
/// copies of one name always sit in send order wherever they land.
pub fn offer<'a, E: Pic>(
    typed: &str,
    ctx: &OfferCtx<'a, E>,
    limit: Option<usize>,
) -> Vec<Candidate<'a, E>> {
    let q = typed.to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let mut pool = gather(ctx, Some(&q));
    for c in &mut pool {
        c.used = ctx.usage.map_or(0.0, |f| f(c.emote.name())).max(0.0);
        let lower = c.emote.name().to_lowercase();
        c.pop = ctx
            .pop
            .and_then(|f| f(&lower))
            .unwrap_or(c.emote.pop() as f64)
            .max(0.0)
            .floor() as u64;
    }
    let mut best_copy: HashMap<&str, u8> = HashMap::new();
    for c in &pool {
        let e = best_copy.entry(c.emote.name()).or_insert(99);
        *e = (*e).min(c.copy);
    }
    let best_copy: HashMap<String, u8> = best_copy
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    // the exact name leads only where typing it sends it: a copy held here (the room, yours, a global).
    // a search answer or an emoji of that exact name leads its own tier, and a favorite or a name you
    // use counts its exact name as starting with what you typed
    let group = |c: &Cand<E>| -> u8 {
        if c.matched == BUCKET_EXACT && c.tier.local() {
            0
        } else if c.matched == BUCKET_NONE {
            9
        } else if (c.used > 0.0 || c.pin.is_some()) && c.matched <= BUCKET_PREFIX {
            1
        } else {
            2 + c.tier.rank()
        }
    };
    let tail = |a: &Cand<E>, b: &Cand<E>| {
        a.len16
            .cmp(&b.len16)
            .then_with(|| cmp_cp(a.emote.name(), b.emote.name()))
            .then_with(|| a.copy.cmp(&b.copy))
            .then_with(|| cmp_cp(&a.key, &b.key))
    };
    pool.sort_by(|a, b| {
        let (ga, gb) = (group(a), group(b));
        if ga != gb {
            return ga.cmp(&gb);
        }
        match ga {
            0 => (a.emote.name() != typed)
                .cmp(&(b.emote.name() != typed))
                .then_with(|| best_copy[a.emote.name()].cmp(&best_copy[b.emote.name()]))
                .then_with(|| cmp_cp(a.emote.name(), b.emote.name()))
                .then_with(|| a.copy.cmp(&b.copy))
                .then_with(|| cmp_cp(&a.key, &b.key)),
            1 => a
                .pin
                .unwrap_or(usize::MAX)
                .cmp(&b.pin.unwrap_or(usize::MAX))
                .then_with(|| total(b.used, a.used))
                .then_with(|| a.tier.rank().cmp(&b.tier.rank()))
                .then_with(|| b.pop.cmp(&a.pop))
                .then_with(|| tail(a, b)),
            9 => b.pop.cmp(&a.pop).then_with(|| tail(a, b)),
            // the spelling typed leads its case twin in every tier, the way it does as the exact name
            _ => a
                .matched
                .cmp(&b.matched)
                .then_with(|| (a.emote.name() != typed).cmp(&(b.emote.name() != typed)))
                .then_with(|| total(b.used, a.used))
                .then_with(|| b.pop.cmp(&a.pop))
                .then_with(|| tail(a, b)),
        }
    });
    // copies of one name keep the slots they earned, but stand in send order within them
    let mut slots: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, c) in pool.iter().enumerate() {
        slots.entry(c.emote.name()).or_default().push(i);
    }
    let mut moves: Vec<(usize, usize, bool)> = Vec::new(); // (slot, from index, shadowed)
    for idx in slots.values() {
        if idx.len() < 2 {
            continue;
        }
        let mut copies: Vec<usize> = idx.clone();
        copies.sort_by(|&a, &b| {
            pool[a]
                .copy
                .cmp(&pool[b].copy)
                .then_with(|| cmp_cp(&pool[a].key, &pool[b].key))
        });
        let native =
            pool[copies[0]].copy == 0 && ctx.room.is_none_or(|r| r.platform != Platform::Kick);
        for (n, &slot) in idx.iter().enumerate() {
            moves.push((slot, copies[n], native && n > 0));
        }
    }
    let list: Vec<Candidate<E>> = {
        let mut by_slot: Vec<(usize, Candidate<E>)> = moves
            .iter()
            .map(|&(slot, from, shadowed)| {
                (
                    slot,
                    Candidate {
                        emote: pool[from].emote,
                        tier: pool[from].tier,
                        matched: pool[from].matched,
                        shadowed,
                    },
                )
            })
            .collect();
        let moved: HashSet<usize> = moves.iter().map(|m| m.0).collect();
        for (i, c) in pool.iter().enumerate() {
            if !moved.contains(&i) {
                by_slot.push((
                    i,
                    Candidate {
                        emote: c.emote,
                        tier: c.tier,
                        matched: c.matched,
                        shadowed: false,
                    },
                ));
            }
        }
        by_slot.sort_by_key(|(i, _)| *i);
        by_slot.into_iter().map(|(_, c)| c).collect()
    };
    match limit {
        Some(n) if n > 0 => list.into_iter().take(n).collect(),
        _ => list,
    }
}

/// something arrived mid-cycle (a room set, a catalog answer). everything up to
/// the cursor stays exactly where it is; the rest is the fresh ranking minus it.
/// cursor -1 = nothing chosen yet.
pub fn extend_offer<'a, E: Pic>(
    prev: &[Candidate<'a, E>],
    cursor: i64,
    typed: &str,
    ctx: &OfferCtx<'a, E>,
    limit: Option<usize>,
) -> Vec<Candidate<'a, E>> {
    let n = (cursor + 1).max(0) as usize;
    let mut out: Vec<Candidate<E>> = prev.iter().take(n).copied().collect();
    let have: HashSet<String> = out.iter().map(|c| key_of(c.emote)).collect();
    out.extend(
        offer(typed, ctx, None)
            .into_iter()
            .filter(|c| !have.contains(&key_of(c.emote))),
    );
    match limit {
        Some(n) if n > 0 => out.truncate(n),
        _ => {}
    }
    out
}

pub struct Browse<'a, E> {
    pub favorite: Vec<Candidate<'a, E>>,
    pub recent: Vec<Candidate<'a, E>>,
    pub room: Vec<Candidate<'a, E>>,
    pub own: Vec<Candidate<'a, E>>,
    pub global: Vec<Candidate<'a, E>>,
}

/// the picker with nothing typed. recents are pictures, not names: one you can no longer reach here is
/// simply absent. a favorite is never absent: one nothing here holds is a `catalog` copy, picked like a
/// search answer. a blocked one is hidden.
pub fn browse<'a, E: Pic>(ctx: &OfferCtx<'a, E>, recents: &[String]) -> Browse<'a, E> {
    let favorites: &'a [E] = ctx.favorites;
    let mut ctx = ctx.clone();
    ctx.catalog = &[];
    ctx.emoji = &[];
    ctx.favorites = &[];
    let all = gather(&ctx, None);
    let used = |c: &Cand<E>| ctx.usage.map_or(0.0, |f| f(c.emote.name()));
    let order = |a: &Cand<E>, b: &Cand<E>| {
        total(used(b), used(a))
            .then_with(|| {
                cmp_cp(
                    &a.emote.name().to_lowercase(),
                    &b.emote.name().to_lowercase(),
                )
            })
            .then_with(|| cmp_cp(a.emote.name(), b.emote.name()))
            .then_with(|| cmp_cp(&a.key, &b.key))
    };
    let plain = |c: &Cand<'a, E>| Candidate {
        emote: c.emote,
        tier: c.tier,
        matched: BUCKET_NONE,
        shadowed: false,
    };
    let of = |t: Tier| {
        let mut l: Vec<&Cand<E>> = all.iter().filter(|c| c.tier == t).collect();
        l.sort_by(|a, b| order(a, b));
        l.into_iter().map(&plain).collect::<Vec<_>>()
    };
    let mut seen: HashSet<&str> = HashSet::new();
    let mut recent = Vec::new();
    for k in recents {
        if let Some(c) = all.iter().find(|c| c.key == *k) {
            if seen.insert(k.as_str()) {
                recent.push(plain(c));
            }
        }
    }
    let mut favorite = Vec::new();
    let mut pinned: HashSet<String> = HashSet::new();
    for e in favorites {
        let key = key_of(e);
        if e.name().is_empty()
            || ctx.blocked.is_some_and(|f| f(&key))
            || !pinned.insert(key.clone())
        {
            continue;
        }
        favorite.push(match all.iter().find(|c| c.key == key) {
            Some(c) => plain(c),
            None => Candidate {
                emote: e,
                tier: Tier::Catalog,
                matched: BUCKET_NONE,
                shadowed: false,
            },
        });
    }
    Browse {
        favorite,
        recent,
        room: of(Tier::Room),
        own: of(Tier::Own),
        global: of(Tier::Global),
    }
}
