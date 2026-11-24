#!/bin/sh
set -eu

exec 3>&1 1>&2

here=$(cd "$(dirname "$0")" && pwd)
manual="Run this in a terminal to download it, then reconnect the server: sh \"$here/endeavor-mcp.sh\" --fetch-only"

hook=
if [ "${1:-}" = --hook ]; then
  hook=1
  shift
fi

fail() {
  [ -z "$hook" ] || exit 0
  echo "endeavor: $*" >&2
  exit 1
}

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

if [ -n "$hook" ] && [ -n "${ENDEAVOR_BIN:-}" ]; then
  [ ! -x "$ENDEAVOR_BIN" ] || "$ENDEAVOR_BIN" hook 3>&- || true
  exit 0
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
      *) fail "HOME isn't set to an absolute path, so there is nowhere to keep endeavor. $manual" ;;
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

if [ -n "$hook" ]; then
  ! have || "$bin" hook 3>&- || true
  exit 0
fi

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

if have && { [ -n "$key" ] || [ -z "$fetch_only" ]; }; then
  run "$@"
fi

mkdir -p "$base" || fail "couldn't create $base. $manual"
lock=$base/.lock

stale() {
  if [ -z "$1" ]; then
    [ -n "$(find "$lock" -maxdepth 0 -mmin +2 2>/dev/null)" ]
  else
    ! kill -0 "$1" 2>/dev/null || [ -n "$(find "$lock" -maxdepth 0 -mmin +20 2>/dev/null)" ]
  fi
}

waited=0
until err=$(mkdir "$lock" 2>&1); do
  if [ ! -d "$lock" ]; then
    err=$(mkdir "$lock" 2>&1) && break
    [ -d "$lock" ] || fail "can't make $lock, so $base can't be written ($err). $manual"
  fi
  owner=$(cat "$lock/pid" 2>/dev/null || true)
  if stale "$owner"; then
    if mkdir "$lock.take" 2>/dev/null; then
      trap 'rmdir "$lock.take" 2>/dev/null; exit 1' INT TERM HUP
      if [ "$(cat "$lock/pid" 2>/dev/null || true)" = "$owner" ] && stale "$owner"; then
        rm -rf "$lock"
      fi
      rmdir "$lock.take" 2>/dev/null || true
      trap - INT TERM HUP
      continue
    fi
    [ -z "$(find "$lock.take" -maxdepth 0 -mmin +1 2>/dev/null)" ] || rmdir "$lock.take" 2>/dev/null || true
  fi
  if [ "$waited" -ge 25 ]; then
    fail "another start has been downloading endeavor for $waited seconds. Reconnect in a minute. $manual"
  fi
  sleep 1
  waited=$((waited + 1))
  if have && { [ -n "$key" ] || [ -z "$fetch_only" ]; }; then
    run "$@"
  fi
done

unlock() {
  if [ "$(cat "$lock/pid" 2>/dev/null || true)" = "$$" ]; then
    rm -rf "$lock"
  fi
}
trap unlock EXIT
trap 'unlock; exit 1' INT TERM HUP
echo $$ >"$lock/pid"

find "$base" -maxdepth 1 -name '.endeavor-install.*' -mmin +60 -exec rm -rf {} + >/dev/null 2>&1 || true

if [ ! -f "$here/install.sh" ]; then
  fail "install.sh is missing from $here. Reinstall the plugin."
fi
status=0
if [ -n "$key" ]; then
  have || sh "$here/install.sh" --quiet --into "$base" --key "$key" --sums "$here/release-key" </dev/null || status=$?
else
  : >"$base/.checked" 2>/dev/null || true
  sh "$here/install.sh" --quiet --into "$base" </dev/null || true
fi

if have; then
  trap - EXIT INT TERM HUP
  unlock
  run "$@"
fi
if [ "$status" -eq 3 ]; then
  root=$(dirname "$here")
  if [ -d "$root/.claude-plugin" ]; then
    update="In Claude Code, run \`claude plugin marketplace update endeavor\` and then \`claude plugin update endeavor@endeavor\`."
  elif [ -f "$root/mcp_config.json" ]; then
    update="Install it again with \`agy plugin install https://github.com/jowch/EndeavorMCP/tree/main/antigravity-plugin\`."
  elif [ -f "$root/mcp.json" ]; then
    update="In Codex, run \`codex plugin marketplace upgrade endeavor\`, then start Codex again."
  else
    update="Update it the way you installed it."
  fi
  fail "the release doesn't hold build $key for this computer. If the plugin was just updated, reconnect in a few minutes. Otherwise this build is old and was removed: update the plugin. $update"
fi
fail "couldn't get endeavor${key:+ (build $key)}. The reason is above. $manual"
