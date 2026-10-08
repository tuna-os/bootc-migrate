#!/usr/bin/env bash
# Verify a commit is release-ready before hand-pushing a tag for it.
#
# Releases are normally automatic: merging a version bump to `main` is what
# cuts them, and `required-checks` already guarantees CI and the full E2E
# matrix were green on that commit before it could merge (see RELEASING.md).
# This script is the escape hatch for the other path — hand-pushing a `v*`
# tag against a commit that is not the head of `main` — where that guarantee
# does not hold. It asks the GitHub API for `CI`'s and `E2E Migration
# Tests`' conclusion on the given commit and fails loudly if either is
# missing or non-green, so a tag is never pushed against a commit that only
# *might* have been verified.
#
# Usage:
#   ./scripts/verify-release-ready.sh [<sha-or-ref>]
#
# Defaults to HEAD. Requires `gh` authenticated against tuna-os/bootc-migrate.

set -euo pipefail

REPO="tuna-os/bootc-migrate"
REF="${1:-HEAD}"
SHA="$(git rev-parse "$REF")"

echo "Checking CI status for $SHA on $REPO..."

# Query each workflow by its file, not the repository's run list: that list
# returns 30 runs by default, and the daily "Image catalog" schedule alone
# adds a run per day on an unchanged head, so after a few weeks CI and E2E
# would fall off the first page and read as "missing" on a green commit.
check_workflow() {
  local name="$1" file="$2"
  local conclusion
  conclusion="$(gh api "repos/$REPO/actions/workflows/$file/runs?head_sha=$SHA&per_page=100" \
    --jq '.workflow_runs | sort_by(.created_at) | last | (.conclusion // .status // "missing")')"

  if [ "$conclusion" = "success" ]; then
    echo "  $name: success"
  else
    echo "  $name: $conclusion" >&2
    return 1
  fi
}

status=0
check_workflow "CI" ci.yml || status=1
check_workflow "E2E Migration Tests" e2e-tests.yml || status=1

if [ "$status" -ne 0 ]; then
  echo
  echo "FAIL: $SHA is not release-ready — a hand-pushed tag needs CI and E2E" >&2
  echo "green on this exact commit (see RELEASING.md, 'Cutting it')." >&2
  exit 1
fi

echo
echo "OK: CI and E2E Migration Tests both green on $SHA."
