//! runs core/tests/emote-mark.vectors.json against heatsync_core::mark::tile_says.
//!
//! the file is the site's contract, vendored byte for byte (see
//! scripts/sync-emote-vectors.sh): who sees a picker tile's picture if you send
//! it from this room, and whether the tile is offered at all. every case runs
//! under several seeded shuffles of every list in its world: an answer that
//! depends on what loaded first fails here. a kind or a field this runner does
//! not know is a failure, never a skip.

mod common;

use common::{known, obj, sha256, strs, Res, Rng};
use heatsync_core::law::{self, Index, Own, Pic, Platform, Room, RoomLists, SendCtx};
use heatsync_core::mark::{self, Kind, Mark, TileSays};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};

const FILE: &str = include_str!("emote-mark.vectors.json");
const SHUFFLES: [u32; 5] = [1, 7, 1337, 90210, 424242];

const CASE: [&str; 10] = [
    "kind", "name", "world", "room", "legs", "who", "tile", "tier", "fold", "expect",
];
const EXPECT: [&str; 5] = ["offered", "mark", "kind", "as", "tier"];
const KINDS: [&str; 1] = ["tile"];

// ── the world ──

#[derive(Clone, Debug)]
struct Em {
    p: String,
    id: String,
    name: String,
    zw: bool,
    pop: u64,
    pic: Option<String>,
    hash: Option<String>,
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
    fn pic(&self) -> String {
        self.pic.clone().unwrap_or_else(|| law::key_of(self))
    }
    fn hash(&self) -> Option<&str> {
        self.hash.as_deref()
    }
}

struct World {
    emotes: HashMap<String, Em>,
    rooms: HashMap<String, Room<Em>>,
    people: HashMap<String, Own<Em>>,
    globals: Index<Em>,
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
}

fn build_world(file: &Map<String, Value>, raw: &Map<String, Value>, rng: &mut Rng) -> Res<World> {
    known(raw, &["rooms", "globals", "people"], "world")?;
    let mut emotes = HashMap::new();
    let mut seen: HashMap<String, String> = HashMap::new();
    for (alias, r) in obj(&file["emotes"], "emotes")? {
        let r = obj(r, alias)?;
        known(r, &["p", "id", "name", "zw", "pop", "pic", "hash"], alias)?;
        let text = |k: &str| r.get(k).and_then(Value::as_str).map(String::from);
        let e = Em {
            p: text("p").ok_or("emote without p")?,
            id: text("id").ok_or("emote without id")?,
            name: text("name").ok_or("emote without name")?,
            zw: r.get("zw").and_then(Value::as_bool).unwrap_or(false),
            pop: r.get("pop").and_then(Value::as_u64).unwrap_or(0),
            pic: text("pic"),
            hash: text("hash"),
        };
        if let Some(other) = seen.insert(law::key_of(&e), alias.clone()) {
            return Err(format!("two aliases for one picture: {other} and {alias}"));
        }
        emotes.insert(alias.clone(), e);
    }
    let mut w = World {
        emotes,
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

// ── a case ──

fn says_json(s: &TileSays) -> Value {
    json!({
        "offered": s.offered,
        "mark": s.mark.as_str(),
        "kind": s.kind.as_str(),
        "as": s.as_,
        "tier": s.tier,
    })
}

fn run(file: &Map<String, Value>, c: &Value, seed: u32) -> Res<Value> {
    let c = obj(c, "case")?;
    let name = c.get("name").and_then(Value::as_str).unwrap_or("?");
    let kind = c
        .get("kind")
        .and_then(Value::as_str)
        .ok_or("a case without a kind")?;
    if !KINDS.contains(&kind) {
        return Err(format!(
            "case kind \"{kind}\" is not one this runner understands"
        ));
    }
    known(c, &CASE, name)?;
    known(
        obj(c.get("expect").ok_or("a case without an expect")?, "expect")?,
        &EXPECT,
        &format!("{name} expect"),
    )?;
    let world = c
        .get("world")
        .and_then(Value::as_str)
        .ok_or("a case without a world")?;
    let raw = obj(
        file["worlds"]
            .get(world)
            .ok_or_else(|| format!("unknown world: {world}"))?,
        world,
    )?;
    let w = build_world(file, raw, &mut Rng(seed))?;
    let who = c
        .get("who")
        .and_then(Value::as_str)
        .ok_or("a tile is read for a person")?;
    let own = w
        .people
        .get(who)
        .ok_or_else(|| format!("unknown person: {who}"))?;
    let legs: Vec<&Room<Em>> = strs(c.get("legs"))
        .iter()
        .map(|id| w.room(Some(id)).map(|r| r.expect("a leg is a room")))
        .collect::<Res<_>>()?;
    let ctx = SendCtx {
        room: w.room(c.get("room").and_then(Value::as_str))?,
        legs: &legs,
        own,
        globals: &w.globals,
        picks: None,
        fold: c.get("fold").and_then(Value::as_bool).unwrap_or(false),
    };
    let tile = w.emote(
        c.get("tile")
            .and_then(Value::as_str)
            .ok_or("a case without a tile")?,
    )?;
    let tier = c
        .get("tier")
        .and_then(Value::as_str)
        .ok_or("a case without a tier")?;
    let says = mark::tile_says(&ctx, tile, tier);
    // the cheap answer a list asks is the tile's own `offered`, never another rule
    if mark::offered(&ctx, tile) != says.offered {
        return Err(format!("{name}: offered() and tile_says().offered differ"));
    }
    Ok(says_json(&says))
}

fn file() -> Map<String, Value> {
    match serde_json::from_str::<Value>(FILE).expect("the vector file is json") {
        Value::Object(m) => m,
        _ => panic!("the vector file is an object"),
    }
}

// ── the contract ──

#[test]
fn the_vendored_file_has_a_known_hash() {
    // print it, so it can be compared with the site's `sha256sum shared/emote-mark.vectors.json`
    let got = sha256(FILE.as_bytes());
    println!("emote-mark.vectors.json sha256 {got}");
    // the pinned hash is written by scripts/sync-emote-vectors.sh from the site's
    // file: a hand-edited copy of the vectors no longer matches it
    assert_eq!(
        got,
        include_str!("emote-mark.vectors.sha256").trim(),
        "the vendored vectors were edited: refresh them with scripts/sync-emote-vectors.sh"
    );
}

#[test]
fn is_the_version_this_code_implements() {
    assert_eq!(file()["version"], json!(mark::MARK_VERSION));
}

#[test]
fn names_every_case_once_and_has_enough_of_them() {
    let f = file();
    let cases = f["cases"].as_array().unwrap();
    let names: HashSet<&str> = cases.iter().filter_map(|c| c["name"].as_str()).collect();
    assert_eq!(names.len(), cases.len(), "two cases share a name");
    assert!(cases.len() >= 40, "every rule has a case");
    let kinds: HashSet<&str> = cases.iter().filter_map(|c| c["kind"].as_str()).collect();
    assert_eq!(kinds, KINDS.iter().copied().collect::<HashSet<_>>());
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
        for seed in SHUFFLES {
            match run(&f, c, seed) {
                Ok(got) if got == c["expect"] => {}
                Ok(got) => {
                    failed.push(format!(
                        "{} (shuffle {seed})\n  want {}\n  got  {got}",
                        c["name"], c["expect"]
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
        "emote mark: {}/{} cases pass under {} shuffles each",
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
    let ok = json!({ "offered": true, "mark": "", "kind": "everyone", "as": "chatter", "tier": "people" });
    assert!(
        err(json!({ "kind": "rank", "name": "x", "world": "mark", "expect": ok }))
            .contains("not one this runner understands")
    );
    assert!(err(json!({
        "kind": "tile", "name": "x", "world": "mark", "room": null, "who": "me",
        "tile": "chatter_bob", "tier": "people", "viewer": "me", "expect": ok
    }))
    .contains("field \"viewer\""));
    assert!(err(json!({
        "kind": "tile", "name": "x", "world": "mark", "room": null, "who": "me",
        "tile": "chatter_bob", "tier": "people", "expect": { "colour": 1 }
    }))
    .contains("field \"colour\""));
}

// ── what a tile's word sends decides, never a pick in the box ──

#[test]
fn a_pick_in_the_box_never_changes_the_answer() {
    let f = file();
    let c = f["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| {
            c["name"]
                .as_str()
                .is_some_and(|n| n.starts_with("nl_kripp has 7tv SusgeBinoculars"))
        })
        .expect("the case the test is built on");
    let raw = obj(&f["worlds"][c["world"].as_str().unwrap()], "world").unwrap();
    let w = build_world(&f, raw, &mut Rng(1)).unwrap();
    let tile = w.emote(c["tile"].as_str().unwrap()).unwrap();
    let tier = c["tier"].as_str().unwrap();
    let own = &w.people[c["who"].as_str().unwrap()];
    let room = w.room(c["room"].as_str()).unwrap();
    let base = SendCtx {
        room,
        legs: &[],
        own,
        globals: &w.globals,
        picks: None,
        fold: false,
    };
    let picks: Index<Em> = HashMap::from([(tile.name.clone(), tile.clone())]);
    let with = SendCtx {
        picks: Some(&picks),
        ..base.clone()
    };
    let (a, b) = (
        mark::tile_says(&base, tile, tier),
        mark::tile_says(&with, tile, tier),
    );
    assert_eq!(says_json(&a), says_json(&b));
    assert!(!a.offered);
}

// ── the classifier a readout shares ──

#[test]
fn names_the_audience_of_a_provider_where_it_sits_with_no_room() {
    let say = |p: &str, tier: Option<&str>| {
        let a = mark::audience(p, tier);
        (a.kind, a.mark.as_str(), a.as_)
    };
    assert_eq!(say("7tv", Some("room")), (Kind::Ext, "7", "7tv".into()));
    assert_eq!(
        say("twitch", Some("own")),
        (Kind::Everyone, "T", "twitch".into())
    );
    assert_eq!(
        say("inventory", Some("global")),
        (Kind::Heatsync, "H", "hs".into())
    );
    assert_eq!(
        say("7tv", Some("catalog")),
        (Kind::Heatsync, "H", "hs".into())
    );
    assert_eq!(say("7tv", Some("own")), (Kind::Heatsync, "H", "hs".into()));
    assert_eq!(say("", Some("room")), (Kind::Ext, "", "ext".into()));
    // a provider the table does not name is an addon's, with no letter
    assert_eq!(
        say("constructor", Some("room")),
        (Kind::Ext, "", "constructor".into())
    );
}

#[test]
fn a_mark_is_one_letter_or_none() {
    for (m, s) in [
        (Mark::Plain, ""),
        (Mark::T, "T"),
        (Mark::K, "K"),
        (Mark::Y, "Y"),
        (Mark::SevenTv, "7"),
        (Mark::Bttv, "B"),
        (Mark::Ffz, "F"),
        (Mark::H, "H"),
    ] {
        assert_eq!(m.as_str(), s);
        assert_eq!(m.letter().map(String::from).unwrap_or_default(), s);
    }
}

// ── one picture ──

fn em(id: &str, pic: Option<&str>, hash: Option<&str>) -> Em {
    Em {
        p: "hs".into(),
        id: id.into(),
        name: "x".into(),
        zw: false,
        pop: 0,
        pic: pic.map(String::from),
        hash: hash.map(String::from),
    }
}

#[test]
fn is_the_same_key_the_same_pic_or_the_same_upload_hash() {
    assert!(mark::same_picture(
        &em("a", None, None),
        &em("a", None, None)
    ));
    assert!(!mark::same_picture(
        &em("a", None, None),
        &em("b", None, None)
    ));
    assert!(mark::same_picture(
        &em("a", Some("p"), None),
        &em("b", Some("p"), None)
    ));
    // a picture named by `pic` is also the one whose own key it is
    assert!(mark::same_picture(
        &em("a", Some("hs:b"), None),
        &em("b", None, None)
    ));
    assert!(mark::same_picture(
        &em("a", None, Some("h")),
        &em("b", None, Some("h"))
    ));
    assert!(!mark::same_picture(
        &em("a", None, Some("")),
        &em("b", None, Some(""))
    ));
}
