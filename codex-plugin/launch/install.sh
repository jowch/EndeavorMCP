#!/bin/sh
set -eu

release=${ENDEAVOR_RELEASE_URL:-https://github.com/jowch/EndeavorMCP/releases/download/helpers}
release=${release%/}
if [ -n "${ENDEAVOR_PIN_URL+set}" ]; then
  pin_url=$ENDEAVOR_PIN_URL
  [ "$pin_url" != none ] || pin_url=
elif [ -n "${ENDEAVOR_RELEASE_URL:-}" ]; then
  pin_url=
else
  pin_url=https://raw.githubusercontent.com/jowch/EndeavorMCP/main/scripts/release-key
fi
dir=${ENDEAVOR_INSTALL_DIR:-}
into=
key=
quiet=
sums=

fail() {
  echo "install.sh: $*" >&2
  exit 1
}

is_key() {
  case $1 in
    '' | *[!0-9a-fA-F]*) return 1 ;;
  esac
}

while [ $# -gt 0 ]; do
  case $1 in
    --dir)
      [ $# -ge 2 ] || fail "--dir needs a folder."
      dir=$2
      shift 2
      ;;
    --dir=*)
      dir=${1#--dir=}
      shift
      ;;
    --into)
      [ $# -ge 2 ] || fail "--into needs a folder."
      into=$2
      shift 2
      ;;
    --key)
      [ $# -ge 2 ] || fail "--key needs a build."
      key=$2
      shift 2
      ;;
    --sums)
      [ $# -ge 2 ] || fail "--sums needs a file."
      sums=$2
      shift 2
      ;;
    --quiet)
      quiet=1
      shift
      ;;
    -h | --help)
      echo "usage: install.sh [--dir FOLDER] [--key BUILD]   install endeavor into FOLDER (default \$ENDEAVOR_INSTALL_DIR, else ~/.local/bin), the newest build or BUILD"
      exit 0
      ;;
    *) fail "unknown argument $1 (usage: install.sh [--dir FOLDER] [--key BUILD])" ;;
  esac
done

[ -z "$quiet" ] || exec 1>&2
if [ -n "$key" ] && ! is_key "$key"; then
  fail "--key '$key' isn't a build's key."
fi
[ -z "$sums" ] || [ -n "$key" ] || fail "--sums needs --key: the checksums are for one build."
[ -z "$sums" ] || [ -r "$sums" ] || fail "can't read $sums."
default_dir=
if [ -n "$into" ]; then
  dir=$into
elif [ -z "$dir" ]; then
  [ -n "${HOME:-}" ] || fail "HOME isn't set; give a folder with --dir."
  dir=$HOME/.local/bin
  default_dir=1
fi
case $dir in
  /*) ;;
  *) dir=$PWD/$dir ;;
esac

os=$(uname -s)
arch=$(uname -m)
case $os in
  Linux) os=linux ;;
  Darwin) os=darwin ;;
  MINGW* | MSYS* | CYGWIN*) os=windows ;;
  *) fail "there is no endeavor for $os $arch. It is built for Linux and macOS (x86_64, aarch64), and Windows (x86_64)." ;;
esac
case $arch in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *) fail "there is no endeavor for $(uname -s) $arch. It is built for x86_64 and aarch64." ;;
esac
platform=$os-$arch
exe=endeavor
suffix=
if [ "$os" = windows ]; then
  [ "$arch" = x86_64 ] || fail "there is no endeavor for Windows $arch. It is built for x86_64."
  exe=endeavor.exe
  suffix=.exe
fi

limited() {
  secs=$1
  shift
  if command -v timeout >/dev/null 2>&1; then
    timeout "$secs" "$@"
    return
  fi
  "$@" &
  job=$!
  (
    trap 'kill "$nap" 2>/dev/null; exit 0' TERM
    sleep "$secs" &
    nap=$!
    wait "$nap"
    kill "$job" 2>/dev/null
  ) >/dev/null 2>&1 &
  dog=$!
  status=0
  wait "$job" || status=$?
  kill "$dog" 2>/dev/null || true
  wait "$dog" 2>/dev/null || true
  return "$status"
}

fetch() {
  if command -v curl >/dev/null 2>&1; then
    if [ $# -ge 2 ]; then
      curl -fsSL --retry 2 --connect-timeout 15 --max-time 900 --proto-redir =https -o "$2" "$1"
    else
      curl -fsSL --retry 2 --connect-timeout 15 --max-time 60 --proto-redir =https "$1"
    fi
  elif command -v wget >/dev/null 2>&1; then
    if [ -z "${wget2+set}" ]; then
      case $(wget --version 2>/dev/null </dev/null | head -n 1) in "GNU Wget2"*) wget2=1 ;; *) wget2= ;; esac
    fi
    enforce=
    case $1 in https://*) [ -z "$wget2" ] || enforce=--https-enforce=hard ;; esac
    got=0
    if [ $# -ge 2 ]; then
      limited 900 wget -q -S $enforce --tries=3 --connect-timeout=15 --timeout=60 -O "$2" "$1" 2>"$tmp/wget.headers" || got=$?
    else
      limited 60 wget -q -S $enforce --tries=3 --connect-timeout=15 --timeout=60 -O - "$1" 2>"$tmp/wget.headers" || got=$?
    fi
    if away=$(redirect_not_https "$1" <"$tmp/wget.headers"); then
      [ $# -lt 2 ] || rm -f "$2"
      echo "install.sh: $1 was redirected to $away, which isn't https, so the download was thrown away." >&2
      return 1
    fi
    return "$got"
  else
    fail "this needs curl or wget, and neither is on the PATH."
  fi
}

redirect_not_https() {
  awk -v url="$1" '
    BEGIN { https = (tolower(substr(url, 1, 8)) == "https://"); found = 0 }
    {
      line = $0
      sub(/^[ \t]+/, "", line)
      if (tolower(substr(line, 1, 9)) != "location:") next
      to = substr(line, 10)
      sub(/^[ \t]+/, "", to)
      sub(/[ \t\r]+$/, "", to)
      if (to ~ /^[A-Za-z][A-Za-z0-9+.-]*:/) https = (tolower(substr(to, 1, 8)) == "https://")
      if (!https) { print to; found = 1; exit }
    }
    END { exit !found }'
}

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d' ' -f1
  else
    fail "checking the download needs sha256sum or shasum, and neither is on the PATH."
  fi
}

mkdir -p "$dir" || fail "couldn't create $dir."
tmp=$(mktemp -d "$dir/.endeavor-install.XXXXXX") || fail "couldn't write in $dir."
trap 'rm -rf "$tmp"' EXIT
trap 'rm -rf "$tmp"; exit 1' INT TERM HUP

set_newest() {
  printf '%s\n' "$key" >"$into/.newest.$$" && mv -f "$into/.newest.$$" "$into/.newest" || fail "couldn't write $into/.newest."
}

pinner="the plugin"
sums_from=$sums
if [ -z "$into" ] && [ -z "$sums" ] && [ -n "$pin_url" ]; then
  skip="Nothing was installed. To install the release's newest build checked only against the release's own checksum file, run it again with ENDEAVOR_PIN_URL=none (piped from curl: curl -fsSL https://raw.githubusercontent.com/jowch/EndeavorMCP/main/scripts/install.sh | ENDEAVOR_PIN_URL=none sh)."
  fetch "$pin_url" "$tmp/release-key" || fail "couldn't read the build main pins from $pin_url, so the download can't be checked against it. $skip"
  pinned_key=$(head -n 1 "$tmp/release-key" | tr -d ' \t\r\n')
  if ! is_key "$pinned_key" || ! grep -Eq '^[0-9a-fA-F]{64} [ *]' "$tmp/release-key"; then
    fail "$pin_url doesn't pin a build with checksums. $skip"
  fi
  if [ -z "$key" ] || [ "$key" = "$pinned_key" ]; then
    key=$pinned_key
    sums=$tmp/release-key
    pinner=main
    sums_from=$pin_url
  else
    echo "install.sh: main pins build $pinned_key, not $key, so build $key is checked only against the release's own checksum file." >&2
  fi
fi

newest=
if [ -z "$key" ]; then
  key=$(fetch "$release/LATEST") || fail "couldn't download $release/LATEST."
  key=$(printf '%s' "$key" | tr -d ' \t\r\n')
  is_key "$key" || fail "the release's LATEST file doesn't name a build ('$key')."
  newest=1
fi

if [ -n "$into" ]; then
  dir=$into/$key
  if [ -x "$dir/$exe" ]; then
    [ -z "$newest" ] || set_newest
    exit 0
  fi
  mkdir -p "$dir" || fail "couldn't create $dir."
fi

not_held() {
  echo "install.sh: the release doesn't hold build $key for $platform. A new build appears a few minutes after it is pushed; an old one may have been removed." >&2
  exit 3
}

is_missing() {
  if command -v curl >/dev/null 2>&1; then
    [ "$1" -eq 22 ] || [ "$1" -eq 37 ]
  else
    [ "$1" -eq 8 ]
  fi
}

name=endeavor-$key-$platform$suffix
sum_of() {
  awk -v n="$name" '{ sub(/\r$/, ""); f = $2; sub(/^\*/, "", f); if (f == n && $1 ~ /^[0-9a-fA-F]+$/ && length($1) == 64) { print tolower($1); exit } }' "$1"
}
pinned=
if [ -n "$sums" ] && grep -Eq '^[0-9a-fA-F]{64} [ *]' "$sums"; then
  pinned=1
  want=$(sum_of "$sums")
  [ -n "$want" ] || fail "the checksums in $sums_from list no binary of build $key for $platform."
else
  if ! fetch "$release/endeavor-$key.sha256" "$tmp/sums"; then
    if [ -z "$newest" ] && fetch "$release/LATEST" >/dev/null 2>&1; then
      not_held
    fi
    fail "couldn't download $release/endeavor-$key.sha256."
  fi
  want=$(sum_of "$tmp/sums")
  [ -n "$want" ] || fail "the newest build ($key) has no binary for $platform."
fi

status=0
fetch "$release/$name" "$tmp/$exe" || status=$?
if [ "$status" -ne 0 ]; then
  [ -z "$newest" ] && is_missing "$status" && not_held
  fail "couldn't download $release/$name."
fi
have=$(sha256 "$tmp/$exe")
if [ "$have" != "$want" ]; then
  if [ -n "$pinned" ]; then
    fail "the download from $release/$name doesn't match the checksum $pinner pins (SHA-256 $have, expected $want). It was deleted; nothing was installed."
  fi
  fail "the download from $release/$name doesn't match its checksum (SHA-256 $have, expected $want). It was deleted; nothing was installed."
fi

chmod 755 "$tmp/$exe"
mv -f "$tmp/$exe" "$dir/$exe" || fail "couldn't replace $dir/$exe."

if [ -n "$into" ]; then
  [ -z "$newest" ] || set_newest
  [ -n "$quiet" ] || echo "Installed endeavor (build $key, $platform) in $dir/$exe"
  exit 0
fi

echo "Installed endeavor (build $key, $platform) in $dir/$exe"
version=$("$dir/$exe" --version 2>/dev/null | head -n 1) || version=
if [ -n "$version" ]; then
  echo "$version"
else
  echo "Warning: $dir/$exe didn't run with --version. Check that it suits this computer." >&2
fi

case ":${PATH:-}:" in
  *":$dir:"*) ;;
  *)
    if [ -n "$default_dir" ]; then line='export PATH="$HOME/.local/bin:$PATH"'; else line="export PATH=\"$dir:\$PATH\""; fi
    echo
    echo "$dir isn't on your PATH. Add this line to your shell's startup file (~/.profile, ~/.zshrc or ~/.bashrc), then open a new terminal:"
    echo "    $line"
    ;;
esac

echo
echo "This copy is for running endeavor yourself, such as endeavor serve or endeavor status. The plugins keep their own copy and don't use this one."
echo "To install the plugin for your agent (Claude Code or Codex), see the README: https://github.com/jowch/EndeavorMCP#install"
