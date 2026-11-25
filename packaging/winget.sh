#!/bin/sh
# the winget package HeatSync.TUI: the github release's windows exe, installed by winget as a portable console app.
#   sh packaging/winget.sh [vX.Y.Z]  refresh packaging/winget (its three manifests) for that release, default the
#                                    latest. downloads the exe, checks it against the release's own SHA256SUMS,
#                                    writes the version, date and hash. publishes nothing
#   sh packaging/winget.sh pr        open the pull request on microsoft/winget-pkgs with what is committed in
#                                    packaging/winget, from your fork of it (made if missing). the only step that
#                                    leaves this machine. the first one waits on the microsoft CLA: one comment on it
# after a release is on github:  sh packaging/winget.sh, look at the diff and commit it, then sh packaging/winget.sh pr
set -eu
cd "$(git rev-parse --show-toplevel)"
dir=packaging/winget
repo=mellen9999/heatsync-tui
id=HeatSync.TUI
asset=heatsync-tui-x86_64-windows.exe
up=microsoft/winget-pkgs
schema=1.28.0

fail() {
  echo "$1" >&2
  exit 1
}

# the literal `$schema=` is the yaml-language-server's own key
# shellcheck disable=SC2016
head_line() {
  printf '# yaml-language-server: $schema=https://aka.ms/winget-manifest.%s.%s.schema.json\n\n' "$1" "$schema"
}

refresh() {
  tag=${1:-$(gh release view --repo "$repo" --json tagName --jq .tagName)}
  ver=${tag#v}
  case "$tag" in v[0-9]*) ;; *) fail "that isn't a release tag: $tag" ;; esac
  case "$ver" in *[!0-9.]* | *..* | *.) fail "that isn't a release tag: $tag" ;; esac
  day=$(gh release view "$tag" --repo "$repo" --json publishedAt --jq '.publishedAt[:10]') || fail "no release $tag on github"
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  base="https://github.com/$repo/releases/download/$tag"
  curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS" || fail "can't fetch $tag's SHA256SUMS: is $tag out?"
  want=$(awk -v f="$asset" '$2 == f { print $1 }' "$tmp/SHA256SUMS")
  [ -n "$want" ] || fail "$tag has no $asset"
  curl -fsSL "$base/$asset" -o "$tmp/$asset" || fail "can't download $asset"
  got=$(sha256sum "$tmp/$asset" | cut -d' ' -f1)
  [ "$got" = "$want" ] || fail "$asset doesn't match the release's SHA256SUMS: not touching anything"
  sum=$(printf %s "$got" | tr '[:lower:]' '[:upper:]')
  mkdir -p "$dir"
  rm -f "$dir"/*.yaml
  {
    head_line version
    cat <<EOF
PackageIdentifier: $id
PackageVersion: $ver
DefaultLocale: en-US
ManifestType: version
ManifestVersion: $schema
EOF
  } > "$dir/$id.yaml"
  {
    head_line installer
    cat <<EOF
PackageIdentifier: $id
PackageVersion: $ver
MinimumOSVersion: 10.0.0.0
InstallerType: portable
Commands:
- heatsync-tui
ReleaseDate: $day
Installers:
- Architecture: x64
  InstallerUrl: $base/$asset
  InstallerSha256: $sum
ManifestType: installer
ManifestVersion: $schema
EOF
  } > "$dir/$id.installer.yaml"
  {
    head_line defaultLocale
    cat <<EOF
PackageIdentifier: $id
PackageVersion: $ver
PackageLocale: en-US
Publisher: heatsync
PublisherUrl: https://heatsync.org/
PublisherSupportUrl: https://github.com/$repo/issues
PackageName: heatsync-tui
PackageUrl: https://github.com/$repo
License: MIT
LicenseUrl: https://github.com/$repo/blob/HEAD/LICENSE
ShortDescription: Twitch, Kick and YouTube live chat in the terminal, with real emotes and vim keys.
Description: |-
  Live chat from Twitch, Kick and YouTube in tabs in one terminal. It reads without an account; log in with heatsync to send on Twitch and Kick (YouTube is read-only).
  7TV, BetterTTV, FrankerFaceZ and heatsync emotes are drawn as images where the terminal can show them (sixel or kitty graphics). Vim keys throughout.
Moniker: heatsync-tui
Tags:
- 7tv
- chat
- emotes
- kick
- live-chat
- multichat
- terminal
- tui
- twitch
- youtube
ReleaseNotesUrl: https://github.com/$repo/releases/tag/$tag
ManifestType: defaultLocale
ManifestVersion: $schema
EOF
  } > "$dir/$id.locale.en-US.yaml"
  git --no-pager diff --stat -- "$dir"
  echo "done: $dir is ready for $tag. read the diff, commit it, then: sh packaging/winget.sh pr"
}

pr() {
  [ -z "$(git status --porcelain -- "$dir")" ] || fail "$dir has changes that aren't committed: commit them first, so the repo records exactly what winget gets"
  ver=$(sed -n 's/^PackageVersion: //p' "$dir/$id.yaml")
  [ -n "$ver" ] || fail "no manifests in $dir: run sh packaging/winget.sh first"
  gh release view "v$ver" --repo "$repo" >/dev/null || fail "no release v$ver on github: nothing to publish"
  me=$(gh api user --jq .login) || fail "gh isn't logged in: gh auth login"
  fork="$me/winget-pkgs"
  gh repo view "$fork" >/dev/null 2>&1 || gh repo fork "$up" --clone=false >/dev/null || fail "can't fork $up"
  gh repo sync "$fork" --branch master >/dev/null || fail "can't bring $fork up to date with $up"
  pub=${id%%.*}
  name=${id#*.}
  path="manifests/$(printf %s "$pub" | cut -c1 | tr '[:upper:]' '[:lower:]')/$pub/$name"
  if gh api "repos/$up/contents/$path/$ver" >/dev/null 2>&1; then
    echo "winget already has $id $ver"
    return 0
  fi
  open=$(gh pr list --repo "$up" --state open --search "$id $ver in:title" --json url --jq '.[0].url // ""')
  [ -z "$open" ] || fail "a pull request for $id $ver is already open: $open"
  if gh api "repos/$up/contents/$path" >/dev/null 2>&1; then title="New version: $id version $ver"; else title="New package: $id version $ver"; fi
  branch="$(printf %s "$id" | tr '[:upper:]' '[:lower:]')-$ver"
  parent=$(gh api "repos/$fork/branches/master" --jq .commit.sha)
  tree=$(gh api "repos/$fork/git/commits/$parent" --jq .tree.sha)
  # one commit holding the three files, made on github: the fork is never cloned (winget-pkgs is huge)
  entries='[]'
  for f in "$dir"/*.yaml; do
    blob=$(jq -Rs '{content: ., encoding: "utf-8"}' < "$f" | gh api "repos/$fork/git/blobs" --input - --jq .sha)
    entries=$(printf %s "$entries" | jq -c --arg p "$path/$ver/${f##*/}" --arg s "$blob" '. + [{path: $p, mode: "100644", type: "blob", sha: $s}]')
  done
  tree=$(jq -n --arg b "$tree" --argjson e "$entries" '{base_tree: $b, tree: $e}' | gh api "repos/$fork/git/trees" --input - --jq .sha)
  # the commit is public: it carries github's private address for you, never your own
  noreply="$(gh api user --jq .id)+$me@users.noreply.github.com"
  commit=$(jq -n --arg m "$title" --arg t "$tree" --arg p "$parent" --arg n "$me" --arg e "$noreply" \
    '{message: $m, tree: $t, parents: [$p], author: {name: $n, email: $e}}' | gh api "repos/$fork/git/commits" --input - --jq .sha)
  gh api "repos/$fork/git/refs" -f ref="refs/heads/$branch" -f sha="$commit" >/dev/null || fail "can't make branch $branch on $fork (left from an earlier try? delete it there first)"
  echo "opening the pull request on $up as $me"
  gh pr create --repo "$up" --base master --head "$me:$branch" --title "$title" --body "## 📖 Description

$title: the portable windows exe from https://github.com/$repo/releases/tag/v$ver, its hash checked against that release's SHA256SUMS.

## 📦 Manifest Checklist

- [x] Checked that there aren't other open pull requests for the same manifest update/change
- [x] This PR only modifies one (1) manifest"
}

case "${1:-}" in
  pr) [ "$#" -eq 1 ] || fail "usage: sh packaging/winget.sh [vX.Y.Z | pr]"; pr ;;
  "" | v*) [ "$#" -le 1 ] || fail "usage: sh packaging/winget.sh [vX.Y.Z | pr]"; refresh "${1:-}" ;;
  *) fail "usage: sh packaging/winget.sh [vX.Y.Z | pr]" ;;
esac
