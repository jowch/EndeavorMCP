#!/bin/bash
set -uo pipefail
GUI=1
[ "${1:-}" = --no-gui ] && GUI=0

JULIA_VERSION=1.12.6
JULIA_URL=https://julialang-s3.julialang.org/bin/linux/x64/1.12/julia-1.12.6-linux-x86_64.tar.gz
JULIA_SHA256=bbabf3bef19421a9dbd24a767d807606ab85e444323b5a1c73ffe293fa3d079a
MARIMO_VERSION=0.25.1
RUST_TOOLCHAIN=1.98.1

say() { echo "cloud-setup: $*" >&2; }

if [ "$(uname -s)-$(uname -m)" != Linux-x86_64 ]; then
  say "this script is for the cloud's Linux x86_64 VMs; nothing done"
  exit 0
fi
if [ "$(id -u)" = 0 ]; then SUDO=; else SUDO="sudo -n"; fi

APT_PACKAGES=(
  build-essential clang cmake pkg-config libssl-dev
  libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libvulkan-dev
  libx11-xcb-dev libxcb1-dev libfontconfig-dev libfreetype-dev libasound2-dev
  libzstd-dev libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev
  libjavascriptcoregtk-4.1-dev libxdo-dev mesa-vulkan-drivers
  xvfb openbox imagemagick xdotool jq
)
R_PACKAGES=(
  r-base-core build-essential r-cran-bh r-cran-commonmark r-cran-curl r-cran-httpuv
  r-cran-jsonlite r-cran-later r-cran-processx r-cran-promises r-cran-renv
)
[ $GUI = 1 ] && APT_PACKAGES+=("${R_PACKAGES[@]}") || APT_PACKAGES=("${R_PACKAGES[@]}")

apt_packages() {
  local missing=()
  for p in "${APT_PACKAGES[@]}"; do
    dpkg-query -W -f='${Status}' "$p" 2>/dev/null | grep -q "ok installed" || missing+=("$p")
  done
  [ ${#missing[@]} = 0 ] && return 0
  say "installing ${#missing[@]} apt packages"
  $SUDO apt-get update -q >/dev/null &&
    DEBIAN_FRONTEND=noninteractive $SUDO apt-get install -y -q --no-install-recommends "${missing[@]}" >/dev/null ||
    say "apt-get failed; the app won't build on Linux, nor R notebooks run, until it succeeds"
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
  UV_TOOL_BIN_DIR=/usr/local/bin uv tool install --quiet --force "marimo==$MARIMO_VERSION" ||
    say "couldn't install marimo"
}

rust_components() {
  command -v rustup >/dev/null || return 0
  rustup toolchain install --profile minimal -c clippy -c rustfmt "$RUST_TOOLCHAIN" >/dev/null 2>&1 ||
    say "couldn't add clippy and rustfmt"
}

apt_packages &
julia_toolchain &
marimo_toolchain &
rust_components &
wait
exit 0
