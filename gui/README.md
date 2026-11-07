# heatsync-gui

heat-sorted live multichat on the desktop — twitch, kick and youtube in one native window (not a webview). prototype; not on crates.io.

## run

```
cargo run --release -p heatsync-gui
```

`--smoke` opens a window, renders 30 frames and exits (the ci check on linux, macos and windows). `--stats` prints frame cost.

needs an opengl 2.0+ driver (renderer is glow) — some vms, rdp sessions and bare windows installs lack one.

`gui/` is the only egui-specific code; protocol, emotes, heat sorting, editing and vi bindings live in `heatsync-core`.

mit.
