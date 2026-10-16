#!/bin/sh
# The Helpers workflow's Publish step (.github/workflows/helpers.yml), kept
# here so a test can run it with a fake `gh` (tests/publish_helpers.rs).
#
# Run from the checkout's top folder, with the builds in dist/<platform>/
# (endeavor, or endeavor.exe on Windows), KEY set to the release key, and gh
# signed in with contents: write. It uploads endeavor-<key>-<platform> and
# endeavor-<key>.sha256 to the "helpers" release, and on main (GITHUB_REF)
# LATEST naming the key. The Linux builds must be there; macOS and Windows
# are published when they are.
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
mkdir -p publish
for dir in dist/*/; do
  platform=$(basename "$dir")
  ext=
  case $platform in windows-*) ext=.exe ;; esac
  cp "$dir/endeavor$ext" "publish/endeavor-$key-$platform$ext"
done
(cd publish && sha256sum endeavor-"$key"-* > "endeavor-$key.sha256")
gh release view helpers >/dev/null 2>&1 || gh release create helpers --prerelease --target "$GITHUB_SHA" \
  --title "Server helpers" --notes "$notes"
# An earlier run of this key may have published a build this run
# didn't make (its macOS or Windows build failed). That build stays
# in the release, so its line stays in the checksum file.
if gh release view helpers --json assets --jq '.assets[].name' | grep -qx "endeavor-$key.sha256"; then
  mkdir -p earlier
  gh release download helpers --pattern "endeavor-$key.sha256" --dir earlier --clobber
  while read -r sum file; do
    file=${file#\*}
    if [ -n "$file" ] && [ ! -f "publish/$file" ]; then
      printf '%s  %s\n' "$sum" "$file" >> "publish/endeavor-$key.sha256"
    fi
  done < "earlier/endeavor-$key.sha256"
fi
cat "publish/endeavor-$key.sha256"
gh release edit helpers --notes "$notes"
gh release upload helpers publish/* --clobber
# A manual run on another branch builds that branch's helpers but doesn't make them the newest.
if [ "$GITHUB_REF" = refs/heads/main ]; then
  echo "$key" > LATEST
  gh release upload helpers LATEST --clobber
fi
