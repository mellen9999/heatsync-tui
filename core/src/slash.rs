//! slash commands typed in the composer, the way every chat client does it.
//!
//! the rule that matters: we only claim commands a CLIENT owns — joining,
//! parting, quitting, and the heatsync-only /w /dm /op /re (twitch dropped
//! chat-line whispers, so `/w` has nothing else to mean). everything else is the platform's (`/ban`, `/timeout`,
//! `/me`, `/vip`, `/raid` …) and goes out verbatim, so moderating from here
//! behaves exactly like typing into twitch or kick directly. claiming a name
//! the platform also uses would silently break a mod action, which is why the
//! list below is short and stays short.

/// what a line typed into the composer turns out to be.
#[derive(Debug, PartialEq)]
pub enum Cmd {
    Join(String),
    /// leave a named channel, or the focused one when `None`.
    Part(Option<String>),
    Quit,
    /// open (or focus) a virtual tab: `/mentions`, `/feed`, `/whispers`.
    Tab(crate::TabKind),
    /// `/w <user> <text>` — a twitch whisper through heatsync.
    Whisper(String, String),
    /// `/dm <user> <text>` — a heatsync direct message.
    Dm(String, String),
    /// `/op <text>` — post to the heatsync feed.
    Op(String),
    /// `/re <text>` — reply to the last OP you posted.
    Re(String),
    /// `/sharelayout` (`/sl`, `/share`) — copy the open channels as a heatsync.org/m/ link.
    Share,
    /// `/layout <link>` — open the channels in a share link.
    Layout(String),
    /// send these bytes to the platform as an ordinary message.
    Send(String),
    /// a client command we recognise but that was typed without its argument.
    Usage(&'static str),
}

/// parse one composer line. anything that isn't ours comes back as `Send`, so
/// platform commands are never swallowed.
pub fn parse(input: &str) -> Cmd {
    let line = input.trim();
    // `//x` escapes: sends a literal `/x`, the only way to start a message with
    // a slash. same convention chatterino uses.
    if let Some(rest) = line.strip_prefix("//") {
        return Cmd::Send(format!("/{rest}"));
    }
    let Some(body) = line.strip_prefix('/') else {
        return Cmd::Send(line.to_string());
    };
    let mut it = body.splitn(2, char::is_whitespace);
    let verb = it.next().unwrap_or_default().to_lowercase();
    let arg = it.next().map(str::trim).filter(|s| !s.is_empty());
    match verb.as_str() {
        "join" | "j" => match arg {
            Some(a) => Cmd::Join(a.to_string()),
            None => Cmd::Usage("usage: /join <channel>  (or kick:<channel>)"),
        },
        "part" | "leave" | "close" => Cmd::Part(arg.map(str::to_string)),
        "quit" | "q" | "exit" => Cmd::Quit,
        "mentions" | "feed" | "whispers" => match crate::TabKind::parse(&verb) {
            Some(k) => Cmd::Tab(k),
            None => Cmd::Send(line.to_string()),
        },
        "w" | "whisper" | "dm" => {
            let mut p = arg.unwrap_or_default().splitn(2, char::is_whitespace);
            let to = p.next().unwrap_or_default();
            let text = p.next().map(str::trim).unwrap_or_default();
            match (verb == "dm", to.is_empty() || text.is_empty()) {
                (_, true) if verb == "dm" => Cmd::Usage("usage: /dm <user> <message>"),
                (_, true) => Cmd::Usage("usage: /w <user> <message>"),
                (true, _) => Cmd::Dm(to.into(), text.into()),
                _ => Cmd::Whisper(to.into(), text.into()),
            }
        }
        "sharelayout" | "sl" | "share" => Cmd::Share,
        "layout" => match arg {
            Some(a) => Cmd::Layout(a.to_string()),
            None => Cmd::Usage("usage: /layout <heatsync.org/m/ link>"),
        },
        "op" => match arg {
            Some(a) => Cmd::Op(a.to_string()),
            None => Cmd::Usage("usage: /op <text>  (posts to the heatsync feed)"),
        },
        "re" => match arg {
            Some(a) => Cmd::Re(a.to_string()),
            None => Cmd::Usage("usage: /re <text>  (replies to your last OP)"),
        },
        // not ours — the platform's problem, and it must arrive untouched.
        _ => Cmd::Send(line.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_takes_a_channel() {
        assert_eq!(parse("/join hot"), Cmd::Join("hot".into()));
        assert_eq!(parse("/j xqc"), Cmd::Join("xqc".into()));
        assert_eq!(
            parse("/join kick:trainwreckstv"),
            Cmd::Join("kick:trainwreckstv".into())
        );
        assert_eq!(parse("  /join   hot  "), Cmd::Join("hot".into()));
    }

    #[test]
    fn join_without_an_argument_explains_itself() {
        assert!(matches!(parse("/join"), Cmd::Usage(_)));
        assert!(matches!(parse("/join   "), Cmd::Usage(_)));
    }

    #[test]
    fn part_and_quit_have_the_usual_aliases() {
        assert_eq!(parse("/part"), Cmd::Part(None));
        assert_eq!(parse("/leave"), Cmd::Part(None));
        assert_eq!(parse("/part xqc"), Cmd::Part(Some("xqc".into())));
        assert_eq!(parse("/quit"), Cmd::Quit);
        assert_eq!(parse("/q"), Cmd::Quit);
    }

    #[test]
    fn virtual_tabs_open_by_name() {
        use crate::TabKind;
        assert_eq!(parse("/mentions"), Cmd::Tab(TabKind::Mentions));
        assert_eq!(parse("/feed"), Cmd::Tab(TabKind::Feed));
        assert_eq!(parse("/Whispers"), Cmd::Tab(TabKind::Whispers));
    }

    #[test]
    fn share_and_layout_are_ours() {
        assert_eq!(parse("/share"), Cmd::Share);
        assert_eq!(parse("/sharelayout"), Cmd::Share);
        assert_eq!(parse("/sl"), Cmd::Share);
        assert_eq!(
            parse("/layout https://heatsync.org/m/t:a"),
            Cmd::Layout("https://heatsync.org/m/t:a".into())
        );
        assert!(matches!(parse("/layout"), Cmd::Usage(_)));
    }

    #[test]
    fn whispers_dms_and_feed_posts_are_ours() {
        assert_eq!(
            parse("/w bob hi there"),
            Cmd::Whisper("bob".into(), "hi there".into())
        );
        assert_eq!(
            parse("/whisper bob  hi"),
            Cmd::Whisper("bob".into(), "hi".into())
        );
        assert_eq!(parse("/dm cat yo"), Cmd::Dm("cat".into(), "yo".into()));
        assert_eq!(parse("/op gm all"), Cmd::Op("gm all".into()));
        assert_eq!(parse("/re agreed"), Cmd::Re("agreed".into()));
        for bare in ["/w", "/w bob", "/dm", "/dm cat", "/op", "/re"] {
            assert!(matches!(parse(bare), Cmd::Usage(_)), "{bare}");
        }
        // `//w` still sends a literal /w
        assert_eq!(parse("//w bob hi"), Cmd::Send("/w bob hi".into()));
    }

    #[test]
    fn verbs_are_case_insensitive() {
        assert_eq!(parse("/JOIN hot"), Cmd::Join("hot".into()));
        assert_eq!(parse("/Quit"), Cmd::Quit);
    }

    /// the one that must never regress: claiming a platform command would break
    /// moderating from the TUI, silently.
    #[test]
    fn platform_commands_pass_through_untouched() {
        for line in [
            "/ban someguy",
            "/timeout someguy 600 spam",
            "/me waves",
            "/vip friend",
            "/unban someguy",
            "/mod someguy",
            "/raid xqc",
            "/clear",
            "/slow 30",
            "/emoteonly",
            "/followers 10m",
            "/announce hello chat",
        ] {
            assert_eq!(
                parse(line),
                Cmd::Send(line.into()),
                "{line} must reach the platform"
            );
        }
    }

    #[test]
    fn ordinary_messages_are_untouched() {
        assert_eq!(parse("hello chat"), Cmd::Send("hello chat".into()));
        assert_eq!(parse("PagMan"), Cmd::Send("PagMan".into()));
        // a bare slash isn't a command
        assert_eq!(parse("/"), Cmd::Send("/".into()));
    }

    #[test]
    fn double_slash_sends_a_literal_slash() {
        assert_eq!(parse("//join hot"), Cmd::Send("/join hot".into()));
        assert_eq!(parse("//ban"), Cmd::Send("/ban".into()));
    }
}
