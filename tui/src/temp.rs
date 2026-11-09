//! the temp tab: one unsaved channel tab for "the stream i'm looking at right
//! now", like the extension's live tab. opening from the picker or `/open` fills
//! it; `/keep` or `s` (or a send that lands) promotes it to a saved tab. it is
//! never persisted: `main::persisted` is the only door to disk and skips it.

use crate::app::App;
use crate::{input, parse_tab, save_state};
use heatsync_core::i18n::{tr, trf};
use heatsync_core::Platform;

fn find(app: &App) -> Option<usize> {
    app.channels.iter().position(|c| c.temp)
}

/// look at a channel. a tab that already covers it (saved or temp) is just
/// selected; otherwise it takes the temp slot, replacing any earlier temp.
pub(crate) fn open(app: &mut App, tok: &str) {
    let Some(sub) = parse_tab(tok).into_iter().next() else {
        return;
    };
    let (platform, name) = sub.clone();
    if let Some(i) = app.channels.iter().position(|c| c.matches(platform, &name)) {
        app.focus = i;
        return;
    }
    drop_temp(app);
    input::add_tab(app, &[sub], true);
}

/// forget the temp tab (leave its channel, remove the tab). nothing is saved.
fn drop_temp(app: &mut App) {
    let Some(i) = find(app) else { return };
    input::part_subs(&app.out, &app.channels[i]);
    app.channels.remove(i);
    app.emotes.remove(i);
    let last = app.channels.len().saturating_sub(1);
    if app.focus > i {
        app.focus -= 1;
    }
    if app.manage_cursor > i {
        app.manage_cursor -= 1;
    }
    app.focus = app.focus.min(last);
    app.manage_cursor = app.manage_cursor.min(last);
}

/// `/keep` / `s`: save the focused tab if it is the temp one.
pub(crate) fn keep(app: &mut App) {
    match app.channels.get_mut(app.focus) {
        Some(c) if c.temp => {
            c.temp = false;
            let name = c.name.clone();
            save_state(app);
            app.status = Some(trf("temp.kept", &[("name", &name)]));
        }
        _ => app.status = Some(tr("temp.nothing_to_keep").into()),
    }
}

/// a message to (platform, name) went out fine: a temp tab on it becomes saved.
pub(crate) fn sent(app: &mut App, platform: Platform, name: &str) {
    if let Some(c) = app
        .channels
        .iter_mut()
        .find(|c| c.temp && c.matches(platform, name))
    {
        c.temp = false;
        save_state(app);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::special::tests::app;
    use crate::{palette, persisted};
    use heatsync_core::{Channel, Platform};
    use ratatui::style::Modifier;

    fn names(a: &App) -> Vec<(String, bool)> {
        a.channels
            .iter()
            .map(|c| (c.name.clone(), c.temp))
            .collect()
    }

    #[test]
    fn open_makes_a_temp_tab_and_focuses_it() {
        let mut a = app();
        open(&mut a, "forsen");
        assert_eq!(a.channels.len(), 3);
        assert!(a.channels[2].temp);
        assert_eq!(a.focus, 2);
        assert_eq!(a.emotes.len(), 3);
    }

    #[test]
    fn a_second_open_replaces_the_first() {
        let mut a = app();
        open(&mut a, "forsen");
        open(&mut a, "kick:trainwreckstv");
        assert_eq!(a.channels.len(), 3);
        assert_eq!(a.emotes.len(), 3);
        assert_eq!(a.channels.iter().filter(|c| c.temp).count(), 1);
        assert_eq!(a.channels[2].name, "trainwreckstv");
        assert_eq!(a.channels[2].platform, Platform::Kick);
        assert_eq!(a.focus, 2);
    }

    #[test]
    fn replacing_keeps_focus_on_the_same_tab() {
        let mut a = app();
        open(&mut a, "forsen");
        a.focus = 0;
        open(&mut a, "nl_kripp"); // new temp is focused, as any open is
        assert_eq!(names(&a)[2], ("nl_kripp".into(), true));
        a.focus = 1;
        a.manage_cursor = 2;
        open(&mut a, "xqc"); // covered: only selects
        assert_eq!(a.focus, 0);
        assert_eq!(a.channels.len(), 3);
    }

    #[test]
    fn a_covered_channel_selects_the_saved_tab_and_makes_no_temp() {
        let mut a = app();
        open(&mut a, "kick:kripp");
        assert_eq!(a.focus, 1);
        assert_eq!(a.channels.len(), 2);
        assert!(a.channels.iter().all(|c| !c.temp));
        // same name, other platform, is a different channel
        open(&mut a, "kripp");
        assert_eq!(a.channels.len(), 3);
        // the temp itself, opened again, is just selected
        a.focus = 0;
        open(&mut a, "kripp");
        assert_eq!((a.focus, a.channels.len()), (2, 3));
    }

    #[test]
    fn keep_saves_it_and_persists() {
        let mut a = app();
        open(&mut a, "forsen");
        keep(&mut a);
        assert!(!a.channels[2].temp);
        assert_eq!(a.status.as_deref(), Some("kept forsen"));
        assert_eq!(persisted(&a).channels.len(), 3);
        // the slot is free again: the next open adds a fresh temp
        open(&mut a, "nl_kripp");
        assert_eq!(a.channels.len(), 4);
        keep(&mut a);
        keep(&mut a);
        assert_eq!(
            a.status.as_deref(),
            Some("nothing to keep — open a stream with o or /open")
        );
    }

    #[test]
    fn an_explicit_add_of_the_temp_channel_keeps_it() {
        let mut a = app();
        open(&mut a, "forsen");
        crate::input::open_channel(&mut a, "forsen");
        assert_eq!(a.channels.len(), 3);
        assert!(!a.channels[2].temp);
    }

    #[test]
    fn a_landed_send_keeps_and_other_sends_do_not() {
        let mut a = app();
        open(&mut a, "forsen");
        sent(&mut a, Platform::Twitch, "xqc"); // a send elsewhere
        assert!(a.channels[2].temp);
        sent(&mut a, Platform::Kick, "forsen"); // wrong platform
        assert!(a.channels[2].temp);
        sent(&mut a, Platform::Twitch, "FORSEN");
        assert!(!a.channels[2].temp);
    }

    #[test]
    fn a_failed_hs_send_does_not_keep_but_a_good_one_does() {
        use crate::hsauth::Note;
        let mut a = app();
        open(&mut a, "forsen");
        let note = |sent, text: &str| Note {
            platform: Platform::Twitch,
            channel: "forsen".into(),
            text: text.into(),
            sent,
            logged_out: false,
        };
        a.hs_tx.send(note(false, "nope")).unwrap();
        crate::feed::drain_hs(&mut a);
        assert!(a.channels[2].temp);
        a.hs_tx.send(note(true, "")).unwrap();
        crate::feed::drain_hs(&mut a);
        assert!(!a.channels[2].temp);
    }

    #[test]
    fn the_temp_tab_never_reaches_the_save_file_or_a_share_link() {
        let mut a = app();
        open(&mut a, "forsen");
        let saved = persisted(&a).channels;
        assert_eq!(saved.len(), 2);
        assert!(saved.iter().flatten().all(|(_, n)| n != "forsen"));
        a.focus = 0;
        crate::input::handle_key(
            &mut a,
            crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Char('s')),
        );
        assert_eq!(
            a.status.as_deref(),
            Some("nothing to keep — open a stream with o or /open")
        );
        assert_eq!(persisted(&a).channels.len(), 2);
    }

    #[test]
    fn closing_it_just_drops_it() {
        let mut a = app();
        open(&mut a, "forsen");
        crate::input::handle_key(
            &mut a,
            crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Char('x')),
        );
        assert_eq!(a.channels.len(), 2);
        assert!(a.channels.iter().all(|c| !c.temp));
    }

    #[test]
    fn the_label_is_underlined() {
        use ratatui::backend::TestBackend;
        let mut a = app();
        open(&mut a, "forsen");
        a.focus = 0;
        let mut t = ratatui::Terminal::new(TestBackend::new(60, 1)).unwrap();
        t.draw(|f| crate::draw::draw_tabs(f, f.area(), &a)).unwrap();
        let buf = t.backend().buffer().clone();
        let row: String = (0..60).map(|x| buf[(x, 0)].symbol()).collect();
        let x = row.find("forsen").unwrap() as u16;
        assert!(buf[(x, 0)].modifier.contains(Modifier::UNDERLINED));
        let xq = row.find("xqc").unwrap() as u16;
        assert!(!buf[(xq, 0)].modifier.contains(Modifier::UNDERLINED));
        let _ = (palette::TAG, Channel::new("", Platform::Twitch, 1));
    }
}
