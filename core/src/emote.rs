//! emote resolution. mirrors the server: /api/channel/:c/emotes returns a
//! precedence-ordered (7tv > bttv > ffz > twitch > kick) deduped list, so we
//! insert first-wins into a name→emote map and word-match message text against
//! it. modifiers (BTTV `!` / FFZ effects) are a later pixel-op pass.

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

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// a rendered chunk of a message: literal text, or a resolved emote.
#[derive(Clone, Debug, PartialEq)]
pub enum Token {
    Text(String),
    Emote(String), // emote name; resolve via EmoteSet::get for the url
}

/// split message text into text + emote tokens by whole-word match. adjacent
/// text words coalesce (single-spaced). input is assumed already sanitized.
pub fn tokenize(content: &str, set: &EmoteSet) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::new();
    let mut text = String::new();
    for word in content.split_whitespace() {
        if set.get(word).is_some() {
            if !text.is_empty() {
                out.push(Token::Text(std::mem::take(&mut text)));
            }
            out.push(Token::Emote(word.to_string()));
        } else {
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(word);
        }
    }
    if !text.is_empty() {
        out.push(Token::Text(text));
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
        }
    }

    #[test]
    fn first_insert_wins_on_collision() {
        let set = EmoteSet::from_list([
            Emote { provider: "7tv".into(), ..e("Kappa") },
            Emote { provider: "bttv".into(), ..e("Kappa") },
        ]);
        assert_eq!(set.get("Kappa").unwrap().provider, "7tv");
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn tokenizes_text_and_emotes() {
        let set = EmoteSet::from_list([e("KEKW"), e("GAMBA")]);
        let toks = tokenize("lol KEKW that GAMBA moment", &set);
        assert_eq!(
            toks,
            vec![
                Token::Text("lol".into()),
                Token::Emote("KEKW".into()),
                Token::Text("that".into()),
                Token::Emote("GAMBA".into()),
                Token::Text("moment".into()),
            ]
        );
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
