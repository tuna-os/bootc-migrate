#!/usr/bin/env bash
# Verify a commit is release-ready before it is tagged.
#
# release.yml triggers on any `v*` tag push with no gate of its own — it
# does not check that the tagged commit's CI or E2E matrix passed. RELEASING.md
# "Before tagging" step 2 requires the full E2E matrix green on the exact
# commit being tagged, but that has been a purely manual, memory-dependent
# check. This script makes it a command instead: it asks the GitHub API for
# ci.yml's and e2e-tests.yml's conclusion on the given commit and fails
# loudly if either is missing or non-green, so a tag is never pushed against
# a commit that only *might* have been verified.
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

check_workflow() {
  local workflow="$1"
  local conclusion
  conclusion="$(gh api "repos/$REPO/actions/runs?head_sha=$SHA" \
    --jq "[.workflow_runs[] | select(.name==\"$workflow\")] | sort_by(.created_at) | last | .conclusion // \"missing\"")"

  if [ "$conclusion" = "success" ]; then
    echo "  $workflow: success"
  else
    echo "  $workflow: $conclusion" >&2
    return 1
  fi
}

status=0
check_workflow "CI" || status=1
check_workflow "E2E Migration Tests" || status=1

if [ "$status" -ne 0 ]; then
  echo
  echo "FAIL: $SHA is not release-ready — see RELEASING.md 'Before tagging'." >&2
  echo "A red or missing run here means the E2E matrix has not been proven" >&2
  echo "green on this exact commit. Do not tag until it has." >&2
  exit 1
fi

echo
echo "OK: CI and E2E Tests both green on $SHA."
