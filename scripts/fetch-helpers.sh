#!/bin/sh
# Download the runtime helpers for servers that the Helpers workflow
# (.github/workflows/helpers.yml) built from the same helper source as this
# checkout, into target/helpers/<platform>/, where the app and
# scripts/bundle.sh look. Needs the GitHub CLI (gh), signed in.
#
# The workflow runs only when the helper's source changes, so this takes the
# newest successful run whose commit has the same helper source as HEAD.
set -eu
cd "$(dirname "$0")/.."
out=target/helpers
workflow=helpers.yml
paths="crates/endeavor-remote crates/wire Cargo.lock rust-toolchain.toml"

source_of() {
  for p in $paths; do git rev-parse "$1:$p" 2>/dev/null || echo missing; done
  git show "$1:Cargo.toml" 2>/dev/null | sed -n 's/^version = "\(.*\)"/\1/p' | head -1
}
here=$(source_of HEAD)

run=
for line in $(gh run list --workflow "$workflow" --status success --limit 50 --json databaseId,headSha -q '.[] | "\(.databaseId):\(.headSha)"'); do
  id=${line%%:*}
  sha=${line#*:}
  git cat-file -e "$sha^{commit}" 2>/dev/null || git fetch -q origin "$sha" 2>/dev/null || continue
  if [ "$(source_of "$sha")" = "$here" ]; then
    run=$id
    break
  fi
done

if [ -z "$run" ]; then
  echo "No Helpers run matches this checkout's helper source." >&2
  echo "Push it and wait for the workflow, start one with 'gh workflow run $workflow --ref <branch>'," >&2
  echo "or build locally with scripts/build-helpers.sh." >&2
  exit 1
fi

rm -rf "$out.part"
gh run download "$run" --dir "$out.part"
mkdir -p "$out"
for dir in "$out.part"/*/; do
  platform=$(basename "$dir")
  rm -rf "$out/$platform"
  mv "$dir" "$out/$platform"
  chmod 755 "$out/$platform/endeavor-remote"
  echo "$out/$platform/endeavor-remote"
done
rm -rf "$out.part"
