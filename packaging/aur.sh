#!/bin/sh
# the AUR package heatsync-tui: it builds the crates.io release on the user's machine.
#   sh packaging/aur.sh [X.Y.Z]  refresh packaging/aur (PKGBUILD + .SRCINFO) for that version, default the one in
#                                tui/Cargo.toml. downloads its crate from crates.io (so it must be published first)
#                                and writes the version and checksum. publishes nothing
#   sh packaging/aur.sh push     publish what is committed in packaging/aur to the AUR. needs your ssh key
#                                registered on aur.archlinux.org. the only step that leaves this machine
# after cargo publish:  sh packaging/aur.sh, look at the diff and commit it, then sh packaging/aur.sh push
set -eu
cd "$(git rev-parse --show-toplevel)"
dir=packaging/aur
aur=ssh://aur@aur.archlinux.org/heatsync-tui.git

fail() {
  echo "$1" >&2
  exit 1
}

# downloads the crate for $1 into $2 and prints its sha256
crate_sum() {
  curl -fsSL "https://static.crates.io/crates/heatsync-tui/heatsync-tui-$1.crate" -o "$2" \
    || fail "can't download heatsync-tui $1 from crates.io: is it published? (cargo publish -p heatsync-core -p heatsync-tui)"
  sha256sum "$2" | cut -d' ' -f1
}

refresh() {
  ver=${1:-$(sed -n 's/^version = "\(.*\)"$/\1/p' tui/Cargo.toml | head -n1)}
  case "$ver" in "" | *[!0-9.]* | *..* | .* | *.) fail "that isn't a version: $ver" ;; esac
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  sum=$(crate_sum "$ver" "$tmp/crate")
  old=$(sed -n 's/^pkgver=//p' "$dir/PKGBUILD")
  # a new version starts over at release 1; the same version again (a packaging fix) keeps its release number to bump by hand
  rel=$(sed -n 's/^pkgrel=//p' "$dir/PKGBUILD")
  [ "$old" = "$ver" ] || rel=1
  awk -v ver="$ver" -v rel="$rel" -v sum="$sum" '
    /^pkgver=/ { print "pkgver=" ver; next }
    /^pkgrel=/ { print "pkgrel=" rel; next }
    /^sha256sums=/ { print "sha256sums=(\047" sum "\047)"; next }
    { print }
  ' "$dir/PKGBUILD" > "$tmp/PKGBUILD"
  cp "$tmp/PKGBUILD" "$dir/PKGBUILD"
  (cd "$dir" && makepkg --printsrcinfo) > "$tmp/SRCINFO"
  cp "$tmp/SRCINFO" "$dir/.SRCINFO"
  git --no-pager diff --stat -- "$dir"
  echo "done: $dir is ready for $ver. read the diff, commit it, then: sh packaging/aur.sh push"
}

push() {
  [ -z "$(git status --porcelain -- "$dir")" ] || fail "$dir has changes that aren't committed: commit them first, so the repo records exactly what the AUR gets"
  ver=$(sed -n 's/^pkgver=//p' "$dir/PKGBUILD")
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  want=$(sed -n "s/^sha256sums=('\\(.*\\)')\$/\\1/p" "$dir/PKGBUILD")
  got=$(crate_sum "$ver" "$tmp/crate")
  [ "$got" = "$want" ] || fail "the PKGBUILD's checksum isn't crates.io's for $ver: run sh packaging/aur.sh, commit, then push"
  (cd "$dir" && makepkg --printsrcinfo) > "$tmp/SRCINFO"
  cmp -s "$tmp/SRCINFO" "$dir/.SRCINFO" || fail ".SRCINFO doesn't match the PKGBUILD: run sh packaging/aur.sh, commit, then push"
  # the AUR keeps its own git history of just these two files, commits there are public under this name
  name=$(git config user.name || true)
  email=$(git config user.email || true)
  [ -n "$name" ] && [ -n "$email" ] || fail "set your git name and email first (git config user.name / user.email): AUR commits carry them"
  git clone -q "$aur" "$tmp/aur" || fail "can't reach the AUR: is your ssh key registered on aur.archlinux.org?"
  cp "$dir/PKGBUILD" "$dir/.SRCINFO" "$tmp/aur/"
  git -C "$tmp/aur" add PKGBUILD .SRCINFO
  if git -C "$tmp/aur" diff --cached --quiet; then
    echo "the AUR already has this"
    return 0
  fi
  git -C "$tmp/aur" --no-pager diff --cached --stat
  rel=$(sed -n 's/^pkgrel=//p' "$dir/PKGBUILD")
  git -C "$tmp/aur" -c "user.name=$name" -c "user.email=$email" commit -q -m "heatsync-tui $ver-$rel"
  echo "publishing $ver-$rel to the AUR as $name"
  git -C "$tmp/aur" push origin HEAD:master
  echo "done: heatsync-tui $ver-$rel is on the AUR"
}

case "${1:-}" in
  push) [ "$#" -eq 1 ] || fail "usage: sh packaging/aur.sh [X.Y.Z | push]"; push ;;
  "" | [0-9]*) [ "$#" -le 1 ] || fail "usage: sh packaging/aur.sh [X.Y.Z | push]"; refresh "${1:-}" ;;
  *) fail "usage: sh packaging/aur.sh [X.Y.Z | push]" ;;
esac
