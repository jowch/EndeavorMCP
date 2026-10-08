#!/bin/bash
# SessionStart hook for Claude Code cloud sessions on this repository alone.
# A session with several repositories (a Claude project) doesn't run repo
# hooks; there the environment's setup script does this (Endeavor's
# docs/cloud.md).
set -uo pipefail
[ "${CLAUDE_CODE_REMOTE:-}" = true ] || exit 0
cd "$CLAUDE_PROJECT_DIR" || exit 0
scripts/cloud-setup.sh --no-gui
exit 0
