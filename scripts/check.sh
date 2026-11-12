#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
# Run the same checks as CI (formatting, clippy without warnings, all tests), before pushing:
#   scripts/check.sh          everything
#   scripts/check.sh --quick  formatting and clippy only (no tests)
# CI also builds for ARM64 inside AlmaLinux 9; that part can't be checked here.
set -eu
cd "$(dirname "$0")/../src"
quick=0
[ "${1:-}" = "--quick" ] && quick=1

step() { printf '\n== %s\n' "$*"; }
step "formatting"
cargo fmt --all --check || { echo "run: (cd src && cargo fmt --all)"; exit 1; }
step "clippy"
cargo clippy --release --locked --all-targets -- -D warnings
if [ "$quick" = 0 ]; then
    step "tests"
    cargo test --release --locked
fi
printf '\nAll checks passed.\n'
