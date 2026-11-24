#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
out=${HELPERS_OUT:-target/helpers}
paths="crates/endeavor-mcp crates/wire plugin runtime rust-toolchain.toml"
platforms="linux-x86_64 linux-aarch64"

closure() {
  awk -v start=endeavor-mcp '
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

key() {
  {
    for p in $paths; do git rev-parse "HEAD:$p"; done
    git show HEAD:Cargo.lock | closure
    git show HEAD:Cargo.toml | sed -n 's/^version = "\(.*\)"/\1/p' | head -1
  } | shasum -a 256 | cut -c1-12
}

dirty() {
  [ -n "$(git status --porcelain -- $paths Cargo.toml)" ] || [ "$(closure < Cargo.lock)" != "$(git show HEAD:Cargo.lock | closure)" ]
}

fetch_only=
case "${1:-}" in
  --key) key; exit 0 ;;
  --fetch-only) fetch_only=1 ;;
esac

key=$(key)
if dirty; then
  key="$key-dirty"
fi

missing=
for platform in $platforms; do
  stamp="$out/$platform/SOURCE"
  if [ -x "$out/$platform/endeavor" ] && [ -f "$stamp" ] && [ "$(cat "$stamp")" = "$key" ] && [ "${key%-dirty}" = "$key" ]; then
    echo "$out/$platform/endeavor (up to date)"
  else
    missing="$missing $platform"
  fi
done
[ -z "$missing" ] && exit 0

repo=$(git remote get-url origin | sed -E 's#^(https://github\.com/|git@github\.com:)##; s#\.git$##')
base="https://github.com/$repo/releases/download/helpers"

if [ "${key%-dirty}" = "$key" ] && sums=$(curl -fsSL "$base/endeavor-$key.sha256" 2>/dev/null); then
  for platform in $missing; do
    name="endeavor-$key-$platform"
    mkdir -p "$out/$platform"
    part="$out/$platform/endeavor.part"
    curl -fsSL -o "$part" "$base/$name"
    want=$(printf '%s\n' "$sums" | awk -v n="$name" '$2 == n { print $1 }')
    have=$(shasum -a 256 "$part" | cut -d' ' -f1)
    if [ -z "$want" ] || [ "$want" != "$have" ]; then
      rm -f "$part"
      echo "The downloaded $name doesn't match its checksum." >&2
      exit 1
    fi
    chmod 755 "$part"
    mv "$part" "$out/$platform/endeavor"
    echo "$key" > "$out/$platform/SOURCE"
    echo "$out/$platform/endeavor (downloaded)"
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
