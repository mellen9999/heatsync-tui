//! per-emote blocks from the keyboard: `/block <emote>`, `/unblock <emote>`,
//! `/blocked`, and `b` in the message cursor. hides the EXACT emote (keyed by
//! its identity, see core's blockid module), never the name. kept in the config; when
//! logged in the account is told too, so the extension and chatterino agree.

use crate::app::App;
use crate::{config, hsauth, save_state};
use heatsync_core::block;
use heatsync_core::emote::{emotes_in, Emote, EmoteSet};
use heatsync_core::i18n::{tr, trf};
use unicode_width::UnicodeWidthStr;

/// the emote this name means in the focused tab (its channel set + globals).
fn resolve(app: &App, name: &str) -> Option<Emote> {
    app.emotes.get(app.focus)?.get(name).cloned()
}

/// tell the account, off the UI thread. a failure is said out loud: the block
/// still works here, it just won't follow you to other devices.
fn sync(app: &App, key: String, on: bool) {
    let (Some(client), Some(ch)) = (app.hs.clone(), app.channels.get(app.focus)) else {
        return;
    };
    hsauth::spawn_note(app.hs_tx.clone(), ch.platform, ch.name.clone(), move || {
        let o = client.set_emote_block(&key, on);
        if o.ok {
            o
        } else {
            hsauth::Outcome {
                ok: false,
                text: trf("input.blocks_account_miss", &[("text", &o.text)]),
            }
        }
    });
}

/// block this exact emote; the words to show.
fn block_one(app: &mut App, e: &Emote) -> String {
    let Some(key) = block::key(e) else {
        return trf("input.blocks_too_long", &[("name", &e.name)]);
    };
    if !block::block(e) {
        return trf("input.blocks_already", &[("name", &e.name)]);
    }
    save_state(app);
    sync(app, key, true);
    trf("input.blocks_done", &[("name", &e.name)])
}

/// `/block <emote>`
pub(crate) fn block_cmd(app: &mut App, name: &str) {
    app.status = Some(match resolve(app, name) {
        Some(e) => block_one(app, &e),
        None => trf("input.blocks_no_emote", &[("name", name)]),
    });
}

/// `/unblock <emote>` — by the name it was blocked under, else by what the name
/// means in this tab now.
pub(crate) fn unblock_cmd(app: &mut App, name: &str) {
    let mut keys = block::keys_named(name);
    if keys.is_empty() {
        keys.extend(
            resolve(app, name)
                .map(|e| block::held(&e))
                .unwrap_or_default(),
        );
    }
    if keys.is_empty() {
        app.status = Some(trf("input.blocks_not_blocked", &[("name", name)]));
        return;
    }
    for k in keys {
        block::unblock_key(&k);
        sync(app, k, false);
    }
    save_state(app);
    app.status = Some(trf("input.blocks_unblocked", &[("name", name)]));
}

/// `/blocked`
pub(crate) fn list_cmd(app: &mut App) {
    app.status = Some(summary(&block::list()));
}

pub(crate) fn summary(list: &[(String, String)]) -> String {
    if list.is_empty() {
        return tr("input.blocks_none").into();
    }
    let names: Vec<String> = list
        .iter()
        .map(|(k, n)| {
            if n.is_empty() {
                format!("({})", block::describe(k))
            } else {
                n.clone()
            }
        })
        .collect();
    let mut out = trf("input.blocks_list_head", &[("n", &list.len().to_string())]);
    out.push(' ');
    for (i, n) in names.iter().enumerate() {
        if out.width() + n.width() > 70 {
            out.push_str(&format!("… +{}", names.len() - i));
            return out;
        }
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(n);
    }
    out
}

/// `b` on a message in cursor mode: block the first emote on it that isn't
/// blocked yet (press again for the next one).
pub(crate) fn cursor_block(app: &mut App, text: &str, refs: &EmoteSet) {
    let Some(set) = app.emotes.get(app.focus) else {
        return;
    };
    let pick = emotes_in(text, set, refs)
        .into_iter()
        .find(|e| !block::is_blocked(e))
        .cloned();
    app.status = Some(match pick {
        Some(e) => block_one(app, &e),
        None => tr("input.blocks_none_left").into(),
    });
}

/// at startup, when logged in: fold the account's blocks into the local set and
/// keep them. offline or logged out, the saved set still applies.
pub(crate) fn pull(app: &App) {
    let Some(client) = app.hs.clone() else { return };
    std::thread::spawn(move || {
        if let Ok(keys) = client.emote_blocks() {
            if block::merge_keys(keys) > 0 {
                config::save_blocks(&block::list());
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_names_them_and_truncates() {
        assert!(summary(&[]).starts_with("nothing blocked"));
        let two = vec![("k1".into(), "Kappa".into()), ("k2".into(), String::new())];
        let s = summary(&two);
        assert!(s.starts_with("blocked 2: Kappa, ("), "{s}");
        let many: Vec<_> = (0..40)
            .map(|i| (format!("k{i}"), format!("Emote{i}")))
            .collect();
        let s = summary(&many);
        assert!(s.chars().count() < 90 && s.contains("… +"), "{s}");
    }
}
