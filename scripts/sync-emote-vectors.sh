#!/usr/bin/env bash
# refresh the vendored emote vectors (the law, the picker tile marks, the big emote-only lines)
# from the site checkout, byte for byte. the files only ever change on the site side; never edit the
# copies here.
#   scripts/sync-emote-vectors.sh [path-to-site-checkout]
set -euo pipefail
site="${1:-$HOME/projects/heatsync}"
root="$(cd "$(dirname "$0")/.." && pwd)"
for name in emote-law emote-mark emote-only; do
  src="$site/shared/$name.vectors.json"
  dst="$root/core/tests/$name.vectors.json"
  [ -f "$src" ] || { echo "not found: $src" >&2; exit 1; }
  cp "$src" "$dst"
  sha256sum "$dst" | cut -d' ' -f1 > "${dst%.json}.sha256"
  echo "$name $(cat "${dst%.json}.sha256")"
done
echo "now run: cargo test -p heatsync-core --test emote_law_vectors --test emote_mark_vectors --test emote_only_vectors"
