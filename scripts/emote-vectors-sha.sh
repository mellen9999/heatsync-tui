#!/usr/bin/env bash
# print the sha256 of the vendored emote-law vectors (the file itself, not the
# pinned note), one line, no test run. a cross-repo gate compares it to the
# site's `sha256sum shared/emote-law.vectors.json`.
#   scripts/emote-vectors-sha.sh
set -euo pipefail
f="$(cd "$(dirname "$0")/.." && pwd)/core/tests/emote-law.vectors.json"
[ -f "$f" ] || { echo "not found: $f" >&2; exit 1; }
sha256sum "$f" | cut -d' ' -f1
