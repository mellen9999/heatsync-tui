# heatsync tui

heat-sorted live multichat in the terminal. twitch + kick, real emotes, vim keys.

```
cargo install heatsync-tui
```

```
heatsync-tui                 # or just: hs — opens your saved channels (or a demo set)
heatsync-tui xqc forsen kick:trainwreckstv   # explicit channels
heatsync-tui login           # pair this terminal (see login)
```

keys: `j`/`k` scroll · `h`/`l` (or `j`/`k` with a side tab bar) switch channel ·
`i` compose · `o` pick a live channel · `m` manage · `x` close · `T` move tab bar · `v` message cursor (`j`/`k` `ctrl-d`/`ctrl-u` `g`/`G` move · `r` reply · `y` yank · `d` delete · `esc` back) · `t` timestamps · `P` hide the pinned message · `space` pause · `q` quit.
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

- `/w <user> <msg>` twitch whisper · `/dm <user> <msg>` heatsync dm — works from any tab
- `/op <text>` post to the feed · `/re <text>` reply to the newest OP in the feed tab
- `/w` finds anyone whose chat line is on screen or who you've whispered; `/dm` finds your dm threads and the people you follow on heatsync. whispering needs twitch linked with whispers allowed (the tui prints the exact link if not).
- sends and reads need the matching tick above; if one is missing the tui says so in a line and which command fixes it.

## emotes

emotes render as real (animated) images when the terminal supports inline
graphics. the client auto-detects and picks the best protocol; nothing to set.

| terminal | os | result |
|----------|----|--------|
| **foot** | linux | sixel — crisp, stable (the light default) |
| **Windows Terminal** | windows | sixel — works out of the box |
| **iTerm2** | macos | native inline images |
| kitty · ghostty | linux/mac | flicker-free, extra smooth |
| **WezTerm** | win/mac/linux | flicker-free — the one graphics terminal on all three |
| anything else | — | emote names as text (still fully usable) |

sixel terminals animate at a steady ~10fps (tear-free); the kitty protocol
(kitty/ghostty/wezterm) runs smoother and never flickers. you don't need a heavy
terminal — foot / Windows Terminal are light and look great. WezTerm is only
worth it if you specifically want the silkiest animation on windows.

running inside **tmux** works: the client asks tmux for the outer terminal and
wraps graphics in passthrough — no keyboard-stealing capability query.
