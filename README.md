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

### the composer is a real vim line editor

`i` opens it in insert mode; `esc` drops to a normal mode ON the line —
motions (`h l 0 ^ $ w b e ge f t F T ; ,`), operators (`d c y` + motion,
`dd cc yy D C Y`), counts (`3w`, `d2w`), `x X s r ~ p P`, visual mode
(`v V`, `o` swaps ends), undo/redo (`u` / `ctrl-r`), and `.` repeats the last
change — inserted text included. `esc` again leaves the composer. insert mode
keeps the emacs keys too (`ctrl-w/u/a/e`). the footer chip shows
insert/normal/visual.

### completion is a dropdown

`tab` opens a popup above the composer — fuzzy-ranked (prefix > word-boundary >
substring > subsequence), live-narrowing as you type, provider badge per row
(`7tv bttv ffz emoji user chan`). `tab`/`↓` and `shift-tab`/`↑` move, `enter`
accepts, `esc` closes. `@` completes recent chatters as mentions; join (`o`)
completes channels you've opened before. the popup overlays chat — the layout
never shifts.

### emoji

`:joy:` becomes 😂 the moment you close the colon (full gemoji shortcode set).
`:par` pops the dropdown with matching emoji AND emotes after two chars —
accepting an emote swaps the `:query` for the emote name.

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

the pipeline is tuned for pristine pixels: 2x cdn assets, Lanczos3 resampling
to the exact cell footprint, DEC 2026 synchronized updates (tear-free while
chat scrolls), stable row heights (no reflow jump when an image lands), a
bounded lru of encoded frames, and a 64MB disk cache so relaunches are
instant. sixel terminals animate at a steady ~10fps; the kitty protocol
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
