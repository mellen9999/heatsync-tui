# Decision record

Every decision behind this repo, who made it, what proves it, and whether it is
settled. Each row carries the command that checks it, so a claim here can be
falsified rather than believed.

Verdicts: **CONFIRMED** — verified against the pushed tree. **mellen's call** —
a judgement, recorded so it is not silently relitigated. **OPEN** — not proven
yet, and named as such.

---

## 1. The terminal client is its own repo · CONFIRMED

It lived in `cli/` inside the web monorepo, where it shared none of that repo's
tooling and had no Rust CI at all.

Moving it also moved it between CI regimes: this repo runs `cargo fmt --check`
and `cargo clippy -- -D warnings`, which the monorepo never did. The code arrived
with **127 formatting diffs and 16 clippy errors** — none of them new, all of
them previously unenforced. Worth remembering as the general rule: *code moving
between repos moves between CI regimes, and the destination's gates are the ones
that count.*

```
git log --oneline | wc -l          # 44 — history came across, not a fresh init
cargo fmt --check && cargo clippy --workspace -- -D warnings
```

## 2. The binary is `heatsync-tui`, not `heatsync` · CONFIRMED

An unrelated MIT project of the same name ships `heatsync-bin` on the AUR and
declares both `provides: [heatsync]` and `conflicts: [heatsync]`. Shipping
`/usr/bin/heatsync` would make pacman refuse to install both.

Ours predates theirs by roughly 19 months via the domain — and that does not
matter. A file conflict makes *us* look like the squatter, on the one shelf where
an Arch user meets us first.

```
grep -A2 '^\[\[bin\]\]' tui/Cargo.toml
```

## 3. Every published crate carries its own LICENSE and README · CONFIRMED

crates.io packages cannot include files above their own root, so a workspace-root
LICENSE does not ship. Each publishable crate has its own copy.

```
ls core/LICENSE core/README.md tui/LICENSE tui/README.md
```

`gui/` is `publish = false` and so is exempt today, but it gets both anyway —
it is the crate most likely to be distributed next.

## 4. The editing model lives in core · CONFIRMED

Lifted out of the TUI so a second face could share it. The mechanism worth
recording: `core/src/key.rs` defines `KeyCode`/`KeyModifiers`/`KeyEvent` that
deliberately **mirror crossterm's shape**, so `edit.rs`'s ~40 call sites needed
only a changed `use` line. `tui/src/key.rs` maps crossterm into it, returning
`None` for keys that do not map.

Core is now **2,909 lines across 11 modules with 12 dependencies and no
framework**: protocol, emotes, heat, editing, vi bindings, slash commands,
clipboard, sanitisation, key events.

```
git log --oneline --diff-filter=A -- core/src/edit.rs   # 565ee48
cargo tree -p heatsync-core --prefix none --no-dedupe | sed 's/ (.*//' | sort -u | wc -l
```

## 5. glow, not wgpu · CONFIRMED, with a known cost

A chat window is 2D text and quads. wgpu buys nothing here and drags in the whole
wgpu-hal/D3D12 tree, which as of `windows 0.62.2` **does not compile on the
windows runner at all**. Both backends are GPU-accelerated; this is not a
software-rendering choice.

The cost is real and should not be buried: **glow hard-requires an OpenGL 2.0+
driver.** The windows CI runner has none, which is why CI installs Mesa's
software `opengl32.dll` beside the exe. That is not only a CI quirk — any Windows
box without a GL driver (a VM, an RDP session, a bare install) hits the same
wall. It is the standing argument for revisiting wgpu once wgpu-hal builds again.

```
grep -A4 '^eframe' gui/Cargo.toml
```

## 6. Publish order is core, then tui · CONFIRMED, not yet done

`heatsync-tui` depends on `heatsync-core` by both `path` and `version`; crates.io
rejects a bare `path` dep, and it will reject `heatsync-tui` until
`heatsync-core@0.1.0` exists. A publish yanks but never deletes, so the order is
one-way.

Neither name is taken yet — checked, both return *does not exist*:

```
curl -s -A ua https://crates.io/api/v1/crates/heatsync-core
curl -s -A ua https://crates.io/api/v1/crates/heatsync-tui
```

Still **mellen's two clicks**: `cargo publish -p heatsync-core`, then
`cargo publish -p heatsync-tui`.

## 7. Native, not Tauri or a PWA · mellen's call

The technical case for wrapping the existing web client in Tauri is strong and
was not dismissed: it would reuse the ~92k lines of web client rather than
re-deriving them. That is the honest cost of this decision and it is recorded in
full rather than argued away — see [framework-choice.md](framework-choice.md).

mellen's reason is a perception argument, not a performance one: *"ppl think
browser is slow and shit and rust/c is fast and clean."* For a client whose whole
pitch is that it is lighter than what people already run, what users believe
about a webview is part of the product.

## 8. Wedge positioning, never a Chatterino replacement · mellen's call

The client complements Chatterino rather than competing with it. This constrains
scope permanently and is the reason the README does not read as a migration
pitch.

## 9. Full parity with the web chat client · mellen's call

Scope is parity, not a subset. Recorded because it is the decision that makes
inline emotes, name paints and virtualisation *requirements* rather than
nice-to-haves — and requirement 1 in framework-choice.md is what eliminated
every alternative toolkit.

## 10. egui, chosen on one requirement · CONFIRMED

See [framework-choice.md](framework-choice.md) for the full comparison, including
the finding that the immediate-vs-retained axis barely applies to a client that
repaints continuously anyway.

The three gating risks were each proven by hand and are now held by tests in
`gui/src/e2e.rs`, which drive the real widget tree through egui_kittest/AccessKit
— no GPU, so they run on all three runners:

- an emote sits **inline**, asserted on geometry (`before` and `after` share a y)
- a long message wraps
- a 10k backlog renders **fewer than 200 rows**, and a 3-message backlog renders 3

```
cargo test --workspace
```

## 11. Windows is proven by opening a real window · CONFIRMED for the harness, OPEN for the driver

CI compiled and unit-tested on Windows for weeks without anything ever creating a
window. The smoke step now does, on all three platforms, through
`cargo run --release -p heatsync-gui -- --smoke`.

Two harness bugs were found and fixed getting there, both worth keeping:

- The first version redirected stdout/stderr to files and printed them after
  `WaitForExit(ms)`. **That call does not wait for the redirected streams**, so
  the failure arrived as a bare `exit code 1` with the message unflushed in a
  file nobody read. Nothing is redirected now; the binary's stderr *is* the log.
- The deadline is `timeout-minutes`, not a script. A window that never maps
  leaves eframe not calling the app at all, so no in-process guard can fire —
  and unlike GNU `timeout`, the runner's own deadline exists on macOS too.

**OPEN:** the runner has no real GPU, so this proves the glow path against Mesa's
software rasteriser only. Only mellen booting heatpc to Windows proves it against
an actual Radeon driver.

## 12. Hidden windows must not burn a core · CONFIRMED

eframe keeps driving the app when the window is not visible — it calls `logic()`
instead of `ui()` and spins. A chat client sits minimised all day, so this
contradicted the entire reason for going native.

`gui/src/cadence.rs` is a pure function: hidden → no repaint; unfocused → at most
2/s; unknown visibility → treated as visible, because guessing wrong in that
direction costs battery rather than correctness. Eight tests.

Worth recording the misdiagnosis: the 103% CPU was first blamed on the app, and
the actual cause was **mellen's monitor being off**, so the window never mapped.

---

## Measured baseline

Numbers, not impressions. Re-measure before claiming any of them moved.

| | value |
|---|---|
| `heatsync-gui` release binary | 7.18 MB |
| dependency crates — gui / tui / core | 162 / 154 / 12 |
| frame cost, 10k-message backlog | 0.46 ms |
| resident memory | 88 MB |
| Chatterino, for reference | 40–60 MB installed |

## Still open

1. **Windows on real hardware** — decision 11. mellen's boot.
2. **crates.io publish** — decision 6. mellen's two clicks; both names still free.
3. **Featherweight pass** — `default_fonts` compiles Ubuntu + emoji into the
   binary when the UI font is Cozette 13px, and the 88 MB RSS has not been
   attributed between font atlas, emote textures and parsed messages. Both are
   measurable and neither has been measured.
