//! runs core/tests/emote-law.vectors.json against heatsync_core::law.
//!
//! the file is the site's contract, vendored byte for byte (see
//! scripts/sync-emote-vectors.sh). every case runs under several seeded
//! shuffles of every list in its world: an answer that depends on what loaded
//! first fails here. a kind or a field this runner does not know is a failure,
//! never a skip — a client that silently ignored a new rule would pass while
//! breaking it.

use heatsync_core::emote::{self, Emote, EmoteSet, RefKind, Segment};
use heatsync_core::law::{
    self, Index, OfferCtx, Own, Pic, Platform, Ref, Room, RoomLists, SendCtx, Token,
};
use heatsync_core::tabsets::TabSets;
use heatsync_core::Platform as TabPlatform;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};

const FILE: &str = include_str!("emote-law.vectors.json");
const SHUFFLES: [u32; 5] = [1, 7, 1337, 90210, 424242];

// ── sha256, so the test can print the vendored file's hash without a dependency ──

fn sha256(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
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
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[4 * i],
                chunk[4 * i + 1],
                chunk[4 * i + 2],
                chunk[4 * i + 3],
            ]);
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
        for i in 0..8 {
            h[i] = h[i].wrapping_add(v[i]);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

// ── seeded shuffles (mulberry32, as the site's runner) ──

struct Rng(u32);
impl Rng {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x6d2b79f5);
        let mut t = self.0;
        t = (t ^ (t >> 15)).wrapping_mul(t | 1);
        t ^= t.wrapping_add((t ^ (t >> 7)).wrapping_mul(t | 61));
        f64::from(t ^ (t >> 14)) / 4294967296.0
    }
    fn shuffled<T: Clone>(&mut self, list: &[T]) -> Vec<T> {
        let mut out = list.to_vec();
        for i in (1..out.len()).rev() {
            let j = (self.next() * (i + 1) as f64).floor() as usize;
            out.swap(i, j);
        }
        out
    }
}

// ── the world ──

#[derive(Clone, Debug)]
struct Em {
    p: String,
    id: String,
    name: String,
    zw: bool,
    pop: u64,
}
impl Pic for Em {
    fn provider(&self) -> &str {
        &self.p
    }
    fn id(&self) -> &str {
        &self.id
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn zero_width(&self) -> bool {
        self.zw
    }
    fn pop(&self) -> u64 {
        self.pop
    }
}

struct World {
    emotes: HashMap<String, Em>,
    alias_of: HashMap<String, String>,
    rooms: HashMap<String, Room<Em>>,
    people: HashMap<String, Own<Em>>,
    globals: Index<Em>,
}

type Res<T> = Result<T, String>;

fn known(obj: &Map<String, Value>, allowed: &[&str], at: &str) -> Res<()> {
    for k in obj.keys() {
        if !allowed.contains(&k.as_str()) {
            return Err(format!(
                "{at}: field \"{k}\" is not one this runner understands"
            ));
        }
    }
    Ok(())
}

fn obj<'a>(v: &'a Value, at: &str) -> Res<&'a Map<String, Value>> {
    v.as_object()
        .ok_or_else(|| format!("{at}: expected an object"))
}

fn strs(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

impl World {
    fn emote(&self, alias: &str) -> Res<&Em> {
        self.emotes
            .get(alias)
            .ok_or_else(|| format!("unknown emote alias: {alias}"))
    }
    fn list(&self, rng: &mut Rng, v: Option<&Value>) -> Res<Vec<Em>> {
        rng.shuffled(&strs(v))
            .iter()
            .map(|a| self.emote(a).cloned())
            .collect()
    }
    fn alias(&self, e: Option<&Em>) -> Value {
        match e {
            None => Value::Null,
            Some(e) => Value::String(
                self.alias_of
                    .get(&law::key_of(e))
                    .cloned()
                    .unwrap_or_else(|| format!("?{}", law::key_of(e))),
            ),
        }
    }
    fn room(&self, id: Option<&str>) -> Res<Option<&Room<Em>>> {
        match id {
            None => Ok(None),
            Some(id) => self
                .rooms
                .get(id)
                .map(Some)
                .ok_or_else(|| format!("unknown room: {id}")),
        }
    }
    fn person(&self, who: Option<&str>) -> Res<Option<&Own<Em>>> {
        match who {
            None => Ok(None),
            Some(w) => self
                .people
                .get(w)
                .map(Some)
                .ok_or_else(|| format!("unknown person: {w}")),
        }
    }
}

fn build_world(file: &Map<String, Value>, raw: &Map<String, Value>, rng: &mut Rng) -> Res<World> {
    known(raw, &["rooms", "globals", "people"], "world")?;
    let mut emotes = HashMap::new();
    let mut alias_of: HashMap<String, String> = HashMap::new();
    for (alias, r) in obj(&file["emotes"], "emotes")? {
        let r = obj(r, alias)?;
        known(r, &["p", "id", "name", "zw", "pop"], alias)?;
        let e = Em {
            p: r["p"].as_str().ok_or("emote without p")?.to_string(),
            id: r["id"].as_str().ok_or("emote without id")?.to_string(),
            name: r["name"].as_str().ok_or("emote without name")?.to_string(),
            zw: r.get("zw").and_then(Value::as_bool).unwrap_or(false),
            pop: r.get("pop").and_then(Value::as_u64).unwrap_or(0),
        };
        if let Some(other) = alias_of.insert(law::key_of(&e), alias.clone()) {
            return Err(format!("two aliases for one picture: {other} and {alias}"));
        }
        emotes.insert(alias.clone(), e);
    }
    let mut w = World {
        emotes,
        alias_of,
        rooms: HashMap::new(),
        people: HashMap::new(),
        globals: HashMap::new(),
    };
    let rooms = obj(&raw["rooms"], "rooms")?;
    let ids: Vec<String> = rooms.keys().cloned().collect();
    for id in rng.shuffled(&ids) {
        let r = obj(&rooms[&id], &id)?;
        known(r, &["owner", "third", "native", "loaded"], &id)?;
        let cut = id.find(':').ok_or("room id without a colon")?;
        let platform =
            Platform::parse(&id[..cut]).ok_or_else(|| format!("unknown platform in room {id}"))?;
        let (owner, third, native) = (
            w.list(rng, r.get("owner"))?,
            w.list(rng, r.get("third"))?,
            w.list(rng, r.get("native"))?,
        );
        let loaded = r.get("loaded").and_then(Value::as_bool).unwrap_or(true);
        w.rooms.insert(
            id.clone(),
            law::build_room(
                platform,
                &id[cut + 1..],
                RoomLists {
                    owner: &owner,
                    third: &third,
                    native: &native,
                },
                loaded,
            ),
        );
    }
    let people = obj(&raw["people"], "people")?;
    let whos: Vec<String> = people.keys().cloned().collect();
    for who in rng.shuffled(&whos) {
        let p = obj(&people[&who], &who)?;
        known(p, &["inventory", "personal", "native"], &who)?;
        let mut native = Vec::new();
        if let Some(n) = p.get("native") {
            let n = obj(n, "native")?;
            let plats: Vec<String> = n.keys().cloned().collect();
            for plat in rng.shuffled(&plats) {
                native.push((
                    Platform::parse(&plat).ok_or_else(|| format!("unknown platform {plat}"))?,
                    w.list(rng, n.get(&plat))?,
                ));
            }
        }
        let own = law::build_own(
            &w.list(rng, p.get("inventory"))?,
            &w.list(rng, p.get("personal"))?,
            &native,
        );
        w.people.insert(who, own);
    }
    w.globals = law::build_globals(&w.list(rng, Some(&raw["globals"]))?);
    Ok(w)
}

// ── shapes ──

fn refs_in<'a>(w: &'a World, raw: Option<&Value>) -> Res<Option<HashMap<String, Ref<'a, Em>>>> {
    let Some(raw) = raw.filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let mut out = HashMap::new();
    for (k, v) in obj(raw, "refs")? {
        let a = v.as_str().ok_or("a ref is an alias or \"gone\"")?;
        out.insert(
            k.clone(),
            if a == "gone" {
                Ref::Gone
            } else {
                Ref::Pic(w.emote(a)?)
            },
        );
    }
    Ok(Some(out))
}

fn refs_out(w: &World, refs: &HashMap<String, Ref<Em>>) -> Value {
    Value::Object(
        refs.iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    match v {
                        Ref::Gone => json!("gone"),
                        Ref::Pic(e) => w.alias(Some(e)),
                    },
                )
            })
            .collect(),
    )
}

fn tokens_in<'a>(w: &'a World, raw: Option<&Value>) -> Res<Vec<Token<'a, Em>>> {
    let mut out = Vec::new();
    for t in raw
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let cp = t["cp"].as_array().ok_or("token without cp")?;
        let at = |i: usize| {
            cp.get(i)
                .and_then(Value::as_u64)
                .map(|n| n as usize)
                .ok_or("token cp is two numbers")
        };
        out.push(Token {
            start: at(0)?,
            end: at(1)?,
            emote: w.emote(t["emote"].as_str().ok_or("token without emote")?)?,
        });
    }
    Ok(out)
}

fn tokens_out(w: &World, tokens: &[Token<Em>]) -> Value {
    Value::Array(
        tokens
            .iter()
            .map(|t| json!({ "cp": [t.start, t.end], "emote": w.alias(Some(t.emote)) }))
            .collect(),
    )
}

fn chips_out(w: &World, drawn: &[law::Drawn<Em>], with_from: bool) -> Value {
    Value::Array(
        drawn
            .iter()
            .map(|d| {
                let mut m = Map::new();
                m.insert("word".into(), json!(d.word));
                m.insert("emote".into(), w.alias(d.emote));
                if with_from {
                    m.insert("from".into(), json!(d.from.as_str()));
                }
                m.insert("overlay".into(), json!(d.overlay));
                Value::Object(m)
            })
            .collect(),
    )
}

/// everything an offer, extend or browse case can say about its surroundings
struct Surroundings {
    catalog: Vec<Em>,
    emoji: Vec<Em>,
    usage: HashMap<String, f64>,
    pop: Option<HashMap<String, f64>>,
    blocked: HashSet<String>,
    suggest_own: bool,
}

const OFFER_PART: [&str; 7] = [
    "room",
    "catalog",
    "emoji",
    "usage",
    "pop",
    "blocked",
    "suggestOwn",
];

impl Surroundings {
    fn of(w: &World, part: &Map<String, Value>) -> Res<Surroundings> {
        let list = |k: &str| -> Res<Vec<Em>> {
            strs(part.get(k))
                .iter()
                .map(|a| w.emote(a).cloned())
                .collect()
        };
        let nums = |k: &str| -> Option<HashMap<String, f64>> {
            part.get(k).and_then(Value::as_object).map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_f64().map(|n| (k.clone(), n)))
                    .collect()
            })
        };
        let blocked = strs(part.get("blocked"))
            .iter()
            .map(|a| w.emote(a).map(law::key_of))
            .collect::<Res<HashSet<_>>>()?;
        Ok(Surroundings {
            catalog: list("catalog")?,
            emoji: list("emoji")?,
            usage: nums("usage").unwrap_or_default(),
            pop: nums("pop"),
            blocked,
            suggest_own: part
                .get("suggestOwn")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        })
    }
}

fn offer_ctx<'a>(
    w: &'a World,
    who: Option<&str>,
    room: Option<&str>,
    s: &'a Surroundings,
    usage: &'a dyn Fn(&str) -> f64,
    pop: &'a dyn Fn(&str) -> Option<f64>,
    blocked: &'a dyn Fn(&str) -> bool,
) -> Res<OfferCtx<'a, Em>> {
    let own = w.person(who)?.ok_or("an offer needs a person")?;
    Ok(OfferCtx {
        room: w.room(room)?,
        own,
        globals: &w.globals,
        catalog: &s.catalog,
        emoji: &s.emoji,
        usage: Some(usage),
        pop: s.pop.as_ref().map(|_| pop),
        blocked: Some(blocked),
        suggest_own: s.suggest_own,
    })
}

fn offered(w: &World, list: &[law::Candidate<Em>]) -> Value {
    Value::Array(
        list.iter()
            .map(|c| {
                let mut m = Map::new();
                m.insert("emote".into(), w.alias(Some(c.emote)));
                m.insert("tier".into(), json!(c.tier.as_str()));
                m.insert("match".into(), json!(c.matched));
                if c.shadowed {
                    m.insert("shadowed".into(), json!(true));
                }
                Value::Object(m)
            })
            .collect(),
    )
}

const KNOWN_KIND: [(&str, &[&str]); 5] = [
    (
        "read",
        &[
            "kind", "name", "world", "room", "sender", "text", "tokens", "refs", "fold", "expect",
        ],
    ),
    ("session", &["kind", "name", "world", "who", "steps"]),
    (
        "offer",
        &[
            "kind",
            "name",
            "world",
            "room",
            "who",
            "typed",
            "catalog",
            "emoji",
            "usage",
            "pop",
            "blocked",
            "suggestOwn",
            "limit",
            "expect",
        ],
    ),
    (
        "extend",
        &[
            "kind", "name", "world", "who", "typed", "cursor", "prev", "next", "limit", "expect",
        ],
    ),
    (
        "browse",
        &[
            "kind",
            "name",
            "world",
            "room",
            "who",
            "recents",
            "usage",
            "blocked",
            "suggestOwn",
            "expect",
        ],
    ),
];
const STEP: [&str; 8] = [
    "room",
    "legs",
    "picks",
    "stored",
    "fold",
    "text",
    "textRepeat",
    "expect",
];
const STEP_EXPECT: [&str; 6] = ["from", "refs", "tokens", "wire", "wireIsText", "chips"];

fn str_of<'a>(c: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    c.get(k).and_then(Value::as_str)
}

fn run(file: &Map<String, Value>, c: &Value, seed: u32) -> Res<Value> {
    let c = obj(c, "case")?;
    let name = str_of(c, "name").unwrap_or("?");
    let kind = str_of(c, "kind").ok_or("a case without a kind")?;
    let allowed = KNOWN_KIND
        .iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, f)| *f)
        .ok_or_else(|| format!("case kind \"{kind}\" is not one this runner understands"))?;
    known(c, allowed, name)?;
    let world = str_of(c, "world").ok_or("a case without a world")?;
    let raw = obj(
        file["worlds"]
            .get(world)
            .ok_or_else(|| format!("unknown world: {world}"))?,
        world,
    )?;
    let mut rng = Rng(seed);
    let w = build_world(file, raw, &mut rng)?;

    match kind {
        "read" => {
            let text = str_of(c, "text").ok_or("read without text")?;
            let refs = refs_in(&w, c.get("refs"))?;
            let ctx = law::ReadCtx {
                room: w.room(str_of(c, "room"))?,
                refs: refs.as_ref(),
                sender: w.person(str_of(c, "sender"))?,
                globals: &w.globals,
                fold: c.get("fold").and_then(Value::as_bool).unwrap_or(false),
            };
            Ok(chips_out(
                &w,
                &law::read_line(text, &tokens_in(&w, c.get("tokens"))?, &ctx),
                true,
            ))
        }
        "session" => {
            let own = w
                .person(str_of(c, "who"))?
                .ok_or("a session needs a person")?;
            let mut out = Vec::new();
            for (i, s) in c["steps"]
                .as_array()
                .ok_or("session without steps")?
                .iter()
                .enumerate()
            {
                let s = obj(s, "step")?;
                known(s, &STEP, &format!("{name} step {i}"))?;
                let expect = obj(&s["expect"], "expect")?;
                known(expect, &STEP_EXPECT, &format!("{name} step {i} expect"))?;
                let text = match s.get("textRepeat") {
                    Some(r) => vec![
                        r[0].as_str().ok_or("textRepeat word")?;
                        r[1].as_u64().ok_or("textRepeat count")? as usize
                    ]
                    .join(" "),
                    None => str_of(s, "text").ok_or("step without text")?.to_string(),
                };
                let picks: Option<Index<Em>> = match s.get("picks") {
                    Some(p) => Some(
                        obj(p, "picks")?
                            .iter()
                            .map(|(k, v)| {
                                w.emote(v.as_str().unwrap_or(""))
                                    .map(|e| (k.clone(), e.clone()))
                            })
                            .collect::<Res<_>>()?,
                    ),
                    None => None,
                };
                let legs: Vec<&Room<Em>> = strs(s.get("legs"))
                    .iter()
                    .map(|id| w.room(Some(id)).map(|r| r.expect("a leg is a room")))
                    .collect::<Res<_>>()?;
                let ctx = SendCtx {
                    room: w.room(str_of(s, "room"))?,
                    legs: &legs,
                    own,
                    globals: &w.globals,
                    picks: picks.as_ref(),
                    fold: s.get("fold").and_then(Value::as_bool).unwrap_or(false),
                };
                let stored = refs_in(&w, s.get("stored"))?;
                let shipped = if s.get("stored").is_some_and(|v| !v.is_null()) {
                    law::edit_line(stored.as_ref(), &text, &ctx)
                } else {
                    law::send_line(&text, &ctx)
                };
                let mut got = Map::new();
                if let Some(from) = expect.get("from") {
                    got.insert(
                        "from".into(),
                        Value::Object(
                            obj(from, "from")?
                                .keys()
                                .map(|word| {
                                    (
                                        word.clone(),
                                        law::send_word(word, &ctx)
                                            .map_or(Value::Null, |h| json!(h.from.as_str())),
                                    )
                                })
                                .collect(),
                        ),
                    );
                }
                got.insert("refs".into(), refs_out(&w, &shipped.refs));
                got.insert("tokens".into(), tokens_out(&w, &shipped.tokens));
                if expect.contains_key("wire") {
                    got.insert(
                        "wire".into(),
                        Value::Object(
                            shipped
                                .wire
                                .iter()
                                .map(|(p, t)| (p.as_str().to_string(), json!(t)))
                                .collect(),
                        ),
                    );
                }
                if expect.contains_key("wireIsText") {
                    let hit: Vec<&str> = strs(expect.get("wireIsText"))
                        .iter()
                        .filter_map(|p| Platform::parse(p))
                        .filter(|p| shipped.wire.get(p).is_some_and(|t| *t == text))
                        .map(Platform::as_str)
                        .collect();
                    got.insert("wireIsText".into(), json!(hit));
                }
                got.insert("chips".into(), chips_out(&w, &shipped.chips, false));
                out.push(Value::Object(got));
            }
            Ok(Value::Array(out))
        }
        "offer" => {
            let s = Surroundings::of(&w, c)?;
            let (usage, pop, blocked) = (
                |n: &str| s.usage.get(n).copied().unwrap_or(0.0),
                |l: &str| s.pop.as_ref().and_then(|p| p.get(l).copied()),
                |k: &str| s.blocked.contains(k),
            );
            let ctx = offer_ctx(
                &w,
                str_of(c, "who"),
                str_of(c, "room"),
                &s,
                &usage,
                &pop,
                &blocked,
            )?;
            let limit = c.get("limit").and_then(Value::as_u64).map(|n| n as usize);
            Ok(offered(
                &w,
                &law::offer(
                    str_of(c, "typed").ok_or("offer without typed")?,
                    &ctx,
                    limit,
                ),
            ))
        }
        "extend" => {
            let who = str_of(c, "who");
            let typed = str_of(c, "typed").ok_or("extend without typed")?;
            let (prev_part, next_part) = (obj(&c["prev"], "prev")?, obj(&c["next"], "next")?);
            known(prev_part, &OFFER_PART, &format!("{name} prev"))?;
            known(next_part, &OFFER_PART, &format!("{name} next"))?;
            let (ps, ns) = (
                Surroundings::of(&w, prev_part)?,
                Surroundings::of(&w, next_part)?,
            );
            let (pu, pp, pb) = (
                |n: &str| ps.usage.get(n).copied().unwrap_or(0.0),
                |l: &str| ps.pop.as_ref().and_then(|p| p.get(l).copied()),
                |k: &str| ps.blocked.contains(k),
            );
            let (nu, np, nb) = (
                |n: &str| ns.usage.get(n).copied().unwrap_or(0.0),
                |l: &str| ns.pop.as_ref().and_then(|p| p.get(l).copied()),
                |k: &str| ns.blocked.contains(k),
            );
            let prev = law::offer(
                typed,
                &offer_ctx(&w, who, str_of(prev_part, "room"), &ps, &pu, &pp, &pb)?,
                None,
            );
            let next = offer_ctx(&w, who, str_of(next_part, "room"), &ns, &nu, &np, &nb)?;
            let limit = c.get("limit").and_then(Value::as_u64).map(|n| n as usize);
            Ok(offered(
                &w,
                &law::extend_offer(
                    &prev,
                    c["cursor"].as_i64().ok_or("extend without cursor")?,
                    typed,
                    &next,
                    limit,
                ),
            ))
        }
        _ => {
            let s = Surroundings::of(&w, c)?;
            let (usage, pop, blocked) = (
                |n: &str| s.usage.get(n).copied().unwrap_or(0.0),
                |l: &str| s.pop.as_ref().and_then(|p| p.get(l).copied()),
                |k: &str| s.blocked.contains(k),
            );
            let ctx = offer_ctx(
                &w,
                str_of(c, "who"),
                str_of(c, "room"),
                &s,
                &usage,
                &pop,
                &blocked,
            )?;
            let recents: Vec<String> = strs(c.get("recents"))
                .iter()
                .map(|a| w.emote(a).map(law::key_of))
                .collect::<Res<_>>()?;
            let b = law::browse(&ctx, &recents);
            let names = |l: &[law::Candidate<Em>]| {
                Value::Array(l.iter().map(|x| w.alias(Some(x.emote))).collect())
            };
            Ok(
                json!({ "recent": names(&b.recent), "room": names(&b.room), "own": names(&b.own), "global": names(&b.global) }),
            )
        }
    }
}

/// what a case must produce: its `expect`, in the shape `run` returns
fn expected(c: &Value) -> Value {
    if c["kind"] != "session" {
        return c["expect"].clone();
    }
    Value::Array(
        c["steps"]
            .as_array()
            .map(|v| v.as_slice())
            .unwrap_or_default()
            .iter()
            .map(|s| {
                Value::Object(
                    STEP_EXPECT
                        .iter()
                        .filter_map(|k| s["expect"].get(*k).map(|v| ((*k).to_string(), v.clone())))
                        .collect(),
                )
            })
            .collect(),
    )
}

fn file() -> Map<String, Value> {
    match serde_json::from_str::<Value>(FILE).expect("the vector file is json") {
        Value::Object(m) => m,
        _ => panic!("the vector file is an object"),
    }
}

#[test]
fn the_vendored_file_has_a_known_hash() {
    // print it, so it can be compared with the site's `sha256sum shared/emote-law.vectors.json`
    let got = sha256(FILE.as_bytes());
    println!("emote-law.vectors.json sha256 {got}");
    // the pinned hash is written by scripts/sync-emote-vectors.sh from the site's
    // file: a hand-edited copy of the vectors no longer matches it
    assert_eq!(
        got,
        include_str!("emote-law.vectors.sha256").trim(),
        "the vendored vectors were edited: refresh them with scripts/sync-emote-vectors.sh"
    );
    assert_eq!(
        sha256(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn is_the_version_this_code_implements() {
    assert_eq!(file()["version"], json!(law::LAW_VERSION));
}

#[test]
fn names_every_case_once_and_covers_every_job() {
    let f = file();
    let cases = f["cases"].as_array().unwrap();
    let names: HashSet<&str> = cases.iter().filter_map(|c| c["name"].as_str()).collect();
    assert_eq!(names.len(), cases.len(), "two cases share a name");
    let kinds: HashSet<&str> = cases.iter().filter_map(|c| c["kind"].as_str()).collect();
    assert_eq!(
        kinds,
        KNOWN_KIND.iter().map(|(k, _)| *k).collect::<HashSet<_>>()
    );
}

#[test]
fn every_case_passes_under_every_shuffle() {
    let f = file();
    known(
        &f,
        &["version", "about", "emotes", "worlds", "cases"],
        "the vector file",
    )
    .unwrap();
    let cases = f["cases"].as_array().unwrap();
    let mut failed = Vec::new();
    for c in cases {
        let want = expected(c);
        for seed in SHUFFLES {
            match run(&f, c, seed) {
                Ok(got) if got == want => {}
                Ok(got) => {
                    failed.push(format!(
                        "{} (shuffle {seed})\n  want {want}\n  got  {got}",
                        c["name"]
                    ));
                    break;
                }
                Err(e) => {
                    failed.push(format!("{} (shuffle {seed}): {e}", c["name"]));
                    break;
                }
            }
        }
    }
    println!(
        "emote law: {}/{} cases pass under {} shuffles each",
        cases.len() - failed.len(),
        cases.len(),
        SHUFFLES.len()
    );
    assert!(
        failed.is_empty(),
        "{} case(s) failed:\n{}",
        failed.len(),
        failed.join("\n")
    );
}

#[test]
fn refuses_a_case_it_does_not_understand_instead_of_skipping_it() {
    let f = file();
    let err = |c: Value| run(&f, &c, 1).unwrap_err();
    assert!(err(json!({ "kind": "rank", "name": "x", "world": "main" }))
        .contains("not one this runner understands"));
    assert!(err(json!({ "kind": "read", "name": "x", "world": "main", "room": null, "sender": null, "text": "a", "viewer": "me", "expect": [] })).contains("field \"viewer\""));
    assert!(err(json!({ "kind": "session", "name": "x", "world": "main", "who": "me", "steps": [{ "text": "a", "extra": 1, "expect": {} }] })).contains("field \"extra\""));
    assert!(err(json!({ "kind": "session", "name": "x", "world": "main", "who": "me", "steps": [{ "text": "a", "expect": { "colour": 1 } }] })).contains("field \"colour\""));
}

// ── the terminal's own drawing path ─────────────────────────────────────────
//
// `segments_with` is what the terminal draws a chat line with. these cases run
// the vectors' `read` cases through it, so the drawing path is held to the law
// itself and not only the helper the runner above calls. what the terminal can
// say maps like this: a shipped pick or a taken-down pick is a Pick / Gone ref,
// the sender's 7tv personal set rides as picks (nothing in its tier is lower
// than a pick but a same-named pick, which the ref map already decides), the
// sender's inventory is the relay's Sender refs. a case with a platform token
// span (positions are not kept after the text is rewritten) or the case-fold
// setting (the terminal has none) is not one it can say, and is counted.

fn tui_emote(w: &World, alias: &str, kind: RefKind) -> Res<Emote> {
    let e = w.emote(alias)?;
    Ok(Emote {
        name: e.name.clone(),
        url: format!("https://v.test/{alias}.webp"),
        provider: e.p.clone(),
        id: e.id.clone(),
        animated: false,
        zero_width: e.zw,
        global: false,
        kind,
    })
}

/// (words drawn by the terminal, words the law expects) for one `read` case,
/// None when the terminal cannot say it.
fn drawing(
    file: &Map<String, Value>,
    c: &Value,
    seed: u32,
) -> Res<Option<(Vec<String>, Vec<String>)>> {
    let c = obj(c, "case")?;
    if str_of(c, "kind") != Some("read")
        || c.get("fold").and_then(Value::as_bool) == Some(true)
        || c.get("tokens")
            .is_some_and(|t| !t.as_array().is_some_and(Vec::is_empty))
    {
        return Ok(None);
    }
    let raw = obj(&file["worlds"][str_of(c, "world").ok_or("world")?], "world")?;
    let mut rng = Rng(seed);
    let w = build_world(file, raw, &mut rng)?;
    let em = |a: &String, k: RefKind| tui_emote(&w, a, k);
    let list = |v: Option<&Value>, k: RefKind| -> Res<Vec<Emote>> {
        rng_list(&mut Rng(seed), v)
            .iter()
            .map(|a| em(a, k))
            .collect()
    };
    // a room's set + the globals, exactly as the terminal builds one source room
    let globals = EmoteSet::from_global_list(list(Some(&raw["globals"]), RefKind::Sender)?);
    let room_of = |id: &str| -> Res<EmoteSet> {
        let r = obj(&raw["rooms"][id], id)?;
        let mut lists = Vec::new();
        for k in ["owner", "third", "native"] {
            lists.extend(list(r.get(k), RefKind::Sender)?);
        }
        let mut set = globals.clone();
        set.merge(EmoteSet::from_list(lists));
        Ok(set)
    };
    let room_id = str_of(c, "room");
    let set = match room_id {
        Some(id) => room_of(id)?,
        None => globals.clone(),
    };
    // the same world as ONE merged tab holding every room in it: the row is read
    // against its own room, so the answer must not depend on its neighbours
    let platform_of = |s: &str| match s {
        "kick" => TabPlatform::Kick,
        "youtube" => TabPlatform::Youtube,
        _ => TabPlatform::Twitch,
    };
    let mut rooms = Vec::new();
    for id in raw["rooms"]
        .as_object()
        .map(|m| m.keys())
        .into_iter()
        .flatten()
    {
        let (p, ch) = id.split_once(':').ok_or("room id")?;
        rooms.push((platform_of(p), ch.to_string(), room_of(id)?));
    }
    let tab = TabSets::new(EmoteSet::new(), rooms);
    // the line's own refs: the sender's personal set and shipped picks (picks), the inventory (sender)
    let mut line: Vec<Emote> = Vec::new();
    let mut named: HashSet<String> = HashSet::new();
    if let Some(r) = c.get("refs").and_then(Value::as_object) {
        for (name, v) in r {
            let a = v.as_str().ok_or("ref alias")?;
            named.insert(name.clone());
            line.push(if a == "gone" {
                Emote::gone(name).ok_or("gone name")?
            } else {
                Emote {
                    name: name.clone(),
                    ..tui_emote(&w, a, RefKind::Pick)?
                }
            });
        }
    }
    if let Some(who) = str_of(c, "sender") {
        let p = obj(&raw["people"][who], who)?;
        let (inv, pers) = (
            list(p.get("inventory"), RefKind::Sender)?,
            list(p.get("personal"), RefKind::Pick)?,
        );
        let personal = law::build_index(&pers, None);
        for (n, e) in personal {
            if named.insert(n) {
                line.push(e);
            }
        }
        for (n, e) in law::build_index(&inv, None) {
            if named.insert(n) {
                line.push(e);
            }
        }
    }
    let refs = emote::ref_set(&line);
    let text = str_of(c, "text").ok_or("text")?;
    let draw = |set: &EmoteSet| -> Vec<String> {
        emote::segments_with(text, set, &refs)
            .into_iter()
            .filter_map(|s| match s {
                Segment::Stack(st) => Some(st.layers.into_iter().map(|l| {
                    l.url
                        .trim_start_matches("https://v.test/")
                        .trim_end_matches(".webp")
                        .to_string()
                })),
                Segment::Text(_) => None,
            })
            .flatten()
            .collect()
    };
    let got = draw(&set);
    if let Some(id) = room_id {
        let (p, ch) = id.split_once(':').ok_or("room id")?;
        let in_tab = draw(tab.for_line(platform_of(p), ch));
        if in_tab != got {
            return Err(format!(
                "merged tab read {in_tab:?}, its own room read {got:?}"
            ));
        }
    }
    let want: Vec<String> = c["expect"]
        .as_array()
        .ok_or("expect")?
        .iter()
        .filter_map(|d| d["emote"].as_str().map(String::from))
        .collect();
    Ok(Some((got, want)))
}

fn rng_list(rng: &mut Rng, v: Option<&Value>) -> Vec<String> {
    rng.shuffled(&strs(v))
}

#[test]
fn the_terminals_drawing_path_agrees_with_the_law_on_every_read_case_it_can_say() {
    let f = file();
    let (mut said, mut skipped, mut failed) = (0, 0, Vec::new());
    for c in f["cases"].as_array().unwrap() {
        for seed in SHUFFLES {
            match drawing(&f, c, seed) {
                Ok(None) => {
                    skipped += 1;
                    break;
                }
                Ok(Some((got, want))) if got == want => {}
                Ok(Some((got, want))) => {
                    failed.push(format!(
                        "{} (shuffle {seed})\n  want {want:?}\n  got  {got:?}",
                        c["name"]
                    ));
                    break;
                }
                Err(e) => {
                    failed.push(format!("{}: {e}", c["name"]));
                    break;
                }
            }
            if seed == SHUFFLES[SHUFFLES.len() - 1] {
                said += 1;
            }
        }
    }
    println!("terminal drawing: {said} read cases agree under every shuffle, {skipped} cases it cannot say (tokens, case fold, and the other jobs)");
    // exact, so a quietly shrinking set fails: 22 read cases agree, 5 of the 27
    // read cases are skipped (3 token spans, 2 case fold), and the other 51
    // cases (session, offer, extend, browse) are not drawing
    assert_eq!((said, skipped), (22, 56), "the drawing split moved");
    assert!(
        failed.is_empty(),
        "{} case(s) differ:\n{}",
        failed.len(),
        failed.join("\n")
    );
}
