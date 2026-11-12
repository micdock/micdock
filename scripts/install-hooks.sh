#!/bin/sh
# Let git run scripts/check.sh before every push (undo: git config --unset core.hooksPath).
set -eu
cd "$(dirname "$0")/.."
git config core.hooksPath .githooks
echo "Installed: 'git push' now runs scripts/check.sh first (skip once with --no-verify)."
