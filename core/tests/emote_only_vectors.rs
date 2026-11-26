//! runs core/tests/emote-only.vectors.json against heatsync_core::only.
//!
//! the file is the site's contract for "does this line draw its pictures big",
//! vendored byte for byte (see scripts/sync-emote-vectors.sh). every case runs;
//! a kind or a field this runner does not know is a failure, never a skip: a
//! client that silently ignored a new rule would pass while breaking it.
//!
//! the second half runs each case's `line` through the terminal's own drawing
//! path (`segments_with`, the same call the layout makes) and asks the verdict
//! of what that drew, so the terminal is held to the rule on real lines and
//! not only on the file's pre-read `read` lists.

use heatsync_core::emote::{self, Emote, EmoteSet, RefKind};
use heatsync_core::only::{self, emote_only, Kind, Verdict};
use serde_json::{json, Map, Value};
use std::collections::HashSet;

mod common;
use common::sha256;

const FILE: &str = include_str!("emote-only.vectors.json");

fn file() -> Value {
    serde_json::from_str(FILE).expect("the vector file is json")
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

fn kind(s: &Value) -> Res<Kind> {
    match s.as_str() {
        Some("emote") => Ok(Kind::Emote),
        Some("emoji") => Ok(Kind::Emoji),
        Some("overlay") => Ok(Kind::Overlay),
        Some("mod") => Ok(Kind::Mod),
        Some("text") => Ok(Kind::Text),
        _ => Err(format!("kind {s} is not one this runner understands")),
    }
}

/// one case: the file's read, run through the rule, against its want and pictures.
fn run(c: &Value) -> Res<()> {
    let c = c.as_object().ok_or("a case is an object")?;
    known(c, &["name", "line", "read", "want", "pictures"], "case")?;
    let name = c["name"].as_str().ok_or("name")?;
    let read: Vec<Kind> = c["read"]
        .as_array()
        .ok_or("read")?
        .iter()
        .map(kind)
        .collect::<Res<_>>()
        .map_err(|e| format!("{name}: {e}"))?;
    let want = match c["want"].as_str() {
        Some("big") => true,
        Some("normal") => false,
        _ => return Err(format!("{name}: want is big or normal")),
    };
    let pictures = c["pictures"].as_u64().ok_or("pictures")? as usize;
    let got = emote_only(&read);
    if got
        != (Verdict {
            big: want,
            pictures,
        })
    {
        return Err(format!(
            "{name}: want big={want} pictures={pictures}, got big={} pictures={}",
            got.big, got.pictures
        ));
    }
    Ok(())
}

#[test]
fn the_vendored_file_has_a_known_hash() {
    // print it, so it can be compared with the site's `sha256sum shared/emote-only.vectors.json`
    let got = sha256(FILE.as_bytes());
    println!("emote-only.vectors.json sha256 {got}");
    // the pinned hash is written by scripts/sync-emote-vectors.sh from the site's
    // file: a hand-edited copy of the vectors no longer matches it
    assert_eq!(
        got,
        include_str!("emote-only.vectors.sha256").trim(),
        "the vendored vectors were edited: refresh them with scripts/sync-emote-vectors.sh"
    );
}

#[test]
fn the_constants_are_the_files_rules() {
    let f = file();
    let rules = f["rules"].as_object().expect("rules");
    known(rules, &["max", "scale"], "rules").unwrap();
    assert_eq!(rules["max"].as_u64(), Some(only::BIG_MAX as u64));
    assert_eq!(rules["scale"].as_u64(), Some(u64::from(only::BIG_SCALE)));
}

#[test]
fn the_file_has_only_fields_this_runner_understands() {
    let f = file();
    known(
        f.as_object().unwrap(),
        &["about", "rules", "cases"],
        "the vector file",
    )
    .unwrap();
    let cases = f["cases"].as_array().unwrap();
    let names: HashSet<&str> = cases.iter().filter_map(|c| c["name"].as_str()).collect();
    assert_eq!(names.len(), cases.len(), "two cases share a name");
}

#[test]
fn every_case_gives_its_want_and_pictures() {
    let f = file();
    let cases = f["cases"].as_array().unwrap();
    let failed: Vec<String> = cases.iter().filter_map(|c| run(c).err()).collect();
    println!(
        "emote-only: {}/{} cases pass",
        cases.len() - failed.len(),
        cases.len()
    );
    assert_eq!(cases.len(), 49, "the file's case count moved: re-read it");
    assert!(
        failed.is_empty(),
        "{} case(s) failed:\n{}",
        failed.len(),
        failed.join("\n")
    );
}

#[test]
fn refuses_what_it_does_not_understand_instead_of_skipping_it() {
    let e = |c: Value| run(&c).unwrap_err();
    let ok = json!({"name":"x","line":"Kappa","read":["emote"],"want":"big","pictures":1});
    assert!(run(&ok).is_ok());
    let mut bad_kind = ok.clone();
    bad_kind["read"] = json!(["sticker"]);
    assert!(e(bad_kind).contains("is not one this runner understands"));
    let mut bad_field = ok.clone();
    bad_field["colour"] = json!(1);
    assert!(e(bad_field).contains("field \"colour\""));
    let mut bad_want = ok;
    bad_want["want"] = json!("huge");
    assert!(e(bad_want).contains("want is big or normal"));
}

// ── the terminal's own drawing path ─────────────────────────────────────────

fn em(name: &str, zero_width: bool, kind: RefKind) -> Emote {
    Emote {
        name: name.into(),
        url: format!("u/{name}"),
        provider: "7tv".into(),
        id: name.into(),
        animated: false,
        zero_width,
        global: false,
        kind,
    }
}

/// a room that knows the file's names, as the terminal draws it. `gone` are the
/// words this case reads as text although the room (or the line) names an emote
/// of that word: a taken-down pick.
fn world(gone: &[&str]) -> EmoteSet {
    let mut list: Vec<Emote> = [
        "Kappa", "Keepo", "PogU", "KEKW", "OMEGALUL", "LUL", "MyEmote",
    ]
    .iter()
    .filter(|n| !gone.contains(n))
    .map(|n| em(n, false, RefKind::Sender))
    .collect();
    list.extend(
        ["RainTime", "SnowFall", "Glitter"]
            .iter()
            .map(|n| em(n, true, RefKind::Sender)),
    );
    EmoteSet::from_list(list)
}

/// the verdict of what the terminal draws for `line`: its real read, then the rule.
fn drawn(line: &str, gone: &[&str]) -> Verdict {
    let set = world(gone);
    // a kick token and a youtube emoji ride the line as refs, the way the relay and
    // the platform hand them over; everything else is a word in the room
    let (text, mut refs) = emote::kick_native(line);
    if text.contains(":hand-pink-waving:") {
        refs.push(em(":hand-pink-waving:", false, RefKind::Pick));
    }
    let mut picks: Vec<Emote> = gone.iter().filter_map(|n| Emote::gone(n)).collect();
    picks.extend(refs);
    only::verdict_of(&emote::segments_with(&text, &set, &emote::ref_set(&picks)))
}

/// cases the terminal reads differently from the file, by name, with why. exact,
/// so a quietly growing list fails.
const READS_DIFFERENTLY: [&str; 1] = [
    // a modifier binds to a picture the terminal can change; an emoji is a text cell it
    // cannot, so `p!` stays a word on the line and the line stays normal
    "a modifier beside an emoji is not counted",
];

#[test]
fn the_terminals_drawing_path_agrees_on_every_line_it_can_say() {
    let f = file();
    let (mut said, mut differ) = (0, Vec::new());
    for c in f["cases"].as_array().unwrap() {
        let name = c["name"].as_str().unwrap();
        let line = c["line"].as_str().unwrap();
        let reads = c["read"].as_array().unwrap();
        let words: Vec<&str> = line.split_whitespace().collect();
        // a word the case reads as text that the room would draw is a taken-down pick:
        // only where the words and the read line up one to one can that be told
        let gone: Vec<&str> = if words.len() == reads.len() {
            words
                .iter()
                .zip(reads)
                .filter(|(w, r)| r.as_str() == Some("text") && world(&[]).get(w).is_some())
                .map(|(w, _)| *w)
                .collect()
        } else {
            Vec::new()
        };
        let want = Verdict {
            big: c["want"].as_str() == Some("big"),
            pictures: c["pictures"].as_u64().unwrap() as usize,
        };
        if drawn(line, &gone) == want {
            said += 1;
        } else {
            differ.push(name);
        }
    }
    assert_eq!(
        differ, READS_DIFFERENTLY,
        "the lines the terminal reads differently moved"
    );
    println!(
        "terminal drawing: {said} lines agree, {} read differently",
        differ.len()
    );
    assert_eq!(said, 48);
}
