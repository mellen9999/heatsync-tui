#!/usr/bin/env bash
# refresh the vendored emote-law vectors from the site checkout, byte for byte.
# the file only ever changes on the site side; never edit the copy here.
#   scripts/sync-emote-vectors.sh [path-to-site-checkout]
set -euo pipefail
site="${1:-$HOME/projects/heatsync}"
src="$site/shared/emote-law.vectors.json"
dst="$(cd "$(dirname "$0")/.." && pwd)/core/tests/emote-law.vectors.json"
[ -f "$src" ] || { echo "not found: $src" >&2; exit 1; }
cp "$src" "$dst"
sha256sum "$dst"
echo "now run: cargo test -p heatsync-core --test emote_law_vectors"
