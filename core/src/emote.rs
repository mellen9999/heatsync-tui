//! emote resolution. mirrors the server: /api/channel/:c/emotes returns a
//! precedence-ordered (7tv > bttv > ffz > twitch > kick) deduped list, so we
//! insert first-wins into a name→emote map and word-match message text against
//! it. modifiers (BTTV `w!`-style prefixes / FFZ `ffzX`-style effect words) are
//! parsed here into per-stack [`Effect`] lists; the pixel work happens at render.

use std::collections::HashMap;

use serde::Deserialize;

/// one emote as the HeatSync emote API returns it.
#[derive(Clone, Debug, Deserialize)]
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
}

/// the /api/channel/:c/emotes envelope (only the bits we need).
#[derive(Debug, Deserialize)]
pub struct EmoteSetResponse {
    #[serde(default)]
    pub emotes: Vec<Emote>,
}

/// name → emote lookup, precedence-respecting (first insert wins).
#[derive(Clone, Debug, Default)]
pub struct EmoteSet {
    map: HashMap<String, Emote>,
}

impl EmoteSet {
    pub fn new() -> EmoteSet {
        EmoteSet::default()
    }

    /// build from an API list. the server already ordered by precedence and
    /// deduped, but we defend with first-wins so a stray dup can't override.
    pub fn from_list(list: impl IntoIterator<Item = Emote>) -> EmoteSet {
        let mut set = EmoteSet::new();
        for e in list {
            set.map.entry(e.name.clone()).or_insert(e);
        }
        set
    }

    pub fn get(&self, name: &str) -> Option<&Emote> {
        self.map.get(name)
    }

    /// all emote names (unordered) — completion feeds from this.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.map.keys().map(String::as_str)
    }

    /// all emotes (unordered) — completion needs name + provider for badges.
    pub fn iter(&self) -> impl Iterator<Item = &Emote> {
        self.map.values()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// a pixel effect on an emote stack. BTTV prefix modifiers and FFZ effect words
/// share this vocabulary; the render layer applies them in enum order (geometry
/// → color → motion), so derived Ord IS the application order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Effect {
    RotateL, // l!
    RotateR, // r!
    FlipX,   // h! / ffzX
    FlipY,   // v! / ffzY
    Wide,    // w! / ffzW — stack renders double-width
    Cursed,  // c! / ffzCursed — grayscale + darken
    Rainbow, // p! / ffzRainbow — hue cycle (animates even static emotes)
    Shake,   // s! / ffzHyper — jitter (animates even static emotes)
}

impl Effect {
    /// one-letter code used in stack keys (stable — keys are cache identities).
    pub fn code(self) -> char {
        match self {
            Effect::RotateL => 'l',
            Effect::RotateR => 'r',
            Effect::FlipX => 'x',
            Effect::FlipY => 'y',
            Effect::Wide => 'w',
            Effect::Cursed => 'c',
            Effect::Rainbow => 'p',
            Effect::Shake => 's',
        }
    }

    fn from_code(c: char) -> Option<Effect> {
        Some(match c {
            'l' => Effect::RotateL,
            'r' => Effect::RotateR,
            'x' => Effect::FlipX,
            'y' => Effect::FlipY,
            'w' => Effect::Wide,
            'c' => Effect::Cursed,
            'p' => Effect::Rainbow,
            's' => Effect::Shake,
            _ => return None,
        })
    }

    /// BTTV `X!` prefix letter → effect. `z` (zero-width) is handled separately —
    /// it changes stacking, not pixels.
    fn from_bttv(c: char) -> Option<Effect> {
        Some(match c {
            'w' => Effect::Wide,
            'h' => Effect::FlipX,
            'v' => Effect::FlipY,
            'c' => Effect::Cursed,
            'l' => Effect::RotateL,
            'r' => Effect::RotateR,
            'p' => Effect::Rainbow,
            's' => Effect::Shake,
            _ => return None,
        })
    }

    /// FFZ effect word (typed AFTER the emote it modifies) → effect.
    fn from_ffz(word: &str) -> Option<Effect> {
        Some(match word {
            "ffzX" => Effect::FlipX,
            "ffzY" => Effect::FlipY,
            "ffzW" => Effect::Wide,
            "ffzCursed" => Effect::Cursed,
            "ffzRainbow" => Effect::Rainbow,
            "ffzHyper" => Effect::Shake,
            _ => return None,
        })
    }
}

/// a renderable emote stack: base emote + zero-width overlays + effects. the
/// key doubles as the render cache identity — layer urls joined by '\n', with
/// effects (if any) as a final `#codes` pseudo-layer ('\n' and '#'-first lines
/// can't appear in a real url, so the parse back is unambiguous).
#[derive(Clone, Debug, PartialEq)]
pub struct Stack {
    pub base: String, // base emote name — the text fallback while loading
    pub urls: Vec<String>,
    pub fx: Vec<Effect>, // sorted + deduped (stable keys)
}

impl Stack {
    fn new(base: &str, url: String) -> Stack {
        Stack {
            base: base.to_string(),
            urls: vec![url],
            fx: Vec::new(),
        }
    }

    fn add_fx(&mut self, fx: Effect) {
        if let Err(i) = self.fx.binary_search(&fx) {
            self.fx.insert(i, fx);
        }
    }

    pub fn wide(&self) -> bool {
        self.fx.contains(&Effect::Wide)
    }

    pub fn key(&self) -> String {
        let mut k = self.urls.join("\n");
        if !self.fx.is_empty() {
            k.push_str("\n#");
            k.extend(self.fx.iter().map(|f| f.code()));
        }
        k
    }
}

/// split a stack key back into layer urls + effects (render side).
pub fn split_key(key: &str) -> (Vec<&str>, Vec<Effect>) {
    let mut urls = Vec::new();
    let mut fx = Vec::new();
    for line in key.split('\n') {
        if let Some(codes) = line.strip_prefix('#') {
            fx.extend(codes.chars().filter_map(Effect::from_code));
        } else {
            urls.push(line);
        }
    }
    (urls, fx)
}

/// a laid-out chunk of a message: literal text, or an emote stack.
#[derive(Clone, Debug, PartialEq)]
pub enum Segment {
    Text(String),
    Stack(Stack),
}

/// strip chained BTTV prefixes (`w!h!Emote`) off a word. returns the collected
/// effects, whether `z!` (force zero-width) was present, and the remainder.
fn strip_bttv(word: &str) -> (Vec<Effect>, bool, &str) {
    let mut fx = Vec::new();
    let mut zw = false;
    let mut rest = word;
    loop {
        let mut it = rest.chars();
        match (it.next(), it.next()) {
            (Some(c), Some('!')) if c == 'z' || Effect::from_bttv(c).is_some() => {
                if c == 'z' {
                    zw = true;
                } else {
                    fx.push(Effect::from_bttv(c).unwrap());
                }
                rest = &rest[2..];
            }
            _ => return (fx, zw, rest),
        }
    }
}

/// parse message text into text + emote-stack segments. handles:
///   - zero-width emotes overlaying the preceding base (`GAMBA notL`)
///   - BTTV modifiers, bare (`w! KEKW`) and attached (`w!KEKW`, chains ok)
///   - `z!` forcing any emote to overlay the preceding stack
///   - FFZ effect words after the emote (`KEKW ffzX ffzRainbow`)
///
/// modifier words that never find an emote degrade to literal text. adjacent
/// text words coalesce single-spaced. input is assumed already sanitized.
pub fn segments(content: &str, set: &EmoteSet) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    let mut text = String::new();
    // bare modifiers waiting for their emote, with the literal words to restore
    // if the next word turns out to be text.
    let mut pending: Vec<(Vec<Effect>, bool, String)> = Vec::new();

    fn push_text(text: &mut String, w: &str) {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(w);
    }
    fn flush_pending(text: &mut String, pending: &mut Vec<(Vec<Effect>, bool, String)>) {
        for (_, _, w) in pending.drain(..) {
            push_text(text, &w);
        }
    }
    fn flush_text(out: &mut Vec<Segment>, text: &mut String) {
        if !text.is_empty() {
            out.push(Segment::Text(std::mem::take(text)));
        }
    }

    for word in content.split_whitespace() {
        // FFZ effect word: applies to the immediately-preceding stack (nothing
        // may sit between them — no text, no unconsumed modifiers).
        if let Some(fx) = Effect::from_ffz(word) {
            if text.is_empty() && pending.is_empty() {
                if let Some(Segment::Stack(s)) = out.last_mut() {
                    s.add_fx(fx);
                    continue;
                }
            }
            flush_pending(&mut text, &mut pending);
            push_text(&mut text, word);
            continue;
        }

        // exact name wins over prefix parsing (an emote could be NAMED "h!x").
        let (fx, zw, rest) = if set.get(word).is_some() {
            (Vec::new(), false, word)
        } else {
            strip_bttv(word)
        };
        if rest.is_empty() && (!fx.is_empty() || zw) {
            // bare modifier(s) — hold for the next emote.
            pending.push((fx, zw, word.to_string()));
            continue;
        }
        match set.get(rest) {
            Some(e) => {
                let mut all_fx = fx;
                let mut force_zw = zw;
                for (pfx, pzw, _) in pending.drain(..) {
                    all_fx.extend(pfx);
                    force_zw |= pzw;
                }
                let overlay = e.zero_width || force_zw;
                // an overlay with NO effects of its own joins the previous
                // stack; effects always start a fresh stack (they apply to a
                // whole composite, and a per-layer op would corrupt the base).
                if overlay && all_fx.is_empty() && text.is_empty() {
                    if let Some(Segment::Stack(s)) = out.last_mut() {
                        s.urls.push(e.url.clone());
                        continue;
                    }
                }
                flush_text(&mut out, &mut text);
                let mut s = Stack::new(rest, e.url.clone());
                for f in all_fx {
                    s.add_fx(f);
                }
                out.push(Segment::Stack(s));
            }
            None => {
                flush_pending(&mut text, &mut pending);
                push_text(&mut text, word);
            }
        }
    }
    flush_pending(&mut text, &mut pending);
    flush_text(&mut out, &mut text);
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
        }
    }

    #[test]
    fn first_insert_wins_on_collision() {
        let set = EmoteSet::from_list([
            Emote {
                provider: "7tv".into(),
                ..e("Kappa")
            },
            Emote {
                provider: "bttv".into(),
                ..e("Kappa")
            },
        ]);
        assert_eq!(set.get("Kappa").unwrap().provider, "7tv");
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn segments_text_and_emotes() {
        let set = EmoteSet::from_list([e("KEKW"), e("GAMBA")]);
        let segs = segments("lol KEKW that GAMBA moment", &set);
        assert_eq!(
            segs,
            vec![
                Segment::Text("lol".into()),
                Segment::Stack(Stack::new("KEKW", "https://cdn/KEKW.webp".into())),
                Segment::Text("that".into()),
                Segment::Stack(Stack::new("GAMBA", "https://cdn/GAMBA.webp".into())),
                Segment::Text("moment".into()),
            ]
        );
    }

    fn zw(name: &str) -> Emote {
        Emote {
            zero_width: true,
            ..e(name)
        }
    }

    fn stacks(msg: &str, set: &EmoteSet) -> Vec<Stack> {
        segments(msg, set)
            .into_iter()
            .filter_map(|s| match s {
                Segment::Stack(s) => Some(s),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn zero_width_overlays_previous_base() {
        let set = EmoteSet::from_list([e("GAMBA"), zw("notL")]);
        let s = stacks("GAMBA notL", &set);
        assert_eq!(s.len(), 1);
        assert_eq!(
            s[0].urls,
            vec!["https://cdn/GAMBA.webp", "https://cdn/notL.webp"]
        );
        assert_eq!(s[0].key(), "https://cdn/GAMBA.webp\nhttps://cdn/notL.webp");
    }

    #[test]
    fn bttv_attached_prefix_chains() {
        let set = EmoteSet::from_list([e("KEKW")]);
        let s = stacks("w!h!KEKW", &set);
        assert_eq!(s[0].fx, vec![Effect::FlipX, Effect::Wide]); // sorted apply order
        assert!(s[0].wide());
        assert_eq!(s[0].key(), "https://cdn/KEKW.webp\n#xw");
    }

    #[test]
    fn bttv_bare_prefix_applies_to_next_emote() {
        let set = EmoteSet::from_list([e("KEKW")]);
        let s = stacks("w! KEKW", &set);
        assert_eq!(s[0].fx, vec![Effect::Wide]);
    }

    #[test]
    fn unused_modifier_degrades_to_text() {
        let set = EmoteSet::from_list([e("KEKW")]);
        let segs = segments("w! nothing here", &set);
        assert_eq!(segs, vec![Segment::Text("w! nothing here".into())]);
    }

    #[test]
    fn z_forces_overlay() {
        let set = EmoteSet::from_list([e("GAMBA"), e("KEKW")]);
        let s = stacks("GAMBA z!KEKW", &set);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].urls.len(), 2);
    }

    #[test]
    fn ffz_effect_words_attach_to_previous_stack() {
        let set = EmoteSet::from_list([e("KEKW")]);
        let s = stacks("KEKW ffzX ffzRainbow", &set);
        assert_eq!(s[0].fx, vec![Effect::FlipX, Effect::Rainbow]);
    }

    #[test]
    fn ffz_effect_needs_adjacency() {
        let set = EmoteSet::from_list([e("KEKW")]);
        let segs = segments("KEKW lol ffzX", &set);
        assert_eq!(
            segs,
            vec![
                Segment::Stack(Stack::new("KEKW", "https://cdn/KEKW.webp".into())),
                Segment::Text("lol ffzX".into()),
            ]
        );
    }

    #[test]
    fn exact_emote_name_beats_prefix_parse() {
        let set = EmoteSet::from_list([e("h!i")]);
        let s = stacks("h!i", &set);
        assert_eq!(s[0].base, "h!i");
        assert!(s[0].fx.is_empty());
    }

    #[test]
    fn key_roundtrips_through_split_key() {
        let set = EmoteSet::from_list([e("GAMBA"), zw("notL")]);
        let s = stacks("w!GAMBA notL ffzCursed", &set);
        let key = s[0].key();
        let (urls, fx) = split_key(&key);
        assert_eq!(
            urls,
            vec!["https://cdn/GAMBA.webp", "https://cdn/notL.webp"]
        );
        assert_eq!(fx, vec![Effect::Wide, Effect::Cursed]);
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
}
