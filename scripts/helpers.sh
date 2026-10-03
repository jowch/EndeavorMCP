#!/bin/sh
# Put the runtime helpers for Linux servers (endeavor-remote, x86_64 and
# aarch64) in target/helpers/<platform>/, or in $HELPERS_OUT, built from this
# checkout's source. Endeavor's scripts/helpers.sh runs this in a checkout of
# the commit Endeavor's Cargo.lock pins, with HELPERS_OUT set to its own
# target/helpers.
#
#   scripts/helpers.sh               keep what's there if it matches, else
#                                    download it, else build it
#   scripts/helpers.sh --fetch-only  keep or download, never build (Endeavor's
#                                    git hooks run this)
#   scripts/helpers.sh --key         print the helper source key
#
# Downloads come from the "helpers" release on GitHub, which the Helpers
# workflow (.github/workflows/helpers.yml) fills for every change to the
# helper's source; plain curl, no sign-in. A checkout with uncommitted helper
# changes, or one GitHub hasn't built yet, is built here instead: with
# cargo-zigbuild if it's installed, else on the Linux machine named by
# ENDEAVOR_HELPER_HOST (scripts/build-helpers.sh --via, its architecture only).
set -eu
cd "$(dirname "$0")/.."
out=${HELPERS_OUT:-target/helpers}
paths="crates/endeavor-mcp crates/wire plugin runtime rust-toolchain.toml"
platforms="linux-x86_64 linux-aarch64"

# The crates the helper is built from, as "name version" lines, read from a
# Cargo.lock on stdin: only the helper's own dependencies.
closure() {
  awk -v start=endeavor-remote '
    /^\[\[package\]\]/ { flush(); name = ""; ver = ""; deps = ""; indeps = 0; next }
    /^name = / { gsub(/"/, "", $3); name = $3; next }
    /^version = / { gsub(/"/, "", $3); ver = $3; next }
    /^dependencies = \[/ { indeps = 1; next }
    indeps && /^\]/ { indeps = 0; next }
    indeps { line = $0; gsub(/[",]/, "", line); sub(/^ +/, "", line); deps = deps line "|"; next }
    function flush() { if (name != "") { pkg[name " " ver] = deps; one[name] = name " " ver } }
    END {
      flush()
      queue[1] = one[start]; seen[one[start]] = 1; head = 1; tail = 1
      while (head <= tail) {
        p = queue[head++]; print p
        k = split(pkg[p], d, "|")
        for (i = 1; i <= k; i++) {
          if (d[i] == "") continue
          m = split(d[i], f, " ")
          q = (m == 1) ? one[f[1]] : f[1] " " f[2]
          if (!(q in seen)) { seen[q] = 1; queue[++tail] = q }
        }
      }
    }' | LC_ALL=C sort
}

# The committed helper source, its dependencies and the version it reports, hashed.
key() {
  {
    for p in $paths; do git rev-parse "HEAD:$p"; done
    git show HEAD:Cargo.lock | closure
    git show HEAD:Cargo.toml | sed -n 's/^version = "\(.*\)"/\1/p' | head -1
  } | shasum -a 256 | cut -c1-12
}

fetch_only=
case "${1:-}" in
  --key) key; exit 0 ;;
  --fetch-only) fetch_only=1 ;;
esac

key=$(key)
if [ -n "$(git status --porcelain -- $paths)" ] || [ "$(closure < Cargo.lock)" != "$(git show HEAD:Cargo.lock | closure)" ]; then
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

if [ -n "$fetch_only" ]; then
  echo "Server helpers for this checkout aren't on GitHub yet (key $key); run scripts/helpers.sh to build them, or pull again later." >&2
  exit 1
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
