#!/bin/sh
# Lists the Helpers release's files for builds nothing needs any more, and with
# --delete removes them. GitHub allows 1000 files on one release, and every
# build adds six, so without this the Helpers workflow would start failing.
#
#   scripts/prune-helpers.sh                     list what would go (changes nothing)
#   scripts/prune-helpers.sh --delete            delete it
#   scripts/prune-helpers.sh --delete --above N  delete only if the release holds
#                                                more than N files
#
# A build (its key, as in endeavor-<key>-<platform>) is kept when:
#   - LATEST names it (install.sh, install.ps1, `endeavor update`);
#   - a release-key file names it at the head of any branch here, or it is one
#     of the last PLUGIN_PINS (40) builds main's release-key has named: a plugin
#     installed from such a commit downloads that build, and `endeavor mcp`
#     fetches its Linux helper for a server;
#   - Endeavor's Cargo.lock pins the commit it was built from, at the head of
#     any of Endeavor's branches or as one of the last APP_PINS (10) pins on
#     its main (the app's scripts/helpers.sh downloads it to bundle);
#   - it was published in the last NEW_DAYS days (7): a binary installed from
#     LATEST then fetches the Linux helper of its own build.
# Pins are counted rather than dated so that what is kept stays bounded however
# often main moves: 40 pins are at most 240 files.
# LATEST itself and any file whose name holds no key are never deleted. If any
# of this can't be worked out (a clone, a pinned commit, the release), the
# script stops before deleting anything.
#
# Needs git, curl and jq. It clones both repositories itself (no file
# contents beyond what it reads), so it runs the same in CI and in any folder.
# Deleting needs GH_TOKEN with contents: write; listing works without one, but
# a token avoids the API's rate limit. ENDEAVOR_MCP_REPO and ENDEAVOR_APP_REPO
# name other repositories (owner/name).
set -eu

mcp_repo=${ENDEAVOR_MCP_REPO:-jowch/EndeavorMCP}
app_repo=${ENDEAVOR_APP_REPO:-jowch/Endeavor}
plugin_pins=${PLUGIN_PINS:-40}
app_pins=${APP_PINS:-10}
new_days=${NEW_DAYS:-7}
tag=helpers
# The kept builds' files above this many get a warning: pruning can't help then.
warn_at=800

delete=
above=0
while [ $# -gt 0 ]; do
  case $1 in
    --delete) delete=1 ;;
    --above)
      [ $# -ge 2 ] || { echo "--above needs a number." >&2; exit 2; }
      above=$2
      shift
      ;;
    *)
      echo "usage: prune-helpers.sh [--delete [--above N]]" >&2
      exit 2
      ;;
  esac
  shift
done
case $above$plugin_pins$app_pins$new_days in
  *[!0-9]*) echo "--above, PLUGIN_PINS, APP_PINS and NEW_DAYS take whole numbers." >&2; exit 2 ;;
esac

fail() {
  echo "prune-helpers: $*" >&2
  echo "prune-helpers: nothing was deleted." >&2
  exit 1
}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
trap 'rm -rf "$work"; exit 1' INT TERM HUP

api() {
  if [ -n "${GH_TOKEN:-}" ]; then
    curl -fsSL -H "Accept: application/vnd.github+json" -H "Authorization: Bearer $GH_TOKEN" "$@"
  else
    curl -fsSL -H "Accept: application/vnd.github+json" "$@"
  fi
}

# The moment `days` days ago, in the API's format (GNU date, then BSD's).
ago() {
  date -u -d "$1 days ago" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -v-"$1"d +%Y-%m-%dT%H:%M:%SZ
}
new_since=$(ago "$new_days") || fail "couldn't work out the date $new_days days ago."

# The release's files, as "id name created_at" lines.
release=$(api "https://api.github.com/repos/$mcp_repo/releases/tags/$tag") || fail "couldn't read the $tag release of $mcp_repo."
id=$(printf '%s' "$release" | jq -r .id)
case $id in '' | null | *[!0-9]*) fail "the $tag release of $mcp_repo has no id." ;; esac
: >"$work/assets"
page=1
while :; do
  got=$(api "https://api.github.com/repos/$mcp_repo/releases/$id/assets?per_page=100&page=$page") || fail "couldn't list the release's files (page $page)."
  n=$(printf '%s' "$got" | jq length)
  [ "$n" -gt 0 ] || break
  printf '%s' "$got" | jq -r '.[] | "\(.id) \(.name) \(.created_at)"' >>"$work/assets"
  page=$((page + 1))
done
total=$(wc -l <"$work/assets" | tr -d ' ')
[ "$total" -gt 0 ] || fail "the release lists no files; that isn't believable, so nothing is pruned."

# The build key in a file's name, or nothing (the old endeavor-remote-<key> names count too).
key_of() {
  printf '%s\n' "$1" | sed -n 's/^endeavor-\(remote-\)\{0,1\}\([0-9a-f]\{12\}\)[-.].*/\2/p'
}

# The builds the release holds, oldest first.
sort -k3 "$work/assets" | awk '{ print $2 }' | while read -r name; do key_of "$name"; done | awk 'NF && !seen[$0]++' >"$work/release-keys"

# keep: "key reason" lines.
: >"$work/keep"
keep() {
  printf '%s %s\n' "$1" "$2" >>"$work/keep"
}

latest=$(curl -fsSL "https://github.com/$mcp_repo/releases/download/$tag/LATEST" | tr -d ' \t\r\n') || latest=
case $latest in
  [0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]) keep "$latest" LATEST ;;
  *) fail "couldn't read the key LATEST names ('$latest')." ;;
esac

git clone -q --bare --filter=blob:none "https://github.com/$mcp_repo.git" "$work/mcp.git" || fail "couldn't clone $mcp_repo."
git clone -q --bare --filter=blob:none "https://github.com/$app_repo.git" "$work/app.git" || fail "couldn't clone $app_repo."

# Every release-key file in commit $2 of repository $1, one key per line;
# "?" for one that can't be read.
release_keys() {
  paths=$(git -C "$1" ls-tree -r --name-only "$2") || { echo "?"; return; }
  printf '%s\n' "$paths" | grep -E '(^|/)release-key$' | while read -r path; do
    text=$(git -C "$1" show "$2:$path") || { echo "?"; continue; }
    printf '%s\n' "$text" | head -n 1 | tr -d ' \t\r\n'
    echo
  done
}

# Plugin pins: every branch head, then main's commits that changed a
# release-key, newest first, until plugin_pins builds have been seen.
git -C "$work/mcp.git" for-each-ref --format='%(objectname) %(refname:short)' refs/heads >"$work/mcp-heads"
while read -r commit branch; do
  for k in $(release_keys "$work/mcp.git" "$commit"); do
    [ "$k" != "?" ] || fail "couldn't read release-key at the head of $branch."
    keep "$k" "pinned by the plugin on $branch"
  done
done <"$work/mcp-heads"
git -C "$work/mcp.git" rev-list --full-history main -- scripts/release-key '*/launch/release-key' >"$work/mcp-pins" ||
  fail "couldn't read main's history."
: >"$work/plugin-keys"
while read -r commit; do
  [ "$(wc -l <"$work/plugin-keys")" -lt "$plugin_pins" ] || break
  for k in $(release_keys "$work/mcp.git" "$commit"); do
    [ "$k" != "?" ] || fail "couldn't read release-key at $(echo "$commit" | cut -c1-7)."
    [ -n "$k" ] || continue
    # A key Helpers never built (pinned on a branch, then merged) uses up none of the count.
    grep -qx "$k" "$work/release-keys" || continue
    grep -qx "$k" "$work/plugin-keys" || echo "$k" >>"$work/plugin-keys"
    keep "$k" "pinned by the plugin on main at $(echo "$commit" | cut -c1-7)"
  done
done <"$work/mcp-pins"

# App pins: the endeavor-mcp commit in Endeavor's Cargo.lock at every branch
# head, then on main's commits that changed Cargo.lock, newest first, until
# app_pins of them have been seen. Each is keyed by that commit's own helpers.sh.
pin_at() {
  git -C "$work/app.git" show "$1:Cargo.lock" 2>/dev/null | awk '
    /^name = "endeavor-mcp"$/ { found = 1; next }
    found && /^source = / { if (match($0, /#[0-9a-f]+/)) print substr($0, RSTART + 1, RLENGTH - 1); exit }
    /^\[\[package\]\]/ { found = 0 }'
}
: >"$work/app-pins"
git -C "$work/app.git" for-each-ref --format='%(objectname) %(refname:short)' refs/heads >"$work/app-heads"
while read -r commit branch; do
  pin=$(pin_at "$commit") || true
  [ -z "$pin" ] || grep -q "^$pin " "$work/app-pins" || echo "$pin $branch" >>"$work/app-pins"
done <"$work/app-heads"
git -C "$work/app.git" rev-list --full-history main -- Cargo.lock >"$work/app-history" || fail "couldn't read $app_repo's history."
: >"$work/app-seen"
while read -r commit; do
  [ "$(wc -l <"$work/app-seen")" -lt "$app_pins" ] || break
  pin=$(pin_at "$commit") || true
  [ -n "$pin" ] || continue
  grep -qx "$pin" "$work/app-seen" || echo "$pin" >>"$work/app-seen"
  grep -q "^$pin " "$work/app-pins" || echo "$pin main at $(echo "$commit" | cut -c1-7)" >>"$work/app-pins"
done <"$work/app-history"

while read -r pin where; do
  short=$(echo "$pin" | cut -c1-7)
  if ! git -C "$work/mcp.git" cat-file -e "$pin^{commit}" 2>/dev/null; then
    git -C "$work/mcp.git" fetch -q origin "$pin" 2>/dev/null || fail "Endeavor ($where) pins $mcp_repo at $short, which can't be fetched."
  fi
  git -C "$work/mcp.git" show "$pin:scripts/helpers.sh" 2>/dev/null | grep -q -- '--key)' ||
    fail "Endeavor ($where) pins $short, whose scripts/helpers.sh can't print a key."
  tree=$work/tree-$pin
  git -C "$work/mcp.git" worktree add -q --detach "$tree" "$pin" || fail "couldn't check out $short."
  k=$(cd "$tree" && sh scripts/helpers.sh --key) || fail "scripts/helpers.sh --key failed at $short."
  git -C "$work/mcp.git" worktree remove --force "$tree"
  case $k in
    [0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]) ;;
    *) fail "scripts/helpers.sh --key at $short printed '$k'." ;;
  esac
  keep "$k" "pinned by the app on $where ($short)"
done <"$work/app-pins"

# Recently published builds.
while read -r asset name created; do
  k=$(key_of "$name")
  [ -z "$k" ] || [ "$created" \< "$new_since" ] || keep "$k" "published in the last $new_days days"
done <"$work/assets"

# Every file is kept or goes; print by build, oldest first.
: >"$work/go"
kept=0
cp "$work/release-keys" "$work/keys"
echo "The $tag release of $mcp_repo holds $total files."
echo
while read -r k; do
  files=$(awk -v k="$k" '{ n = $2; sub(/^endeavor-remote-/, "endeavor-", n) } index(n, "endeavor-" k) == 1 { print }' "$work/assets")
  count=$(printf '%s\n' "$files" | wc -l | tr -d ' ')
  when=$(printf '%s\n' "$files" | awk '{ print $3 }' | sort | head -n 1 | cut -c1-10)
  why=$(awk -v k="$k" '$1 == k { $1 = ""; sub(/^ /, ""); print }' "$work/keep" | awk '!seen[$0]++' | head -n 1)
  if [ -n "$why" ]; then
    echo "keep    $k  $count files  $when  $why"
  else
    echo "delete  $k  $count files  $when"
    printf '%s\n' "$files" >>"$work/go"
  fi
done <"$work/keys"
awk '{ print $2 }' "$work/assets" | while read -r name; do
  [ -n "$(key_of "$name")" ] || echo "keep    $name (no build key in its name)"
done

gone=$(grep -c . "$work/go" || true)
left=$((total - gone))
echo
echo "$gone files would go and $left would stay."
if [ "$left" -gt "$warn_at" ]; then
  msg="The $tag release keeps $left files after pruning; GitHub allows 1000. Shorten PIN_DAYS or NEW_DAYS, or move old builds to another release."
  echo "$msg" >&2
  [ -z "${GITHUB_ACTIONS:-}" ] || echo "::warning::$msg"
fi

[ -n "$delete" ] || { echo "Nothing was deleted (run with --delete to delete)."; exit 0; }
if [ "$total" -le "$above" ]; then
  echo "The release holds $total files, not more than $above, so nothing was deleted."
  exit 0
fi
[ "$gone" -gt 0 ] || exit 0
[ -n "${GH_TOKEN:-}" ] || fail "deleting needs GH_TOKEN."
while read -r asset name created; do
  [ "$name" != LATEST ] || fail "LATEST was about to be deleted; this is a bug."
  api -X DELETE "https://api.github.com/repos/$mcp_repo/releases/assets/$asset" >/dev/null || fail "couldn't delete $name (files listed before it are gone)."
  echo "deleted $name"
done <"$work/go"
