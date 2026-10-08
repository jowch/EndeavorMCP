#!/bin/sh
# Keeps the plugin folders in step with their one source each:
#
#   plugin/skills/                         the skills
#   scripts/endeavor-mcp.sh, install.sh    the launcher and its installer
#   scripts/release-key                    the pinned build (empty: the newest)
#
# `sync` writes the copies; `check` fails if any copy differs. Each plugin
# holds the launcher files in launch/ and a copy of the skills, not a link: an
# agent may not follow a link out of the plugin's folder, and a checkout
# without symlink support turns a link into a text file.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

mode=${1:-}
case $mode in
  sync | check) ;;
  *)
    echo "usage: plugins.sh sync|check" >&2
    exit 2
    ;;
esac

bad=0
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
