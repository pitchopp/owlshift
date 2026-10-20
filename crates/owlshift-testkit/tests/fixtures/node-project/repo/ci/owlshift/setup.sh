#!/bin/sh
# The fixture's stack.setup (OWL-330): installs from the pnpm store the
# operator allows read-only, through an overlay store at a stable worktree
# path, so the next run's install keeps this one's node_modules.
set -eu
store="$HOME/Library/pnpm/store/v10"
mkdir -p .pnpm-overlay/v10/projects
ln -sfn "$store/files" .pnpm-overlay/v10/files
ln -sfn "$store/index" .pnpm-overlay/v10/index
pnpm install --offline --frozen-lockfile --store-dir .pnpm-overlay
count=$(cat .setup-count 2>/dev/null || echo 0)
echo $((count + 1)) > .setup-count
