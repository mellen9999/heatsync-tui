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

## releasing (maintainer)

1. bump `version` in `core/Cargo.toml` and `tui/Cargo.toml`, and the `heatsync-core` version beside the path in `tui/Cargo.toml` and `gui/Cargo.toml`. add a `## x.y.z - date` section to `CHANGELOG.md`, run `cargo build --release` so `Cargo.lock` follows, commit
2. `cargo publish -p heatsync-core -p heatsync-tui` (core goes first; `heatsync-gui` is never published)
3. `git tag vX.Y.Z && git push origin main vX.Y.Z`: the release workflow builds the linux, arm64 and windows binaries and publishes the github release with its `SHA256SUMS`. the tag has to match `tui/Cargo.toml`
4. `sh packaging/aur.sh`, read and commit its diff, then `sh packaging/aur.sh push`

## licence

MIT. by contributing you agree your work is released under the repo's [LICENSE](LICENSE).

## security

found a vulnerability? don't open a public issue. see [SECURITY.md](SECURITY.md).
