#!/usr/bin/env bash
# Block until the CI workflow has a successful run for one commit, or give up.
#
# Used by release.yml's ci-gate job so publishing cannot outrun CI. Exit 0 as
# soon as any completed CI run for the commit has conclusion=success (the run
# may come from the tag push or from an earlier branch push of the same SHA).
# Exit 1 once no run is pending AND the discovery window has elapsed: a
# freshly pushed tag can take a little while to show up in the runs API, so an
# empty list (or only stale failed runs) is not final until that window passes.
#
# Environment (all optional except GITHUB_REPOSITORY):
#   GITHUB_REPOSITORY          owner/repo (set by Actions)
#   CI_WORKFLOW_NAME           workflow name to look for (default: CI)
#   CI_GATE_POLL_SECONDS       sleep between polls (default: 30)
#   CI_GATE_DISCOVERY_SECONDS  grace before "no run" is final (default: 600)
#   CI_GATE_GH_BIN             gh executable (default: gh; tests inject a fake)
set -euo pipefail

sha="${1:?usage: wait-for-ci.sh <commit-sha>}"
repo="${GITHUB_REPOSITORY:?GITHUB_REPOSITORY must be set}"
workflow="${CI_WORKFLOW_NAME:-CI}"
poll="${CI_GATE_POLL_SECONDS:-30}"
discovery="${CI_GATE_DISCOVERY_SECONDS:-600}"
gh_bin="${CI_GATE_GH_BIN:-gh}"

started=$SECONDS
while :; do
  runs=$("$gh_bin" run list --repo "$repo" --workflow "$workflow" \
    --commit "$sha" --limit 20 \
    --json databaseId,status,conclusion,headBranch)

  successes=$(jq 'map(select(.conclusion == "success")) | length' <<<"$runs")
  if [ "$successes" -gt 0 ]; then
    echo "CI is green for ${sha}:"
    jq -r '.[] | select(.conclusion == "success") | "  run \(.databaseId) (\(.headBranch))"' <<<"$runs"
    exit 0
  fi

  pending=$(jq 'map(select(.status != "completed")) | length' <<<"$runs")
  elapsed=$((SECONDS - started))
  if [ "$pending" -eq 0 ] && [ "$elapsed" -ge "$discovery" ]; then
    echo "No successful CI run for ${sha}, none in progress, and the ${discovery}s discovery window has passed; refusing to publish." >&2
    jq -r '.[] | "  run \(.databaseId) (\(.headBranch)): \(.status)/\(.conclusion)"' <<<"$runs" >&2
    exit 1
  fi

  if [ "$pending" -eq 0 ]; then
    echo "No CI run visible yet for ${sha} (${elapsed}s into the ${discovery}s discovery window); waiting ${poll}s."
  else
    echo "CI still running for ${sha} (${pending} run(s) pending); waiting ${poll}s."
  fi
  sleep "$poll"
done
