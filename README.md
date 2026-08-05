# heatsync tui

heat-sorted live multichat in the terminal. twitch + kick, real animated emotes,
emote modifiers, vim keys. rust, no async runtime, no C deps.

```
heatsync                     # opens your saved channels
heatsync xqc forsen kick:trainwreckstv   # explicit channels
heatsync login               # set up sending (see below)
```

## keys

normal mode:

| key | action |
|-----|--------|
| `j` / `k` | scroll (or switch channel with a side tab bar) |
| `h` / `l` · `tab` / `shift-tab` · `1`-`9` | switch channel |
| `i` | compose a message |
| `o` | join a channel |
| `m` | manage channels (reorder / leave) |
| `x` close · `T` move tab bar · `space` pause · `q` quit | |

composing (`i`) is a real line editor: arrows/`home`/`end` move, `ctrl-w` kills
the last word, `ctrl-u` kills to start, `ctrl-a`/`ctrl-e` jump. `tab` completes
emote names and `@user` mentions from recent chatters (`shift-tab` cycles back).
join (`o`) tab-completes channels you've opened before.

while your message contains emotes, a live preview strip shows it exactly as it
will render — images, overlay stacks, and modifiers included.

open channels + tab-bar position persist in `~/.config/heatsync/config`.

## emote modifiers

full modifier grammar, applied as a pixel pass on the real images:

| syntax | effect |
|--------|--------|
| `w!emote` / `emote ffzW` | wide (double width) |
| `h!emote` / `emote ffzX` | flip horizontal |
| `v!emote` / `emote ffzY` | flip vertical |
| `c!emote` / `emote ffzCursed` | grayscale + darken |
| `p!emote` / `emote ffzRainbow` | hue cycle (animates static emotes) |
| `s!emote` / `emote ffzHyper` | shake |
| `l!emote` · `r!emote` | rotate left / right |
| `z!emote` | force zero-width (overlay on the previous emote) |

bttv prefixes work bare (`w! emote`) or attached (`w!h!emote`, chains fine);
ffz effect words go after the emote they modify. zero-width emotes stack onto
their base automatically.

## sending

sending is chatterino-style — direct to the platform, using your own token.
run `heatsync login` (twitch) or `heatsync login kick` and follow it. reading
always comes through heatsync; only sending needs your token. tokens are stored
0600 in your config dir and never leave your machine except to the platform.

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
| bare linux console | linux | pixels straight to /dev/fb0 |
| anything else | — | emote names as text (still fully usable) |

sixel terminals animate at a steady ~10fps (tear-free); the kitty protocol
(kitty/ghostty/wezterm) runs smoother and never flickers. running inside
**tmux** works: the client asks tmux for the outer terminal and wraps graphics
in passthrough — no keyboard-stealing capability query.

## headless subcommands

```
heatsync log <channel> <YYYY-MM-DD>   # a day of chat, from the archive
heatsync search <query> [channel]     # search the archive
heatsync hot                          # hottest channels right now
```

## build

```
cargo build --release        # single static-ish binary, lto, ~2MB
```

two crates: `core` (protocol, heat ramp, emote grammar — zero i/o, fully
tested) and `tui` (ratatui face). `cargo test` runs the lot.

## license

MIT
