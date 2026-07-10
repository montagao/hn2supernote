#!/usr/bin/env bash
# Enable the plane-tui auto-build git hooks on this machine.
#
# Points the repo's core.hooksPath at plane-tui/scripts/git-hooks, so a
# `git pull` that brings in new plane-tui sources rebuilds + reinstalls `pti`
# automatically (see _pti-autobuild.sh). Run once per clone (local, syd-dev, …).
set -euo pipefail

hooks_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/git-hooks" && pwd)"
root="$(git -C "$hooks_dir" rev-parse --show-toplevel)"

chmod +x "$hooks_dir"/* 2>/dev/null || true
git -C "$root" config core.hooksPath "$hooks_dir"

echo "hooks enabled: core.hooksPath -> $hooks_dir"
echo "pti now auto-rebuilds after 'git pull' when plane-tui sources change."
echo "(disable with: git -C \"$root\" config --unset core.hooksPath)"
