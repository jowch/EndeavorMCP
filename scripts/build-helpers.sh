#!/bin/sh
# Build the runtime helper (endeavor-remote) for Linux servers into
# target/helpers/<os>-<arch>/, where the app looks in a source checkout and
# which scripts/bundle.sh copies into Endeavor.app. A server whose platform has
# no helper there can't be connected to.
#
#   scripts/build-helpers.sh             cross-build x86_64 and aarch64 (musl) with cargo-zigbuild
#   scripts/build-helpers.sh --via HOST  build on HOST, a Linux machine reachable by ssh, for its
#                                        own architecture, and copy the binary back
#
# Neither way installs anything on this computer. --via installs rustup (and a
# C compiler, with sudo) on HOST if they're missing.
set -eu
cd "$(dirname "$0")/.."
out=target/helpers

if [ "${1:-}" = "--via" ]; then
  host=${2:?usage: scripts/build-helpers.sh --via HOST}
  [ "$(ssh "$host" uname -s)" = Linux ] || { echo "$host isn't running Linux" >&2; exit 1; }
  arch=$(ssh "$host" uname -m)
  target=$arch-unknown-linux-musl
  version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
  toolchain=$(sed -n 's/^channel = "\(.*\)"/\1/p' rust-toolchain.toml)
  # Just the helper and its protocol crate, as a workspace of their own.
  stage=$(mktemp -d)
  trap 'rm -rf "$stage"' EXIT
  mkdir -p "$stage/src/crates"
  cp -R crates/wire crates/endeavor-remote "$stage/src/crates/"
  rm -rf "$stage/src/crates/"*/target
  cp Cargo.lock "$stage/src/"
  printf '[workspace]\nmembers = ["crates/wire", "crates/endeavor-remote"]\nresolver = "3"\n\n[workspace.package]\nversion = "%s"\n' "$version" > "$stage/src/Cargo.toml"
  printf '[toolchain]\nchannel = "%s"\ntargets = ["%s"]\n' "$toolchain" "$target" > "$stage/src/rust-toolchain.toml"
  COPYFILE_DISABLE=1 tar --no-xattrs -C "$stage/src" -cf - . | ssh "$host" '
    set -e
    # Neither may read stdin: it carries the source.
    command -v cc >/dev/null || sudo -n env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq gcc </dev/null >/dev/null
    [ -x "$HOME/.cargo/bin/rustup" ] || curl -sSf https://sh.rustup.rs | sh -s -- -y -q --default-toolchain none >/dev/null
    d="$HOME/endeavor-helper-build"; rm -rf "$d/crates"; mkdir -p "$d"; tar -C "$d" -xf -'
  ssh "$host" "cd ~/endeavor-helper-build && ~/.cargo/bin/cargo build -q --release -p endeavor-remote --target $target" >&2
  mkdir -p "$out/linux-$arch"
  ssh "$host" "cat ~/endeavor-helper-build/target/$target/release/endeavor-remote" > "$out/linux-$arch/endeavor-remote"
  chmod 755 "$out/linux-$arch/endeavor-remote"
  echo "$out/linux-$arch/endeavor-remote"
  exit 0
fi

if ! command -v cargo-zigbuild >/dev/null; then
  echo "cargo-zigbuild isn't installed. Install it (and zig), or build on a Linux machine with --via HOST." >&2
  exit 1
fi
for arch in x86_64 aarch64; do
  target=$arch-unknown-linux-musl
  cargo zigbuild --release -p endeavor-remote --target "$target"
  mkdir -p "$out/linux-$arch"
  cp "target/$target/release/endeavor-remote" "$out/linux-$arch/"
  echo "$out/linux-$arch/endeavor-remote"
done
