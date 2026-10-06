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
# asking the network, and --fetch-only looks for a newer one. Unpinned, a start
# also asks for a newer one in the background, once a day, after starting the
# server; the result applies from the next start.
#
# Binaries live in ${XDG_DATA_HOME:-~/.local/share}/endeavor/bin/<key>/, the
# same for every agent (XDG_DATA_HOME counts only if it is an absolute path).
# They are fetched by install.sh, next to this file, under a lock so that two
# starts download once.
#
# ENDEAVOR_BIN names a binary to run instead, to try the plugin before a
# release holds its build. ENDEAVOR_RELEASE_URL (tests and development) fetches
# from another release, into bin-from/<its address>/ beside bin/, so that it
# can't put a binary where a normal start would run it.
#
# stdout belongs to the MCP server: it is sent to stderr until the exec, which
# gets it back.
set -eu

exec 3>&1 1>&2

here=$(cd "$(dirname "$0")" && pwd)
manual='curl -fsSL https://raw.githubusercontent.com/jowch/EndeavorMCP/main/scripts/install.sh | sh (Windows: scripts/install.ps1)'

fail() {
  echo "endeavor: $*" >&2
  exit 1
}

# A build's key is hexadecimal; it is used as a folder name.
is_key() {
  case $1 in
    '' | *[!0-9a-fA-F]*) return 1 ;;
  esac
}

fetch_only=
if [ "${1:-}" = --fetch-only ]; then
  fetch_only=1
  shift
fi

if [ -n "${ENDEAVOR_BIN:-}" ]; then
  [ -x "$ENDEAVOR_BIN" ] || fail "ENDEAVOR_BIN ($ENDEAVOR_BIN) isn't an executable file."
  [ -z "$fetch_only" ] || exit 0
  exec "$ENDEAVOR_BIN" mcp "$@" 1>&3 3>&-
fi

case ${XDG_DATA_HOME:-} in
  /*) data=$XDG_DATA_HOME ;;
  *)
    case ${HOME:-} in
      /*) data=$HOME/.local/share ;;
      *) fail "HOME isn't set to an absolute path, so there is nowhere to keep endeavor. Install it by hand: $manual" ;;
    esac
    ;;
esac
base=$data/endeavor/bin
if [ -n "${ENDEAVOR_RELEASE_URL:-}" ]; then
  base=$data/endeavor/bin-from/$(printf '%s' "$ENDEAVOR_RELEASE_URL" | cksum | tr ' ' -)
fi
exe=endeavor
case $(uname -s) in
  MINGW* | MSYS* | CYGWIN*) exe=endeavor.exe ;;
esac

key=
if [ -f "$here/release-key" ]; then
  key=$(head -n 1 "$here/release-key" | tr -d ' \t\r\n')
fi
if [ -n "$key" ] && ! is_key "$key"; then
  fail "release-key in $here doesn't hold a build's key."
fi

# The binary to run, or nothing.
have() {
  bin=
  if [ -n "$key" ]; then
    bin=$base/$key/$exe
  elif [ -f "$base/.newest" ]; then
    newest=$(head -n 1 "$base/.newest" | tr -d ' \t\r\n')
    ! is_key "$newest" || bin=$base/$newest/$exe
  fi
  [ -n "$bin" ] && [ -x "$bin" ]
}

# Once a day, unpinned: look for a newer build in a process of its own, which
# neither holds the server's input and output nor delays its start.
check_later() {
  [ -z "$key" ] && [ -d "$base" ] || return 0
  [ -z "$(find "$base/.checked" -mmin -1440 2>/dev/null)" ] || return 0
  : >"$base/.checked" 2>/dev/null || return 0
  sh "$here/endeavor-mcp.sh" --fetch-only </dev/null >/dev/null 2>&1 3>&- &
}

run() {
  [ -z "$fetch_only" ] || exit 0
  [ -n "$key" ] || check_later
  exec "$bin" mcp "$@" 1>&3 3>&-
}

# A fetch-only start asks for a newer build when none is pinned.
if have && { [ -n "$key" ] || [ -z "$fetch_only" ]; }; then
  run "$@"
fi

mkdir -p "$base" || fail "couldn't create $base. Install endeavor by hand: $manual"
lock=$base/.lock
waited=0
until err=$(mkdir "$lock" 2>&1); do
  if [ ! -d "$lock" ]; then
    # The owner may have just let go of it.
    err=$(mkdir "$lock" 2>&1) && break
    [ -d "$lock" ] || fail "can't make $lock, so $base can't be written ($err). Install endeavor by hand: $manual"
  fi
  # The lock folder is made when the lock is taken, and its age is the lock's.
  owner=$(cat "$lock/pid" 2>/dev/null || true)
  stale=
  if [ -z "$owner" ]; then
    [ -z "$(find "$lock" -maxdepth 0 -mmin +2 2>/dev/null)" ] || stale=1
  elif ! kill -0 "$owner" 2>/dev/null || [ -n "$(find "$lock" -maxdepth 0 -mmin +20 2>/dev/null)" ]; then
    stale=1
  fi
  if [ -n "$stale" ]; then
    # Only one start's rename succeeds.
    if mv "$lock" "$lock.stale.$$" 2>/dev/null; then
      # If another start took the lock over first, this moved its new one: give it back.
      if [ "$(cat "$lock.stale.$$/pid" 2>/dev/null || true)" != "$owner" ] && [ ! -e "$lock" ]; then
        mv "$lock.stale.$$" "$lock" 2>/dev/null || true
      fi
      rm -rf "$lock.stale.$$"
      continue
    fi
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

# Only a lock this start made is removed.
unlock() {
  if [ "$(cat "$lock/pid" 2>/dev/null || true)" = "$$" ]; then
    rm -rf "$lock"
  fi
}
trap unlock EXIT
trap 'unlock; exit 1' INT TERM HUP
echo $$ >"$lock/pid"

# A download cut short leaves its temporary folder behind.
find "$base" -maxdepth 1 -name '.endeavor-install.*' -mmin +60 -exec rm -rf {} + >/dev/null 2>&1 || true

if [ ! -f "$here/install.sh" ]; then
  fail "install.sh is missing from $here. Install endeavor by hand: $manual"
fi
if [ -n "$key" ]; then
  have || sh "$here/install.sh" --quiet --into "$base" --key "$key" </dev/null || true
else
  : >"$base/.checked" 2>/dev/null || true
  sh "$here/install.sh" --quiet --into "$base" </dev/null || true
fi

if have; then
  trap - EXIT INT TERM HUP
  unlock
  run "$@"
fi
fail "couldn't get endeavor${key:+ (build $key)}. The reason is above. Install it by hand: $manual"
