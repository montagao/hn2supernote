#!/usr/bin/env bash
# Rebuild + reinstall `pti` when a merge/rebase brought in new plane-tui sources.
#
# Shared by the post-merge and post-rewrite hooks, so a plain `git pull` (or a
# `git pull --rebase`) that touches plane-tui automatically refreshes the
# installed binary. Wired up via core.hooksPath — see install-hooks.sh.
#
# This never fails the git operation: a broken build just leaves the previously
# installed pti in place, and the reason is printed to stderr.
set -uo pipefail

root="$(git rev-parse --show-toplevel 2>/dev/null)" || exit 0
cd "$root" || exit 0  # pathspecs below are repo-root-relative
sub="plane-tui"

# HEAD before this operation. ORIG_HEAD is set by merge and rebase (including
# fast-forward pulls); the reflog is the fallback.
prev="$(git rev-parse --verify --quiet ORIG_HEAD || true)"
[ -z "$prev" ] && prev="$(git rev-parse --verify --quiet 'HEAD@{1}' || true)"
[ -z "$prev" ] && exit 0

# Only rebuild when the code that goes into the binary actually moved.
changed="$(git diff --name-only "$prev" HEAD -- \
  "$sub/src" "$sub/Cargo.toml" "$sub/Cargo.lock" 2>/dev/null)"
[ -z "$changed" ] && exit 0

if ! command -v cargo >/dev/null 2>&1; then
  echo "[pti] plane-tui changed but cargo is missing — skipping auto-build" >&2
  exit 0
fi

echo "[pti] plane-tui sources changed — rebuilding + reinstalling…"
if "$root/$sub/scripts/install-pti.sh"; then
  echo "[pti] updated. Relaunch any running cockpit to pick up the new binary."
else
  echo "[pti] build failed — keeping the previously installed pti." >&2
fi
exit 0
