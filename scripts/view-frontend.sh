#!/bin/sh
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
copy="$here/crates/endeavor-mcp/src/views/endeavor-view.js"
url="https://raw.githubusercontent.com/jowch/Endeavor/main/frontend/dist/view.js"
fetch() { curl -fsSL --retry 3 "$url" -o "$1"; }
case "${1:-}" in
check)
  tmp=$(mktemp)
  trap 'rm -f "$tmp"' EXIT
  fetch "$tmp"
  if ! cmp -s "$tmp" "$copy"; then
    echo "The notebook view's copy of Endeavor's frontend/dist/view.js differs from Endeavor's main."
    echo "Copy it here: sh scripts/view-frontend.sh sync"
    exit 1
  fi
  echo "endeavor-view.js matches Endeavor's main"
  ;;
sync)
  if [ -n "${2:-}" ]; then cp "$2" "$copy"; else fetch "$copy"; fi
  echo "copied to $copy"
  ;;
*)
  echo "usage: sh scripts/view-frontend.sh check | sync [file]" >&2
  exit 2
  ;;
esac
