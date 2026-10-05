//! the mod commands typed in the composer: /ban /unban /timeout /delete.
//!
//! twitch no longer runs these as chat text, so they are caught here and sent
//! through heatsync as real mod actions. parsing only — nothing here touches
//! the network.

use crate::hsauth::ModAction;

#[derive(Debug, PartialEq)]
pub enum Parsed {
    /// do `action` to `target` (a login, or a message id for Delete).
    Run(ModAction, String),
    /// typed wrong: say how it's done.
    Usage(&'static str),
}

const USAGE_BAN: &str = "usage: /ban <user> [reason]";
const USAGE_UNBAN: &str = "usage: /unban <user>";
const USAGE_TIMEOUT: &str = "usage: /timeout <user> [10m|600|1h] [reason]";
const USAGE_DELETE: &str = "usage: /delete <user>  (their last message)  or  /delete <message-id>";
const MAX_TIMEOUT: u32 = 14 * 24 * 3600;

/// `Some` when the line is one of ours. `last_id_of(user)` finds that user's
/// newest message id in the focused tab, for `/delete <user>`.
pub fn parse(line: &str, last_id_of: impl Fn(&str) -> Option<String>) -> Option<Parsed> {
    let body = line.trim().strip_prefix('/')?;
    if body.starts_with('/') {
        return None; // `//x` is an escaped literal
    }
    let mut it = body.split_whitespace();
    let verb = it.next()?.to_lowercase();
    let first = it.next();
    match verb.as_str() {
        "ban" => Some(match first.and_then(login) {
            Some(u) => Parsed::Run(ModAction::Ban, u),
            None => Parsed::Usage(USAGE_BAN),
        }),
        "unban" | "untimeout" => Some(match first.and_then(login) {
            Some(u) => Parsed::Run(ModAction::Unban, u),
            None => Parsed::Usage(USAGE_UNBAN),
        }),
        "timeout" => Some(match first.and_then(login) {
            Some(u) => match it.next().map(secs) {
                None => Parsed::Run(ModAction::Timeout(600), u),
                Some(Some(s)) => Parsed::Run(ModAction::Timeout(s), u),
                // a word that isn't a duration is the start of the reason
                Some(None) => Parsed::Run(ModAction::Timeout(600), u),
            },
            None => Parsed::Usage(USAGE_TIMEOUT),
        }),
        "delete" => Some(match first {
            Some(a) if is_message_id(a) => Parsed::Run(ModAction::Delete, a.to_string()),
            Some(a) => match login(a).and_then(|u| last_id_of(&u)) {
                Some(id) => Parsed::Run(ModAction::Delete, id),
                None => Parsed::Usage("no recent message from them here to delete"),
            },
            None => Parsed::Usage(USAGE_DELETE),
        }),
        _ => None,
    }
}

/// a twitch login: letters, digits, underscore, 1..=25, optional leading @.
fn login(s: &str) -> Option<String> {
    let s = s.trim_start_matches('@').to_lowercase();
    (!s.is_empty() && s.len() <= 25 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
        .then_some(s)
}

/// `600`, `90s`, `10m`, `2h`, `1d` → seconds, 1..=14 days.
fn secs(s: &str) -> Option<u32> {
    let s = s.to_lowercase();
    let (num, mult) = match s.chars().last()? {
        'm' => (&s[..s.len() - 1], 60),
        'h' => (&s[..s.len() - 1], 3600),
        'd' => (&s[..s.len() - 1], 86400),
        's' => (&s[..s.len() - 1], 1),
        c if c.is_ascii_digit() => (&s[..], 1),
        _ => return None,
    };
    let n: u32 = num.parse().ok()?;
    n.checked_mul(mult)
        .filter(|v| (1..=MAX_TIMEOUT).contains(v))
}

/// platform message ids are uuids: 8-4-4-4-12 hex.
fn is_message_id(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(_: &str) -> Option<String> {
        None
    }
    const ID: &str = "3b1c2d4e-5f60-4a7b-8c9d-0e1f2a3b4c5d";

    #[test]
    fn ban_and_unban_take_a_user_and_ignore_the_at_and_case() {
        assert_eq!(
            parse("/ban @Troll", none),
            Some(Parsed::Run(ModAction::Ban, "troll".into()))
        );
        assert_eq!(
            parse("/ban troll being rude", none),
            Some(Parsed::Run(ModAction::Ban, "troll".into()))
        );
        assert_eq!(
            parse("/unban troll", none),
            Some(Parsed::Run(ModAction::Unban, "troll".into()))
        );
        assert_eq!(
            parse("/untimeout troll", none),
            Some(Parsed::Run(ModAction::Unban, "troll".into()))
        );
        assert!(matches!(parse("/ban", none), Some(Parsed::Usage(_))));
        assert!(matches!(
            parse("/ban bad-name!", none),
            Some(Parsed::Usage(_))
        ));
    }

    #[test]
    fn timeout_defaults_to_ten_minutes_and_reads_units() {
        let t = |l: &str| parse(l, none);
        assert_eq!(
            t("/timeout a"),
            Some(Parsed::Run(ModAction::Timeout(600), "a".into()))
        );
        assert_eq!(
            t("/timeout a 30"),
            Some(Parsed::Run(ModAction::Timeout(30), "a".into()))
        );
        assert_eq!(
            t("/timeout a 10m"),
            Some(Parsed::Run(ModAction::Timeout(600), "a".into()))
        );
        assert_eq!(
            t("/timeout a 2h spam"),
            Some(Parsed::Run(ModAction::Timeout(7200), "a".into()))
        );
        assert_eq!(
            t("/timeout a spamming"),
            Some(Parsed::Run(ModAction::Timeout(600), "a".into()))
        );
        // past twitch's 14-day cap or zero: a plain default, never an absurd value
        assert_eq!(
            t("/timeout a 99d"),
            Some(Parsed::Run(ModAction::Timeout(600), "a".into()))
        );
        assert_eq!(
            t("/timeout a 0"),
            Some(Parsed::Run(ModAction::Timeout(600), "a".into()))
        );
        assert!(matches!(t("/timeout"), Some(Parsed::Usage(_))));
    }

    #[test]
    fn delete_takes_a_message_id_or_a_users_last_message() {
        assert_eq!(
            parse(&format!("/delete {ID}"), none),
            Some(Parsed::Run(ModAction::Delete, ID.into()))
        );
        let found = |u: &str| (u == "troll").then(|| ID.to_string());
        assert_eq!(
            parse("/delete @Troll", found),
            Some(Parsed::Run(ModAction::Delete, ID.into()))
        );
        assert!(matches!(
            parse("/delete nobody", found),
            Some(Parsed::Usage(_))
        ));
        assert!(matches!(parse("/delete", none), Some(Parsed::Usage(_))));
    }

    #[test]
    fn everything_else_is_not_ours() {
        for l in ["hello", "/me waves", "/vip x", "//ban x", "/join hot", "/"] {
            assert_eq!(parse(l, none), None, "{l}");
        }
    }

    #[test]
    fn verbs_are_case_insensitive() {
        assert_eq!(
            parse("/BAN x", none),
            Some(Parsed::Run(ModAction::Ban, "x".into()))
        );
    }
}
