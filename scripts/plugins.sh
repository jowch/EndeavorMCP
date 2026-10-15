#!/bin/sh
# Keeps the plugin folders in step with their one source each:
#
#   plugin/skills/                         the skills
#   scripts/endeavor-mcp.sh, install.sh    the launcher and its installer
#   scripts/release-key                    the pinned build
#
# `sync` writes the copies; `check` fails if any copy differs, or if
# release-key isn't the build of this commit's helper source
# (scripts/helpers.sh --key), so that a plugin's skills and binary come from
# one commit. `repin` writes that build into release-key, then syncs: run it
# after committing a change to the helper source, and commit what it changes.
# Each plugin holds the launcher files in launch/ and a copy of the skills,
# not a link: an agent may not follow a link out of the plugin's folder, and a
# checkout without symlink support turns a link into a text file.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

mode=${1:-}
case $mode in
  sync | check | repin) ;;
  *)
    echo "usage: plugins.sh sync|check|repin" >&2
    exit 2
    ;;
esac

if [ "$mode" = repin ]; then
  # The key hashes the committed tree, so uncommitted helper source would pin
  # a build that doesn't hold it.
  if ! sh scripts/helpers.sh --committed; then
    echo "The helper source has uncommitted changes. Commit them, then run: sh scripts/plugins.sh repin" >&2
    exit 1
  fi
  key=$(sh scripts/helpers.sh --key)
  [ -n "$key" ] || { echo "scripts/helpers.sh --key printed no key." >&2; exit 1; }
  printf '%s\n' "$key" >scripts/release-key
  echo "Pinned build $key."
  mode=sync
fi

bad=0
if [ "$mode" = check ]; then
  want=$(sh scripts/helpers.sh --key)
  if [ "$(head -n 1 scripts/release-key | tr -d ' \t\r\n')" != "$want" ]; then
    echo "scripts/release-key doesn't name this commit's build ($want). With the helper source change committed, run: sh scripts/plugins.sh repin" >&2
    bad=1
  fi
fi
for plugin in claude-plugin codex-plugin antigravity-plugin; do
  if [ "$mode" = sync ]; then
    mkdir -p "$plugin/launch"
  fi
  for file in endeavor-mcp.sh install.sh release-key; do
    if [ "$mode" = sync ]; then
      cp "scripts/$file" "$plugin/launch/$file"
    elif ! cmp -s "scripts/$file" "$plugin/launch/$file"; then
      echo "$plugin/launch/$file differs from scripts/$file" >&2
      bad=1
    fi
  done
  if [ "$mode" = check ]; then
    for have in "$plugin"/launch/*; do
      [ -e "$have" ] || continue
      [ -f "scripts/$(basename "$have")" ] || { echo "$have has no source in scripts/" >&2; bad=1; }
    done
  fi
  if [ "$mode" = sync ]; then
    rm -rf "$plugin/skills"
    cp -R plugin/skills "$plugin/skills"
  elif [ -L "$plugin/skills" ]; then
    echo "$plugin/skills is a link; it must be a copy of plugin/skills" >&2
    bad=1
  elif ! diff -r plugin/skills "$plugin/skills" >&2; then
    echo "$plugin/skills differs from plugin/skills" >&2
    bad=1
  fi
done
exit $bad
