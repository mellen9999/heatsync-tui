#!/usr/bin/env bash
# print the sha256 of a vendored emote vectors file (the file itself, not the pinned
# note), one line, no test run. a cross-repo gate compares it to the site's
# `sha256sum shared/<name>.vectors.json`. with no argument: the law's; or name
# the set, `emote-mark` for the picker tile marks, `emote-only` for the big emote-only lines.
#   scripts/emote-vectors-sha.sh [emote-law|emote-mark|emote-only]
set -euo pipefail
name="${1:-emote-law}"
case "$name" in emote-law | emote-mark | emote-only) ;; *) echo "unknown vectors: $name" >&2; exit 1 ;; esac
f="$(cd "$(dirname "$0")/.." && pwd)/core/tests/$name.vectors.json"
[ -f "$f" ] || { echo "not found: $f" >&2; exit 1; }
sha256sum "$f" | cut -d' ' -f1
