#!/bin/sh
# The agent smoke suite (docs/smoke.md): builds the binary and the runner,
# then runs the tasks in smoke/tasks with Claude Code. Arguments go to the
# runner: --only N1-new,N4-long-run  --out DIR  --julia PATH  --depot DEPOT  --retries N
set -eu
cd "$(dirname "$0")/.."
cargo build --locked -p endeavor-mcp -p endeavor-smoke
exec target/debug/endeavor-smoke run "$@"
