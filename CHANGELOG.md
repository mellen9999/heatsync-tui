# changelog

what changed in each release, newest first. the release workflow copies the section for a tag into the github release notes, so every `## x.y.z` heading needs a matching tag.

## 0.4.0 - 2026-11-12

`heatsync-core` moves to 0.4.0 with it (its unused vi module is gone).

### language

- speaks the same 34 languages as the browser extension. it follows your system language, english when yours isn't there. `heatsync-tui lang` shows the list, `heatsync-tui lang de` picks one, `auto` goes back
- long text in wide scripts (chinese, japanese, korean) is cut by its width on screen, not its letter count

### chat

- one platform message is one row: a reconnect no longer draws a kick or youtube room's recent lines (and their pings) a second time, and a line from the history never repeats live. a real second gift with the same words still shows
- a chatter typing "Login authentication failed" (or twitch's welcome words) no longer ends your sends. only twitch's own lines count
- a broken or hostile emote image shows as its name instead of closing the app
- `heatsync-tui hot` says "nothing hot" when nothing is, instead of listing the newest posts with a heat of 0
- `/block` files a block the way the extension, desktop and chatterino do, so a block made in one hides the emote in all of them. blocks made by older versions still work

### smaller

- 21 fewer crates and no jpeg decoder (emotes are gif, webp and png). the linux framebuffer is drawn with two kernel calls instead of two crates

## 0.3.1 - 2026-11-09

### chat

- mod anniversaries get their own green row, including twitch's `modiversary` notice

## 0.3.0 - 2026-11-08

### chat

- emote order fixed: the channel's emote wins a name clash, and platform tokens beat same-name set emotes
- the streamer's own heatsync emotes render for everyone in their channel
- `/block <emote>` hides one exact emote, `/unblock` brings it back, `/blocked` lists them. `b` does it from the message cursor
- notification rows are filled, and your picked heatsync badge shows

### composer and keys

- tab-complete keeps case twins (both spellings stay, the one you typed leads, order is stable)
- `alt+1` to `alt+9` jump to a tab
- lines you sent are remembered across restarts

## 0.2.0 - 2026-11-08

everything since 0.1.1, the last version on crates.io. `heatsync-core` moves to 0.2.0 with it (the tui needs the new core). 0.1.2 was never published; its fixes are in here.

### log in with heatsync

- `heatsync-tui login` pairs the terminal with your heatsync account: it shows a code, you approve it on heatsync.org and tick what the terminal may do. the token is revocable and lives in `~/.config/heatsync` (mode 0600). no third-party token site any more
- sends, mod actions and the live socket go through heatsync.org. the terminal never holds a twitch or kick token
- optional ticks: **mod** (`/ban`, `/unban`, `/timeout`, `/delete` from the composer) and **account** (your dms, whispers and mention hits). run `login` again to change them
- login drops you into the chat ui and says what twitch still needs; `heatsync-tui logout` forgets the login
- twitch send feedback (notice rejections show up), and a duplicate-message bypass so a repeated line still sends
- mod deletes strike the line through with a tag (clearmsg, clearchat, youtube delete)

### new tabs and commands

- `/mentions`, `/feed`, `/whispers`: virtual tabs, pinned left, saved between runs. mentions holds everything that pinged you plus what your heatsync mention rules caught while you were away, and followed-channel go-lives as dim `[chan]` rows. feed is the live heatsync feed (post an OP, reply with `r`). whispers holds twitch whispers and heatsync dms
- `/w <user> <msg>` whisper, `/dm <user> <msg>` heatsync dm, `/op <text>` post to the feed, `/re <text>` reply to the newest OP. unknown names are looked up on heatsync and remembered
- `/open <channel>` (or `o`, the live-channel picker: following, yours, top live) looks at a stream in one temp tab, underlined and never saved. `/keep` or `s` keeps it
- `/sharelayout` (`/sl`) shares your tabs as a heatsync.org/m/ link, `/layout <link>` loads someone else's
- `v` message cursor: move over lines, `r` reply, `y` yank, `d` delete
- `P` hides the pinned message bar; `t` toggles timestamps

### chat

- merged tabs: one channel across twitch, kick and youtube, interleaved, with `[T]` `[K]` `[Y]` tags. any mix, any order, and a merged tab upgrades in place
- youtube live chat (read-only): `yt:<video id>` or a pasted url
- stream events in the chat: go-live and switched-to rows (naming the streamer, in their colour and paint), subs, gifts, raids, redeems, cheers, category changes, spikes and mod actions, with guards so a flapping stream can't wall the chat. a yellow tab level marks unseen events
- reply context rows, `/me`, first-message and highlight styling, announcement and milestone notices, room mode changes (slow, followers-only, emote-only, sub-only)
- role badges, name paints and heatsync profile colours; tab completion for emotes and chatters; a live preview row for a draft that has an emote in it
- post edits, account changes and follows apply live without a restart
- history loads when a tab opens; long messages wrap onto indented rows, capped at eight

### emotes

- global emotes: twitch, bttv, ffz and 7tv globals plus the unlocked pool, under every tab
- emote refs come with each message (heatsync, native twitch, kick), so the right emote draws
- modifier parity with the extension: per-layer effects, ffz motion, `tall`/`wide`, `c!#hex` tint, binding directions, glued suffixes
- the manage editor (`m`, then `e`, then `t`/`k`/`y`) edits any source of a merged tab

### terminal and tmux

- under tmux, sixel images queue after the text and re-send only when they change, and animated frames go around as passthrough, so tmux stops deleting and repainting every image each frame
- chat that moved up scrolls with a real terminal scroll, so images move with the text
- an emote's cells are erased before its pixels land, and the quantiser scales its sampling to the frame size
- repaints are throttled (4fps, 2fps while typing) so keys over mosh only repaint the input line
- ctrl-c exits clean outside the chat ui

### look

- the 8-colour ansi palette. chat text is plain white; bold is for names and headers
- tab unread state; the composer says read-only only when nothing can send

### speed

- the event loop drains chat and finished emote loads on every wake, fetches emotes the moment a line arrives and only wakes for animations that are on screen

## 0.1.1 - 2026-08-25

first real release: live twitch and kick chat, heat sorting, real emotes in the terminal, vim keys.
