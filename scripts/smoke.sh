#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
cargo build --locked -p endeavor-mcp -p endeavor-smoke
exec target/debug/endeavor-smoke run "$@"
