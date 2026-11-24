#!/bin/bash
set -uo pipefail
[ "${CLAUDE_CODE_REMOTE:-}" = true ] || exit 0
cd "$CLAUDE_PROJECT_DIR" || exit 0
scripts/cloud-setup.sh --no-gui
exit 0
