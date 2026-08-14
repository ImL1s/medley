#!/usr/bin/env bash
# The only supported way to merge a pull request in this repository (#202).
#
# `scripts/check_pr_head_ci_run.py` already answers the hard question -- does a
# successful CI run exist for *this exact head*, as opposed to for some earlier
# commit on the branch. Until now it was documented in FORK.md as something a
# human remembers to run, which is the same shape as the bug it was written for:
# a check that exists and is not on the path.
#
# This wrapper puts it on the path. It refuses to merge unless, at one instant:
#
#   1. the PR's head SHA, the remote branch tip, and the SHA the receipt was
#      taken at are the same commit, and
#   2. that commit has a successful run of *this* repository's ci.yml, and
#   3. every required check on the PR has actually concluded successfully.
#
# After a successful merge it prints both the PR head and the landed
# merge commit. Squash/rebase rewrite the SHA; tags must use merge_commit.
#
# (1) is the part a human skips. A PR whose head moved between "checks are
# green" and "merge" is green about a commit that is no longer being merged;
# re-reading the head after the receipt is what closes that window, and it is
# cheap enough that there is no reason to leave it to memory.
#
# Usage:
#   scripts/merge-pr.sh <pr-number> [--squash|--merge|--rebase] [--delete-branch]
#
# `--squash --delete-branch` is the default because that is what this repository
# does; pass others explicitly to override.
set -euo pipefail

die() {
  printf 'merge-pr: %s\n' "$1" >&2
  exit 1
}

[ $# -ge 1 ] || die "usage: scripts/merge-pr.sh <pr-number> [gh pr merge flags...]"

PR="$1"
shift
case "$PR" in
[0-9]*) ;;
*) die "first argument must be a PR number, got '$PR'" ;;
esac

REPO="${MERGE_PR_REPO:-ImL1s/medley}"
REMOTE="${MERGE_PR_REMOTE:-origin}"
MERGE_FLAGS=("$@")
[ ${#MERGE_FLAGS[@]} -gt 0 ] || MERGE_FLAGS=(--squash --delete-branch)

command -v gh >/dev/null 2>&1 || die "gh is required"
command -v python3 >/dev/null 2>&1 || die "python3 is required"

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
GUARD="$REPO_ROOT/scripts/check_pr_head_ci_run.py"
[ -f "$GUARD" ] || die "missing $GUARD"

echo "==> Reading PR #$PR"
pr_json="$(gh pr view "$PR" --repo "$REPO" --json headRefName,headRefOid,state,isDraft)"
read -r BRANCH HEAD STATE DRAFT <<EOF
$(printf '%s' "$pr_json" | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["headRefName"], d["headRefOid"], d["state"], d["isDraft"])')
EOF

[ "$STATE" = "OPEN" ] || die "PR #$PR is $STATE, not OPEN"
[ "$DRAFT" = "False" ] || die "PR #$PR is a draft"

echo "    branch: $BRANCH"
echo "    head:   $HEAD"

echo "==> Reconciling the PR head against the remote branch tip"
TIP="$(git ls-remote "$REMOTE" "refs/heads/$BRANCH" | cut -f1)"
[ -n "$TIP" ] || die "$REMOTE has no refs/heads/$BRANCH"
[ "$TIP" = "$HEAD" ] || die "PR head $HEAD != remote tip $TIP -- the branch moved; re-check and retry"

echo "==> Requiring a successful ci.yml run for exactly $HEAD"
python3 -B "$GUARD" --branch "$BRANCH" --head-sha "$HEAD" --repo "$REPO" --remote "$REMOTE" ||
  die "no successful CI run for $HEAD"

echo "==> Requiring every check on the PR to have concluded successfully"
gh pr checks "$PR" --repo "$REPO" --json name,state,bucket |
  python3 -c '
import json, sys
rows = json.load(sys.stdin)
if not rows:
    # No checks at all is the #202 shape exactly: `gh pr checks` calls that
    # "no checks reported", which reads like a pass and is not one.
    print("merge-pr: the PR reports no checks at all", file=sys.stderr)
    raise SystemExit(1)
bad = [r["name"] for r in rows if r["bucket"] not in ("pass", "skipping")]
if bad:
    print("merge-pr: not concluded successfully: " + ", ".join(bad), file=sys.stderr)
    raise SystemExit(1)
print(f"    {len(rows)} checks, all green")
' || die "checks are not green"

echo "==> Re-reading the head after the receipt"
# The window this closes: everything above described `$HEAD`. A push that lands
# between the receipt and the merge would be merged without any of it.
TIP_AGAIN="$(git ls-remote "$REMOTE" "refs/heads/$BRANCH" | cut -f1)"
[ "$TIP_AGAIN" = "$HEAD" ] ||
  die "branch moved to $TIP_AGAIN during verification -- nothing was merged; re-run"

echo "==> Merging #$PR (${MERGE_FLAGS[*]})"
gh pr merge "$PR" --repo "$REPO" "${MERGE_FLAGS[@]}"
# Squash/rebase rewrite the landed SHA. Tagging and release receipts must
# bind that merge commit, not the pre-merge PR head (#333).
merged_json="$(gh pr view "$PR" --repo "$REPO" --json mergeCommit,mergedAt,state)"
merge_sha="$(
  printf '%s' "$merged_json" | python3 -c '
import json, sys
d = json.load(sys.stdin)
commit = d.get("mergeCommit") or {}
print(commit.get("oid") or "")
'
)"
echo "==> Merged #$PR"
echo "    pr_head:      $HEAD"
echo "    merge_commit: ${merge_sha:-unknown}"
[ -n "$merge_sha" ] || die "gh did not report a mergeCommit after merge; do not tag $HEAD"
