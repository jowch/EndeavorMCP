#!/bin/sh
# Install the newest endeavor from the Helpers release on GitHub, for Linux and
# macOS (Windows: scripts/install.ps1).
#
#   curl -fsSL https://raw.githubusercontent.com/jowch/EndeavorMCP/main/scripts/install.sh | sh
#   curl -fsSL .../install.sh | sh -s -- --dir /some/folder
#
# It reads LATEST from the release (or takes the build from --key), downloads
# endeavor-<key>-<platform> and endeavor-<key>.sha256, checks the SHA-256, and
# puts the binary in DIR as `endeavor` (`endeavor.exe` under Git Bash on
# Windows), replacing one that is there. DIR is --dir, else
# $ENDEAVOR_INSTALL_DIR, else ~/.local/bin. No sudo, and no shell file is
# edited: if DIR isn't on your PATH, the line to add is printed.
#
# The plugin's launcher (endeavor-mcp.sh) calls it with --quiet --into BASE:
# the binary goes to BASE/<key>/endeavor, only if that isn't there already,
# and with no --key BASE/.newest is set to the key LATEST named. --quiet
# writes everything to stderr, since stdout belongs to the MCP server.
#
# ENDEAVOR_RELEASE_URL replaces the release's address (for tests; file:// works).
# The checksum file comes from the same release as the binary, so the check
# catches a damaged download and not a tampered release.
set -eu

release=${ENDEAVOR_RELEASE_URL:-https://github.com/jowch/EndeavorMCP/releases/download/helpers}
release=${release%/}
dir=${ENDEAVOR_INSTALL_DIR:-}
into=
key=
quiet=

fail() {
  echo "install.sh: $*" >&2
  exit 1
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
case $key in
  *[!0-9a-fA-F]*) fail "--key '$key' isn't a build's key." ;;
esac
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

# fetch URL [FILE]: the body to FILE, or to stdout. A connection that doesn't
# open in 15 s is given up on, and so is a file that takes over 15 minutes
# (a build is about 30 MB). curl follows a redirect only to https; wget has no
# such limit.
fetch() {
  if command -v curl >/dev/null 2>&1; then
    if [ $# -ge 2 ]; then
      curl -fsSL --retry 2 --connect-timeout 15 --max-time 900 --proto-redir =https -o "$2" "$1"
    else
      curl -fsSL --retry 2 --connect-timeout 15 --max-time 60 --proto-redir =https "$1"
    fi
  elif command -v wget >/dev/null 2>&1; then
    if [ $# -ge 2 ]; then wget -q --tries=3 --connect-timeout=15 --timeout=60 -O "$2" "$1"; else wget -q --tries=3 --connect-timeout=15 --timeout=60 -O - "$1"; fi
  else
    fail "this needs curl or wget, and neither is on the PATH."
  fi
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

newest=
if [ -z "$key" ]; then
  key=$(fetch "$release/LATEST") || fail "couldn't download $release/LATEST."
  key=$(printf '%s' "$key" | tr -d ' \t\r\n')
  case $key in
    '' | *[!0-9a-fA-F]*) fail "the release's LATEST file doesn't name a build ('$key')." ;;
  esac
  newest=1
fi

if [ -n "$into" ]; then
  dir=$into/$key
  if [ -x "$dir/$exe" ]; then
    [ -z "$newest" ] || printf '%s\n' "$key" >"$into/.newest"
    exit 0
  fi
  mkdir -p "$dir" || fail "couldn't create $dir."
fi

name=endeavor-$key-$platform$suffix
fetch "$release/endeavor-$key.sha256" "$tmp/sums" || fail "couldn't download $release/endeavor-$key.sha256."
want=$(awk -v n="endeavor-$key-$platform$suffix" '{ f = $2; sub(/^\*/, "", f); if (f == n) { print tolower($1); exit } }' "$tmp/sums")
[ -n "$want" ] || fail "the newest build ($key) has no binary for $platform."

fetch "$release/$name" "$tmp/$exe" || fail "couldn't download $release/$name."
have=$(sha256 "$tmp/$exe")
if [ "$have" != "$want" ]; then
  fail "the download from $release/$name doesn't match its checksum (SHA-256 $have, expected $want). It was deleted; nothing was installed."
fi

chmod 755 "$tmp/$exe"
mv -f "$tmp/$exe" "$dir/$exe" || fail "couldn't replace $dir/$exe."

if [ -n "$into" ]; then
  [ -z "$newest" ] || printf '%s\n' "$key" >"$into/.newest"
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
echo "Next, install the plugin for your agent (Claude Code, Codex or Gemini CLI). The steps are in the README: https://github.com/jowch/EndeavorMCP#readme"
