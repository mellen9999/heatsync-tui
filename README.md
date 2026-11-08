# heatsync tui

heat-sorted live multichat in the terminal. twitch, kick and youtube (read-only), real emotes, vim keys.

## install

pick one. each gives you the same `heatsync-tui`.

**1. download it** (no rust needed). linux, on a 64-bit pc or arm board:

```sh
mkdir -p ~/.local/bin
curl -fL -o ~/.local/bin/heatsync-tui "https://github.com/mellen9999/heatsync-tui/releases/latest/download/heatsync-tui-$(uname -m)-linux"
chmod +x ~/.local/bin/heatsync-tui
heatsync-tui --version
```

if that last line says "command not found", `~/.local/bin` isn't on your PATH yet: run `~/.local/bin/heatsync-tui` instead, or add `export PATH="$HOME/.local/bin:$PATH"` to your `~/.bashrc` (`~/.zshrc` for zsh) and open a new terminal.

- `uname -m` fills in `x86_64` (most pcs) or `aarch64` (raspberry pi 4/5 on a 64-bit os, asahi). anything else makes curl say "error: 404": there is no build for that machine, use cargo below
- old or odd distro, or alpine, and the file won't start? get the static build (x86_64 only): end the url with `heatsync-tui-x86_64-linux-musl`
- **windows** (10 or newer, 64-bit): open powershell in the folder you want it in and run

  ```powershell
  curl.exe -fL -o heatsync-tui.exe https://github.com/mellen9999/heatsync-tui/releases/latest/download/heatsync-tui-x86_64-windows.exe
  .\heatsync-tui.exe --version
  ```

  it's a terminal app: run it from powershell or windows terminal, not by double-clicking. the exe isn't code-signed, so if you download it in a browser instead, windows says "windows protected your pc": click **more info**, then **run anyway**. downloading with the powershell command above usually skips that
- proof it's the real file: every release has a `SHA256SUMS`. on linux, download it next to the binary and run `sha256sum -c --ignore-missing SHA256SUMS`; on windows `Get-FileHash heatsync-tui.exe` should print the same hash as the windows line
- update: run the same commands again

**2. cargo** (needs [rust](https://rustup.rs) and a c compiler: `gcc` or `clang` on linux and mac, the visual studio build tools on windows):

```sh
cargo install heatsync-tui
```

**3. arch** (builds it for you, also installs the short name `hs`):

```sh
paru -S heatsync-tui
```

## run

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

## more

[changelog](https://github.com/mellen9999/heatsync-tui/blob/main/CHANGELOG.md) · [contributing](https://github.com/mellen9999/heatsync-tui/blob/main/CONTRIBUTING.md) · [security](https://github.com/mellen9999/heatsync-tui/blob/main/SECURITY.md) · MIT
