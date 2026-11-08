# security

## reporting a vulnerability

email **mellen@heatsync.org** with:

- a clear description of the issue
- steps to reproduce
- potential impact
- your preferred contact for follow-up

please don't open a public GitHub issue for security bugs. we'll acknowledge within 48 hours and aim to ship a fix within 7 days for critical issues.

## supported versions

the latest release.

## worth reporting

- your heatsync token (in `~/.config/heatsync`, mode 0600) printed, logged, readable by other users, or sent anywhere but heatsync.org. plain http is only allowed to loopback
- chat text that gets past the sanitiser and reaches your terminal as an escape sequence (retitle, clipboard write, anything else)
- a chat line, server frame or image that crashes or hangs the app
- a login or pairing flow that grants more than the boxes you ticked

## scope

the terminal never holds a twitch or kick token: it asks heatsync.org, which holds those server-side. the heatsync token it does hold is scoped to this client, revocable, and expires. chat text is untrusted, so every control byte is stripped before it is drawn (`core/src/sanitize.rs`).
