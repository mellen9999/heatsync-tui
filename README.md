# heatsync tui

heat-sorted live multichat in the terminal. twitch, kick and youtube (read-only), real emotes, vim keys.

install (needs [rust](https://rustup.rs)):

```
cargo install heatsync-tui
```

arch: `paru -S heatsync-tui` (also installs the short name `hs`).

run:

```
heatsync-tui                                   # your saved channels (or a demo set)
heatsync-tui xqc forsen kick:trainwreckstv     # pick channels (youtube: yt:<video id>)
heatsync-tui login                             # pair this terminal (see login)
```

keys: `j`/`k` scroll · `h`/`l` (or `j`/`k` with a side tab bar) switch channel ·
`i` compose · `o` pick a live channel (opens a temp tab, underlined) · `s` keep the temp tab · `m` manage · `x` close · `T` move tab bar · `v` message cursor (`j`/`k` `ctrl-d`/`ctrl-u` `g`/`G` move · `r` reply · `y` yank · `d` delete · `esc` back) · `t` timestamps · `P` hide the pinned message · `space` pause · `q` quit.
open channels + tab-bar position persist in `~/.config/heatsync/config`.

## login

`heatsync-tui login` pairs this terminal with your heatsync account: it shows a
code, you approve it on heatsync.org, and a revocable token lands in
`~/.config/heatsync` (mode 0600). on the approve page you tick what the
terminal may do. chat (send chat, feed posts, dms, whispers) is always on; two
boxes are optional:

- **mod** — ban / timeout / delete from the tui
- **account** — read your dms, whispers and mention hits (also signs the live socket in)

run `heatsync-tui login` again any time to change the ticks. no login = read-only.
`HEATSYNC_TOKEN` in the environment still overrides the socket login.

## tabs and commands

open with `/mentions`, `/feed`, `/whispers` — they pin left and stay open next run.

| tab | what it holds | how you talk |
|-----|---------------|--------------|
| mentions | everything that pinged you, plus what your heatsync mention rules caught while you were away | read-only · `enter` jumps to the source tab |
| feed | the live heatsync feed | type to post an OP · `r` on a row (cursor mode) replies to it |
| whispers | twitch whispers + heatsync dms, history loads on open | `r` on a row fills in `/w <them> ` or `/dm <them> ` |

- `/open <channel>` look at a stream in the one temp tab (underlined, never saved, replaced by the next look); `/keep` or `s` saves it, and so does a send that lands. `/join` always saves
- `/layout <heatsync.org/m/ link>` load a shared layout · `/sharelayout` (`/sl`) share yours
- `/w <user> <msg>` twitch whisper · `/dm <user> <msg>` heatsync dm — works from any tab
- `/op <text>` post to the feed · `/re <text>` reply to the newest OP in the feed tab
- `/w` and `/dm` look the name up on heatsync (and remember it); an unknown name leaves your text in the composer. whispering needs twitch linked with whispers allowed (the tui prints the exact link if not).
- sends and reads need the matching tick above; if one is missing the tui says so in a line and which command fixes it.

## emotes

emotes draw as real images (animated) in terminals that support inline
graphics, detected automatically — nothing to set:

- kitty · ghostty · wezterm — kitty graphics, flicker-free
- foot · mlterm · contour · gnome-terminal (vte 0.78+) — sixel
- iterm2 · mintty — inline images
- anything else — emote names as text, fully usable

works inside tmux (it asks tmux for the outer terminal).
