#!/bin/sh
# The Helpers workflow's Publish step (.github/workflows/helpers.yml), kept
# here so a test can run it with a fake `gh` (tests/publish_helpers.rs).
#
# Run from the checkout's top folder, with the builds in dist/<platform>/
# (endeavor, or endeavor.exe on Windows), KEY set to the release key, and gh
# signed in with contents: write. It uploads endeavor-<key>-<platform> and
# endeavor-<key>.sha256 to the "helpers" release, and on main (GITHUB_REF)
# LATEST naming the key. The Linux builds must be there; macOS and Windows
# are published when they are. A binary already published under the key is
# kept, not replaced. With GITHUB_OUTPUT set, the checksum file is also the
# step's output `sums`, for the pin job.
set -e
key=${KEY:-}
if [ -z "$key" ]; then
  echo "There is no release key." >&2
  exit 1
fi
for platform in linux-x86_64 linux-aarch64; do
  if [ ! -f "dist/$platform/endeavor" ]; then
    echo "The $platform build is missing, so nothing is published." >&2
    exit 1
  fi
done
notes="endeavor for Linux, macOS and Windows, published by the Helpers workflow for each change to the helper's source. scripts/install.sh and scripts/install.ps1 install the newest build; scripts/helpers.sh downloads the Linux ones matching your checkout."
gh release view helpers >/dev/null 2>&1 || gh release create helpers --prerelease --target "$GITHUB_SHA" \
  --title "Server helpers" --notes "$notes"
assets=$(gh release view helpers --json assets --jq '.assets[].name')
# An earlier run of this key may have published builds already. Those stay as
# they are, binary and checksum line: the plugins pin each build's checksums
# (release-key), so a binary under a key must never change, and builds aren't
# byte-for-byte reproducible. A build this run made for a platform the
# release lacks (an earlier run's macOS or Windows build failed) is added.
mkdir -p publish earlier
: >earlier/sums
if printf '%s\n' "$assets" | grep -qx "endeavor-$key.sha256"; then
  gh release download helpers --pattern "endeavor-$key.sha256" --dir earlier --clobber
  cp "earlier/endeavor-$key.sha256" earlier/sums
fi
# listed NAME: whether the earlier checksum file has a line for NAME.
listed() {
  awk -v n="$1" '{ f = $2; sub(/^\*/, "", f); if (f == n) { found = 1; exit } } END { exit !found }' earlier/sums
}
for dir in dist/*/; do
  platform=$(basename "$dir")
  ext=
  case $platform in windows-*) ext=.exe ;; esac
  name=endeavor-$key-$platform$ext
  if printf '%s\n' "$assets" | grep -qx "$name" && listed "$name"; then
    echo "$name is already published; it stays as it is."
  else
    cp "$dir/endeavor$ext" "publish/$name"
  fi
done
(cd publish && for f in endeavor-"$key"-*; do [ ! -f "$f" ] || sha256sum "$f"; done) >"publish/endeavor-$key.sha256"
while read -r sum file; do
  file=${file#\*}
  if [ -n "$file" ] && [ ! -f "publish/$file" ]; then
    printf '%s  %s\n' "$sum" "$file" >> "publish/endeavor-$key.sha256"
  fi
done < earlier/sums
cat "publish/endeavor-$key.sha256"
gh release edit helpers --notes "$notes"
gh release upload helpers publish/* --clobber
# The pin job writes these into release-key.
if [ -n "${GITHUB_OUTPUT:-}" ]; then
  { echo "sums<<ENDEAVOR_SUMS"; cat "publish/endeavor-$key.sha256"; echo ENDEAVOR_SUMS; } >>"$GITHUB_OUTPUT"
fi
# A manual run on another branch builds that branch's helpers but doesn't make them the newest.
if [ "$GITHUB_REF" = refs/heads/main ]; then
  echo "$key" > LATEST
  gh release upload helpers LATEST --clobber
fi
