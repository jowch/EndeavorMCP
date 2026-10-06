#!/bin/sh
# The plugin's MCP command in every agent: runs `endeavor mcp` with the
# arguments it is given, getting the binary first if this computer has none for
# the plugin's build.
#
#   endeavor-mcp.sh --skills plugin --folder DIR   run the server
#   endeavor-mcp.sh --fetch-only                   only get the binary
#
# The build is the first line of release-key, next to this file. Empty or
# missing, it is the newest build: one that is already here is used without
# asking the network, and --fetch-only looks for a newer one.
#
# Binaries live in ${XDG_DATA_HOME:-~/.local/share}/endeavor/bin/<key>/, the
# same for every agent. They are fetched by install.sh, next to this file, under
# a lock so that two starts download once.
#
# ENDEAVOR_BIN names a binary to run instead, to try the plugin before a
# release holds its build.
#
# stdout belongs to the MCP server: before the exec, this writes only to stderr.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
manual='curl -fsSL https://raw.githubusercontent.com/jowch/EndeavorMCP/main/scripts/install.sh | sh (Windows: scripts/install.ps1)'

fail() {
  echo "endeavor: $*" >&2
  exit 1
}

fetch_only=
if [ "${1:-}" = --fetch-only ]; then
  fetch_only=1
  shift
fi

if [ -n "${ENDEAVOR_BIN:-}" ]; then
  [ -x "$ENDEAVOR_BIN" ] || fail "ENDEAVOR_BIN ($ENDEAVOR_BIN) isn't an executable file."
  [ -z "$fetch_only" ] || exit 0
  exec "$ENDEAVOR_BIN" mcp "$@"
fi

[ -n "${HOME:-}" ] || fail "HOME isn't set, so there is nowhere to keep endeavor. Install it by hand: $manual"
base=${XDG_DATA_HOME:-$HOME/.local/share}/endeavor/bin
exe=endeavor
case $(uname -s) in
  MINGW* | MSYS* | CYGWIN*) exe=endeavor.exe ;;
esac

key=
if [ -f "$here/release-key" ]; then
  key=$(head -n 1 "$here/release-key" | tr -d ' \t\r\n')
fi
case $key in
  *[!0-9a-fA-F]*) fail "release-key in $here doesn't hold a build's key." ;;
esac

# The binary to run, or nothing.
have() {
  if [ -n "$key" ]; then
    bin=$base/$key/$exe
  else
    bin=
    [ ! -f "$base/.newest" ] || bin=$base/$(head -n 1 "$base/.newest" | tr -d ' \t\r\n')/$exe
  fi
  [ -n "$bin" ] && [ -x "$bin" ]
}

run() {
  [ -z "$fetch_only" ] || exit 0
  exec "$bin" mcp "$@"
}

# A fetch-only start asks for a newer build when none is pinned.
if have && { [ -n "$key" ] || [ -z "$fetch_only" ]; }; then
  run "$@"
fi

mkdir -p "$base" || fail "couldn't create $base. Install endeavor by hand: $manual"
lock=$base/.lock
waited=0
until mkdir "$lock" 2>/dev/null; do
  owner=$(cat "$lock/pid" 2>/dev/null || true)
  if { [ -n "$owner" ] && ! kill -0 "$owner" 2>/dev/null; } || { [ -z "$owner" ] && [ -n "$(find "$lock" -maxdepth 0 -mmin +2 2>/dev/null)" ]; }; then
    rm -rf "$lock"
    continue
  fi
  if [ "$waited" -ge 25 ]; then
    fail "another start has been downloading endeavor for $waited seconds. Reconnect in a minute, or install it by hand: $manual"
  fi
  sleep 1
  waited=$((waited + 1))
  # The other start may have finished.
  if have && { [ -n "$key" ] || [ -z "$fetch_only" ]; }; then
    run "$@"
  fi
done
trap 'rm -rf "$lock"' EXIT
trap 'rm -rf "$lock"; exit 1' INT TERM HUP
echo $$ >"$lock/pid"

# A download cut short leaves its temporary folder behind.
find "$base" -maxdepth 1 -name '.endeavor-install.*' -mmin +60 -exec rm -rf {} + 2>/dev/null || true

if [ ! -f "$here/install.sh" ]; then
  fail "install.sh is missing from $here. Install endeavor by hand: $manual"
fi
if [ -n "$key" ]; then
  have || sh "$here/install.sh" --quiet --into "$base" --key "$key" </dev/null >&2 || true
else
  sh "$here/install.sh" --quiet --into "$base" </dev/null >&2 || true
fi

if have; then
  trap - EXIT INT TERM HUP
  rm -rf "$lock"
  run "$@"
fi
fail "couldn't get endeavor${key:+ (build $key)}. The reason is above. Install it by hand: $manual"
