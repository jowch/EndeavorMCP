#!/bin/sh
# Put the runtime helpers for Linux servers (endeavor-remote, x86_64 and
# aarch64) in target/helpers/<platform>/, where the app and scripts/bundle.sh
# look, built from this checkout's helper source.
#
#   scripts/helpers.sh        keep what's there if it matches, else download
#                             it, else build it
#   scripts/helpers.sh --key  print the helper source key
#
# Downloads come from the "helpers" release on GitHub, which the Helpers
# workflow (.github/workflows/helpers.yml) fills for every change to the
# helper's source; plain curl, no sign-in. A checkout with uncommitted helper
# changes, or one GitHub hasn't built yet, is built here instead: with
# cargo-zigbuild if it's installed, else on the Linux machine named by
# ENDEAVOR_HELPER_HOST (scripts/build-helpers.sh --via, its architecture only).
set -eu
cd "$(dirname "$0")/.."
out=target/helpers
paths="crates/endeavor-remote crates/wire Cargo.lock rust-toolchain.toml"
platforms="linux-x86_64 linux-aarch64"

# The committed helper source plus the version it reports, hashed.
key() {
  {
    for p in $paths; do git rev-parse "HEAD:$p"; done
    sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1
  } | shasum -a 256 | cut -c1-12
}

if [ "${1:-}" = "--key" ]; then
  key
  exit 0
fi

key=$(key)
if [ -n "$(git status --porcelain -- $paths)" ]; then
  key="$key-dirty"
fi

missing=
for platform in $platforms; do
  stamp="$out/$platform/SOURCE"
  if [ -x "$out/$platform/endeavor-remote" ] && [ -f "$stamp" ] && [ "$(cat "$stamp")" = "$key" ] && [ "${key%-dirty}" = "$key" ]; then
    echo "$out/$platform/endeavor-remote (up to date)"
  else
    missing="$missing $platform"
  fi
done
[ -z "$missing" ] && exit 0

# The repository's GitHub owner/name, from origin's URL.
repo=$(git remote get-url origin | sed -E 's#^(https://github\.com/|git@github\.com:)##; s#\.git$##')
base="https://github.com/$repo/releases/download/helpers"

if [ "${key%-dirty}" = "$key" ] && sums=$(curl -fsSL "$base/endeavor-remote-$key.sha256" 2>/dev/null); then
  for platform in $missing; do
    name="endeavor-remote-$key-$platform"
    mkdir -p "$out/$platform"
    part="$out/$platform/endeavor-remote.part"
    curl -fsSL -o "$part" "$base/$name"
    want=$(printf '%s\n' "$sums" | awk -v n="$name" '$2 == n { print $1 }')
    have=$(shasum -a 256 "$part" | cut -d' ' -f1)
    if [ -z "$want" ] || [ "$want" != "$have" ]; then
      rm -f "$part"
      echo "The downloaded $name doesn't match its checksum." >&2
      exit 1
    fi
    chmod 755 "$part"
    mv "$part" "$out/$platform/endeavor-remote"
    echo "$key" > "$out/$platform/SOURCE"
    echo "$out/$platform/endeavor-remote (downloaded)"
  done
  exit 0
fi

if [ "${key%-dirty}" != "$key" ]; then
  echo "The helper source has uncommitted changes; building it here." >&2
else
  echo "GitHub has no helpers for this source yet (key $key); building them here." >&2
fi
if command -v cargo-zigbuild >/dev/null; then
  scripts/build-helpers.sh >&2
  for platform in $platforms; do echo "$key" > "$out/$platform/SOURCE"; done
elif [ -n "${ENDEAVOR_HELPER_HOST:-}" ]; then
  built=$(scripts/build-helpers.sh --via "$ENDEAVOR_HELPER_HOST")
  echo "$key" > "$(dirname "$built")/SOURCE"
  echo "$built (built on $ENDEAVOR_HELPER_HOST)"
else
  echo "Can't build them here: install cargo-zigbuild and zig, set ENDEAVOR_HELPER_HOST to a Linux machine you can ssh to, or push and let the Helpers workflow build them." >&2
  exit 1
fi
