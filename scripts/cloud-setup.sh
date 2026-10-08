#!/bin/bash
# Provisions a Claude Code cloud VM (Ubuntu 24.04, x86_64, root) for work on
# Endeavor and EndeavorMCP: the Linux libraries the app builds against, Xvfb
# to run it without a display, Julia for the notebook runtime, and marimo for
# the planned Python backend. How it is used, and the network hosts it
# needs: https://github.com/jowch/Endeavor/blob/main/docs/cloud.md
#
# Safe to run more than once: each step checks first. It never fails the
# session: a step that can't finish says why on stderr and the rest go on.
# Keep it under about five minutes, or the environment's cache isn't kept.
#
# --no-gui skips the app's Linux libraries and Xvfb (EndeavorMCP needs only
# Julia and marimo). EndeavorMCP keeps a copy of this file; change both.
set -uo pipefail
GUI=1
[ "${1:-}" = --no-gui ] && GUI=0

# Bump with JULIA_VERSION and TARBALLS in EndeavorMCP's
# crates/endeavor-mcp/src/julia.rs (and the app's src/runtime.rs).
JULIA_VERSION=1.12.6
JULIA_URL=https://julialang-s3.julialang.org/bin/linux/x64/1.12/julia-1.12.6-linux-x86_64.tar.gz
JULIA_SHA256=bbabf3bef19421a9dbd24a767d807606ab85e444323b5a1c73ffe293fa3d079a
# The version docs/marimo.md is designed against. runtime-py's uv.lock will
# pin it once that exists; until then this is the one agents try things with.
MARIMO_VERSION=0.25.1
# Bump with rust-toolchain.toml (both repositories pin the same one).
RUST_TOOLCHAIN=1.98.1

say() { echo "cloud-setup: $*" >&2; }

if [ "$(uname -s)-$(uname -m)" != Linux-x86_64 ]; then
  say "this script is for the cloud's Linux x86_64 VMs; nothing done"
  exit 0
fi
if [ "$(id -u)" = 0 ]; then SUDO=; else SUDO="sudo -n"; fi

# The set docs/linux.md lists, plus Xvfb, Openbox and the screenshot tools.
APT_PACKAGES=(
  build-essential clang cmake pkg-config libssl-dev
  libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libvulkan-dev
  libx11-xcb-dev libxcb1-dev libfontconfig-dev libfreetype-dev libasound2-dev
  libzstd-dev libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev
  libjavascriptcoregtk-4.1-dev libxdo-dev mesa-vulkan-drivers
  xvfb openbox imagemagick xdotool jq
)

apt_packages() {
  local missing=()
  for p in "${APT_PACKAGES[@]}"; do
    dpkg-query -W -f='${Status}' "$p" 2>/dev/null | grep -q "ok installed" || missing+=("$p")
  done
  [ ${#missing[@]} = 0 ] && return 0
  say "installing ${#missing[@]} apt packages"
  $SUDO apt-get update -q >/dev/null &&
    DEBIAN_FRONTEND=noninteractive $SUDO apt-get install -y -q --no-install-recommends "${missing[@]}" >/dev/null ||
    say "apt-get failed; the app won't build on Linux until it succeeds"
}

julia_toolchain() {
  if command -v julia >/dev/null && julia --version 2>/dev/null | grep -q "$JULIA_VERSION"; then
    return 0
  fi
  local dir=/opt/julia-$JULIA_VERSION tar
  tar=$(mktemp)
  say "downloading Julia $JULIA_VERSION"
  if ! curl -fsSL --retry 3 -o "$tar" "$JULIA_URL"; then
    say "couldn't download Julia from julialang-s3.julialang.org. If the proxy answered 403, the environment's network access doesn't allow it (see docs/cloud.md in Endeavor)."
    rm -f "$tar"
    return 0
  fi
  if ! echo "$JULIA_SHA256  $tar" | sha256sum -c --quiet; then
    say "the Julia download's SHA-256 doesn't match; not installed"
    rm -f "$tar"
    return 0
  fi
  $SUDO mkdir -p "$dir" && $SUDO tar -xzf "$tar" -C "$dir" --strip-components=1 &&
    $SUDO ln -sf "$dir/bin/julia" /usr/local/bin/julia
  rm -f "$tar"
  julia --version >&2 || say "Julia didn't install"
}

marimo_toolchain() {
  command -v uv >/dev/null || { say "uv isn't installed; skipping marimo"; return 0; }
  if command -v marimo >/dev/null && marimo --version 2>/dev/null | grep -q "$MARIMO_VERSION"; then
    return 0
  fi
  say "installing marimo $MARIMO_VERSION"
  # Into /usr/local/bin, so it is on every shell's PATH.
  UV_TOOL_BIN_DIR=/usr/local/bin uv tool install --quiet --force "marimo==$MARIMO_VERSION" ||
    say "couldn't install marimo"
}

# The pinned toolchain (rust-toolchain.toml) comes without clippy and rustfmt.
rust_components() {
  command -v rustup >/dev/null || return 0
  rustup toolchain install --profile minimal -c clippy -c rustfmt "$RUST_TOOLCHAIN" >/dev/null 2>&1 ||
    say "couldn't add clippy and rustfmt"
}

# Independent, so in parallel: the cache is only kept under about five minutes.
[ $GUI = 1 ] && apt_packages &
julia_toolchain &
marimo_toolchain &
rust_components &
wait
exit 0
