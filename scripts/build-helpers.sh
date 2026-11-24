#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
out=${HELPERS_OUT:-target/helpers}

if [ "${1:-}" = "--via" ]; then
  host=${2:?usage: scripts/build-helpers.sh --via HOST}
  [ "$(ssh "$host" uname -s)" = Linux ] || { echo "$host isn't running Linux" >&2; exit 1; }
  arch=$(ssh "$host" uname -m)
  target=$arch-unknown-linux-musl
  toolchain=$(sed -n 's/^channel = "\(.*\)"/\1/p' rust-toolchain.toml)
  stage=$(mktemp -d)
  trap 'rm -rf "$stage"' EXIT
  mkdir -p "$stage/src/crates"
  cp -R crates/wire crates/endeavor-mcp "$stage/src/crates/"
  cp -R plugin runtime "$stage/src/"
  rm -rf "$stage/src/crates/"*/target
  cp Cargo.toml Cargo.lock "$stage/src/"
  printf '[toolchain]\nchannel = "%s"\ntargets = ["%s"]\n' "$toolchain" "$target" > "$stage/src/rust-toolchain.toml"
  COPYFILE_DISABLE=1 tar --no-xattrs -C "$stage/src" -cf - . | ssh "$host" '
    set -e
    command -v cc >/dev/null || sudo -n env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq gcc </dev/null >/dev/null
    [ -x "$HOME/.cargo/bin/rustup" ] || curl -sSf https://sh.rustup.rs | sh -s -- -y -q --default-toolchain none >/dev/null
    d="$HOME/endeavor-helper-build"; rm -rf "$d/crates" "$d/plugin" "$d/runtime"; mkdir -p "$d"; tar -C "$d" -xf -'
  ssh "$host" "cd ~/endeavor-helper-build && ~/.cargo/bin/cargo build -q --release -p endeavor-mcp --target $target" >&2
  mkdir -p "$out/linux-$arch"
  ssh "$host" "cat ~/endeavor-helper-build/target/$target/release/endeavor" > "$out/linux-$arch/endeavor"
  chmod 755 "$out/linux-$arch/endeavor"
  echo "$out/linux-$arch/endeavor"
  exit 0
fi

if ! command -v cargo-zigbuild >/dev/null; then
  echo "cargo-zigbuild isn't installed. Install it (and zig), or build on a Linux machine with --via HOST." >&2
  exit 1
fi
for arch in x86_64 aarch64; do
  target=$arch-unknown-linux-musl
  cargo zigbuild --release -p endeavor-mcp --target "$target"
  mkdir -p "$out/linux-$arch"
  cp "target/$target/release/endeavor" "$out/linux-$arch/"
  echo "$out/linux-$arch/endeavor"
done
