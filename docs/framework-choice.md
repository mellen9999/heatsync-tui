# Why egui

A written comparison rather than a bake-off. The brief was *maximum performance,
featherweight footprint, no compromises* — so this measures against what this
client actually has to do, not against toolkits in the abstract.

Everything numbered here was measured on the current tree, not recalled.

## What the client has to do

Five requirements. The first three are unusual enough that most toolkits are
eliminated by them alone.

1. **Inline images inside wrapping text.** An emote is a word. It participates in
   line-breaking exactly as a word does, and a zero-width emote overlays the one
   before it. This is not "an image next to some text".
2. **Per-glyph colour.** A name paint is a gradient sampled per character, and it
   animates.
3. **Variable-height virtualisation.** A busy channel is tens of thousands of
   messages, each a different height because each wraps differently. Rows have to
   be positioned without measuring all of them.
4. **Windows, on whatever GPU is there.** Including none.
5. **Small.** Chatterino installs at roughly 40–60 MB.

## The candidates

| | inline images in text | per-glyph colour | licence | Windows GL story |
|---|---|---|---|---|
| **egui** | `horizontal_wrapped` + one item per word/emote | one `LayoutJob` section per char | MIT/Apache-2.0 | glow; needs a GL 2.0+ driver |
| **iced** | no first-class inline-image-in-paragraph | limited | MIT | wgpu; tiny-skia fallback |
| **Dioxus / blitz** | yes — it is a browser layout engine | yes, via CSS | MIT/Apache-2.0 | wgpu |
| **Slint** | rich text is a leaf; no inline widgets in flow | no | **GPL-or-commercial** | Skia/femtovg |
| **winit + rasteriser** | whatever we write | whatever we write | permissive | whatever we pick |

**Slint is disqualified outright.** It is GPL-or-paid; this is an MIT crate. That
is a licence conflict, not a preference.

**iced** loses on requirement 1. Its text widget does not take inline image
content in the paragraph flow, so an emote would need to be its own element, and
line-breaking around a sequence of elements is exactly the layout problem we do
not want to re-solve.

**Dioxus/blitz** would satisfy 1–3 comfortably, because it is a browser layout
engine and browsers already solved inline images and per-glyph colour. It loses on
5 and on the reason we are not shipping a webview at all: it is a large
dependency tree whose whole value is generality we do not need.

**winit + a rasteriser** is the honest floor: it satisfies everything, because
everything would be ours. It is also 60–90k lines of text shaping, layout, input
handling and accessibility that already exist elsewhere.

## The uncomfortable finding

The usual argument for immediate-mode is that it is simpler, and the usual
argument against is that it repaints when nothing changed. **Neither matters much
here.**

A chat client with animated emotes repaints continuously no matter which toolkit
draws it. Retained mode's structural advantage is skipping idle frames — and we
do not have idle frames while a `catJAM` is on screen. So the immediate/retained
axis, which is where most of this comparison would normally be decided, is close
to irrelevant for this specific application. Choosing egui *because* it is
immediate-mode would be reasoning about an advantage that does not apply.

What does matter, and what actually decided it:

- **Requirement 1 eliminates the field.** egui's `horizontal_wrapped` takes an
  arbitrary sequence of items and wraps them, so emitting one item per word and
  one per emote stack gets correct inline layout for free. That is the single
  capability the alternatives lack.
- **The real footprint levers are dependency count and the font atlas**, not the
  toolkit's paint model. `eframe` is already `default-features = false`; the
  remaining weight is `default_fonts` (Ubuntu + emoji, compiled in) and the
  `image` codecs, which are genuinely needed for gif/webp emotes.

  Measured, the split is lopsided: **2,048 KB of font atlas against 192 KB of
  emote textures**, ten to one. Both together are 2.2 MB of a 127 MB resident
  set, so texture memory is not where the footprint goes either — optimising
  emote textures would have been optimising 0.15% of it.

Where idle cost *does* appear, it is a real cost and it needed fixing: eframe
keeps calling the app when the window is hidden, so an unmapped window spun a
core. `gui/src/cadence.rs` is the policy — hidden windows get no repaint,
unfocused ones get at most 2/s — and it is a pure function with its own tests
rather than a condition buried in the frame loop.

## Measured, on this tree

| | value |
|---|---|
| `heatsync-gui` release binary | **7.18 MB** (7,531,848 bytes) |
| `heatsync-gui` dependency crates | **162** |
| `heatsync-tui` dependency crates | 154 |
| `heatsync-core` dependency crates | **12** |
| frame cost, 10k-message backlog | **0.46 ms** |
| font atlas / emote textures | **2,048 KB / 192 KB** |

Release profile is already at its size floor: `lto = true`, `opt-level = "z"`,
`strip = true`, `codegen-units = 1`, `panic = "abort"`.

7.18 MB against Chatterino's 40–60 MB install is the number the product claim
rests on. It is a binary-vs-installer comparison and should be stated that way
rather than as a clean 8×.

## The insurance

This is the part that bounds the risk of being wrong.

`heatsync-core` is **2,909 lines across 11 modules** — protocol, emotes, heat,
editing, vi bindings, slash commands, clipboard, sanitisation, key events — with
**12 dependencies and no framework at all**. The egui-specific code is `gui/`
alone: **1,644 lines**.

So if egui ever fails us, what is discarded is 1,644 lines of view code. The
protocol, the emote handling, the editing model and the heat sort are untouched,
and they are also what `heatsync-tui` already runs on. The framework choice is
deliberately the cheapest thing in the tree to reverse.

## What would change this

- egui gaining a hard dependency on a GPU feature the software GL path lacks.
- A Windows GL failure that Mesa cannot stand in for — which is why the CI smoke
  step opens a real window rather than only compiling.
- iced or blitz gaining inline-image-in-wrapping-text as a first-class layout
  primitive. That is the requirement that decided this, so it is the requirement
  worth re-checking.
