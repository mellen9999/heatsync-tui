# contributing

heatsync-tui is heat-sorted live multichat in the terminal: twitch, kick and youtube, real emotes, vim keys. this repo is a cargo workspace: `core` (protocol, heat, emotes, editing, vi keys), `tui` (the terminal app) and `gui` (a desktop prototype).

## build and test

needs a current stable rust.

```sh
cargo build --release
cargo test --workspace
cargo fmt
cargo clippy --workspace -- -D warnings
```

run it: `cargo run --release -p heatsync-tui -- xqc`

the gui prototype needs an opengl 2.0+ driver: `cargo run --release -p heatsync-gui -- --smoke` opens a window, draws 30 frames and exits.

ci runs `cargo fmt --check`, the clippy and test lines above, and the gui smoke on linux, windows and macos for every pull request.

## commits and pull requests

- conventional commits, lowercase, small and atomic: `fix(streamrow): went-live verb wears yellow`
- one change per pull request. a behaviour change comes with a test
- run the commands above before you open it
- chat text is untrusted: strip it with `core/src/sanitize.rs` before it reaches the terminal

## licence

MIT. by contributing you agree your work is released under the repo's [LICENSE](LICENSE).

## security

found a vulnerability? don't open a public issue. see [SECURITY.md](SECURITY.md).
