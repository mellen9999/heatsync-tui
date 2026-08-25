# heatsync tui

heat-sorted live multichat in the terminal. twitch + kick, real emotes, vim keys.

```
cargo install heatsync-tui
```

```
heatsync                     # opens your saved channels (or a demo set)
heatsync xqc forsen kick:trainwreckstv   # explicit channels
heatsync login           # set up sending (see below)
```

keys: `j`/`k` scroll · `h`/`l` (or `j`/`k` with a side tab bar) switch channel ·
`i` compose · `o` join · `m` manage · `x` close · `T` move tab bar · `space` pause · `q` quit.
open channels + tab-bar position persist in `~/.config/heatsync/config`.

## sending

sending is chatterino-style — direct to the platform, using your own token.
run `heatsync login` and follow it. reading always comes through heatsync;
only sending needs your token.

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
