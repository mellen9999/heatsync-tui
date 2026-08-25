# heatsync-gui

heat-sorted live multichat on the desktop — twitch, kick and youtube in one
window. A native window, not a webview.

Complements Chatterino rather than replacing it: the thing it does that nothing
else does is sort a merged multi-platform chat by heat.

## Running it

```
cargo run --release -p heatsync-gui
```

`--smoke` opens a window, renders 30 frames and exits — the check CI runs on
Linux, macOS and Windows so that "compiles" is never mistaken for "runs".
`--stats` prints frame cost while it runs.

## What it needs

An OpenGL 2.0+ driver. The renderer is glow, so a machine with no GL driver at
all — some VMs, some RDP sessions, a bare Windows install — needs one present.
See [../docs/decisions.md](../docs/decisions.md) for why glow and not wgpu.

## Where the code is

`gui/` is the only egui-specific code in the workspace, and it is small on
purpose: protocol, emotes, heat sorting, the editing model and vi bindings all
live in `heatsync-core`, which has no framework dependency at all. Replacing the
toolkit would discard the view layer and nothing else.

MIT.
