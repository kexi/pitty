#!/bin/bash
# Block until the CI workflow has a successful run for one commit, or give up.
#
# Used by release.yml's ci-gate job so publishing cannot outrun CI. Exit 0 as
# soon as any completed CI run for the commit has conclusion=success (the run
# may come from the tag push or from an earlier branch push of the same SHA).
# Exit 1 once no run is pending AND the discovery window has elapsed: a
# freshly pushed tag can take a little while to show up in the runs API, so an
# empty list (or only stale failed runs) is not final until that window passes.
#
# A poll whose `gh` call fails or returns anything but a JSON array (API 5xx,
# rate limit, network blip, empty body) is a missed poll, not a verdict: it is
# retried after the usual sleep. Only CI_GATE_MAX_MISSED_POLLS misses in a row
# give up, so a transient hiccup mid-wait cannot fail a green release while a
# dead token still fails closed instead of spinning until the job timeout.
#
# Environment (all optional except GITHUB_REPOSITORY):
#   GITHUB_REPOSITORY          owner/repo (set by Actions)
#   CI_WORKFLOW_NAME           workflow name to look for (default: CI)
#   CI_GATE_POLL_SECONDS       sleep between polls (default: 30)
#   CI_GATE_DISCOVERY_SECONDS  grace before "no run" is final (default: 600)
#   CI_GATE_MAX_MISSED_POLLS   consecutive failed polls before giving up (default: 10)
#   CI_GATE_GH_BIN             gh executable (default: gh; tests inject a fake)
set -euo pipefail

sha="${1:?usage: wait-for-ci.sh <commit-sha>}"
repo="${GITHUB_REPOSITORY:?GITHUB_REPOSITORY must be set}"
workflow="${CI_WORKFLOW_NAME:-CI}"
poll="${CI_GATE_POLL_SECONDS:-30}"
discovery="${CI_GATE_DISCOVERY_SECONDS:-600}"
max_missed="${CI_GATE_MAX_MISSED_POLLS:-10}"
gh_bin="${CI_GATE_GH_BIN:-gh}"

started=$SECONDS
missed=0
while :; do
    # `|| runs=""` keeps `set -e` from turning one failed call into a verdict;
    # the array check then treats a failure and a malformed body alike.
    runs=$("$gh_bin" run list --repo "$repo" --workflow "$workflow" \
        --commit "$sha" --limit 20 \
        --json databaseId,status,conclusion,headBranch) || runs=""
    if ! jq -e 'type == "array"' <<<"$runs" >/dev/null 2>&1; then
        missed=$((missed + 1))
        if [ "$missed" -ge "$max_missed" ]; then
            echo "gh run list failed ${missed} times in a row for ${sha}; giving up." >&2
            exit 1
        fi
        echo "gh run list failed (${missed}/${max_missed}); retrying in ${poll}s." >&2
        sleep "$poll"
        continue
    fi
    missed=0

    # One jq pass: successful runs, runs not yet completed, and all runs.
    read -r successes pending total < <(jq -r '[
        (map(select(.conclusion == "success")) | length),
        (map(select(.status != "completed")) | length),
        length
    ] | @tsv' <<<"$runs")

    if [ "$successes" -gt 0 ]; then
        echo "CI is green for ${sha}:"
        jq -r '.[] | select(.conclusion == "success") | "  run \(.databaseId) (\(.headBranch))"' <<<"$runs"
        exit 0
    fi

    elapsed=$((SECONDS - started))
    window_elapsed=$((elapsed >= discovery))
    if [ "$pending" -gt 0 ]; then
        echo "CI still running for ${sha} (${pending} run(s) pending); waiting ${poll}s."
    elif [ "$window_elapsed" -eq 1 ]; then
        echo "No successful CI run for ${sha}, none in progress, and the ${discovery}s discovery window has passed; refusing to publish." >&2
        jq -r '.[] | "  run \(.databaseId) (\(.headBranch)): \(.status)/\(.conclusion)"' <<<"$runs" >&2
        exit 1
    elif [ "$total" -eq 0 ]; then
        echo "No CI run visible yet for ${sha} (${elapsed}s into the ${discovery}s discovery window); waiting ${poll}s."
    else
        echo "Only unsuccessful CI runs visible for ${sha} so far (${total}); a tag-push run may still appear (${elapsed}s into the ${discovery}s discovery window); waiting ${poll}s."
    fi
    sleep "$poll"
done
