#!/usr/bin/env bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
#
# The branch protection the release flow needs (CONTRIBUTING.md, "Releases"), once the release
# workflows are on main:
#
#   1. `develop` exists, cut from `main` when it does not;
#   2. `main` requires the check `conventional PR title` (.github/workflows/release-checks.yml) on
#      top of the checks it requires already;
#   3. `develop` gets `main`'s protection as it then is: the same required checks, one approving
#      review with code owners, signed commits, no force push, no deletion.
#
#   tools/release/protect-branches.sh            # print what it would send
#   tools/release/protect-branches.sh --apply    # send it (a repository administrator's gh login)
#
# Idempotent: run again, it sends the same protection.
set -euo pipefail

repo="${REPO:-bloque-app/aledgent}"
apply=0
[ "${1:-}" = --apply ] && apply=1
title_check="conventional PR title"

current="$(gh api "repos/$repo/branches/main/protection")"
# main's protection, as the PUT endpoint takes it, with the title check added to its checks.
protection="$(jq --arg check "$title_check" '{
  required_status_checks: {
    strict: .required_status_checks.strict,
    checks: ((.required_status_checks.checks | map({context, app_id} | with_entries(select(.value != null))))
             + [{context: $check}] | unique_by(.context))
  },
  enforce_admins: .enforce_admins.enabled,
  required_pull_request_reviews: {
    dismiss_stale_reviews: .required_pull_request_reviews.dismiss_stale_reviews,
    require_code_owner_reviews: .required_pull_request_reviews.require_code_owner_reviews,
    require_last_push_approval: .required_pull_request_reviews.require_last_push_approval,
    required_approving_review_count: 1
  },
  restrictions: null,
  required_linear_history: .required_linear_history.enabled,
  allow_force_pushes: false,
  allow_deletions: false,
  block_creations: .block_creations.enabled,
  required_conversation_resolution: .required_conversation_resolution.enabled,
  lock_branch: false,
  allow_fork_syncing: .allow_fork_syncing.enabled
}' <<<"$current")"

echo "protection for main and develop:"
jq . <<<"$protection"
if [ "$apply" = 0 ]; then
  gh api "repos/$repo/branches/develop" > /dev/null 2>&1 \
    && echo "develop exists" || echo "develop would be cut from main at $(gh api "repos/$repo/commits/main" --jq .sha)"
  echo "(dry run: --apply sends it)"
  exit 0
fi

if ! gh api "repos/$repo/branches/develop" > /dev/null 2>&1; then
  sha="$(gh api "repos/$repo/commits/main" --jq .sha)"
  gh api --method POST "repos/$repo/git/refs" -f ref=refs/heads/develop -f sha="$sha" > /dev/null
  echo "develop cut from main at $sha"
fi
for b in main develop; do
  gh api --method PUT "repos/$repo/branches/$b/protection" --input - <<<"$protection" > /dev/null
  # Signed commits are their own endpoint.
  gh api --method POST "repos/$repo/branches/$b/protection/required_signatures" > /dev/null
  echo "$b protected"
done
