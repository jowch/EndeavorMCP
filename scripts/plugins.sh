#!/bin/sh
# Keeps the plugin folders in step with their one source each:
#
#   plugin/skills/                         the skills
#   scripts/endeavor-mcp.sh, install.sh    the launcher and its installer
#   scripts/release-key                    the pinned build (empty: the newest)
#
# `sync` writes the copies; `check` fails if any copy differs. Each plugin
# holds the launcher files in launch/. claude-plugin/skills is a link to
# plugin/skills; the other two plugins hold a copy, since an agent may not
# follow a link out of the plugin's folder.
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
  if [ "$plugin" = claude-plugin ]; then
    if [ "$(readlink claude-plugin/skills 2>/dev/null)" != ../plugin/skills ]; then
      echo "claude-plugin/skills isn't a link to ../plugin/skills" >&2
      bad=1
    fi
  elif [ "$mode" = sync ]; then
    rm -rf "$plugin/skills"
    cp -R plugin/skills "$plugin/skills"
  elif ! diff -r plugin/skills "$plugin/skills" >&2; then
    echo "$plugin/skills differs from plugin/skills" >&2
    bad=1
  fi
done
exit $bad
