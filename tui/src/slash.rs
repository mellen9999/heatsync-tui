//! slash commands typed in the composer, the way every chat client does it.
//!
//! the rule that matters: we only claim commands a CLIENT owns — joining,
//! parting, quitting. everything else is the platform's (`/ban`, `/timeout`,
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
        assert_eq!(parse("/join kick:trainwreckstv"), Cmd::Join("kick:trainwreckstv".into()));
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
            "/w someguy hello",
            "/announce hello chat",
        ] {
            assert_eq!(parse(line), Cmd::Send(line.into()), "{line} must reach the platform");
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
