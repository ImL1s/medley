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
#   2. that commit has a successful run of *this* repository's ci.yml
#      (absent / queued / skipped are distinct fail-closed verdicts), and
#   3. every required check on the PR has actually concluded successfully
#      (empty `gh pr checks` is "no checks reported", not a pass).
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
for flag in "${MERGE_FLAGS[@]}"; do
  case "$flag" in
  --auto | --auto=*)
    die "deferred auto-merge is not supported; metadata must be checked at merge time"
    ;;
  --admin | --admin=*)
    die "administrator merge is not supported; required checks and queue membership must hold"
    ;;
  --match-head-commit | --match-head-commit=*)
    die "match-head-commit is bound to the verified head; do not override it"
    ;;
  --repo | --repo=* | -R | -R*)
    die "repository is bound to this checkout; do not override it"
    ;;
  -t* | -b* | -F* | --subject | --subject=* | --body | --body=* | --body-file | --body-file=*)
    die "custom merge messages are not supported; update the PR or source commit text instead"
    ;;
  --*) ;;
  -*)
    # gh parses clustered shorts such as -sbCUSTOM as -s plus -b CUSTOM.
    letters="${flag#-}"
    case "$letters" in
    *[tbF]*)
      die "custom merge messages are not supported; update the PR or source commit text instead"
      ;;
    *R*)
      die "repository is bound to this checkout; do not override it"
      ;;
    esac
    ;;
  esac
done

command -v gh >/dev/null 2>&1 || die "gh is required"
command -v python3 >/dev/null 2>&1 || die "python3 is required"

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
GUARD="$REPO_ROOT/scripts/check_pr_head_ci_run.py"
[ -f "$GUARD" ] || die "missing $GUARD"
CLOSING_GUARD="$REPO_ROOT/scripts/check_negated_closing_keywords.py"
[ -f "$CLOSING_GUARD" ] || die "missing $CLOSING_GUARD"

echo "==> Reading PR #$PR"
pr_json="$(gh pr view "$PR" --repo "$REPO" --json headRefName,headRefOid,baseRefName,state,isDraft,autoMergeRequest,id)"
read -r BRANCH HEAD BASE STATE DRAFT PR_NODE <<EOF
$(
  printf '%s' "$pr_json" | python3 -c '
import json, sys
d = json.load(sys.stdin)
if d.get("autoMergeRequest"):
    sys.stderr.write("merge-pr: auto-merge or merge-queue deferral is already enabled\n")
    sys.exit(1)
print(
    d["headRefName"],
    d["headRefOid"],
    d.get("baseRefName") or "",
    d["state"],
    d["isDraft"],
    d.get("id") or "",
)
'
)
EOF

[ "$STATE" = "OPEN" ] || die "PR #$PR is $STATE, not OPEN"
[ "$DRAFT" = "False" ] || die "PR #$PR is a draft"
[ -n "$PR_NODE" ] || die "PR #$PR has no node id"
[ -n "$BASE" ] || die "PR #$PR has no base branch"

echo "==> Rejecting PRs already in the merge queue"
queued_json="$(
  gh api graphql \
    -f query='query($id:ID!){node(id:$id){... on PullRequest{isInMergeQueue}}}' \
    -F "id=$PR_NODE"
)" || die "could not query merge-queue membership"
printf '%s' "$queued_json" | python3 -c '
import json, sys
d = json.load(sys.stdin)
node = (d.get("data") or {}).get("node") or {}
if node.get("isInMergeQueue"):
    sys.stderr.write("merge-pr: PR is already in the merge queue\n")
    sys.exit(1)
' || die "PR is already in the merge queue"

echo "==> Rejecting merge-queue-required base branches"
# `gh pr merge` on a queue-required base enqueues rather than merging
# synchronously. The queue can land the PR after a post-scan title/body
# edit that `--match-head-commit` cannot see, so refuse before enqueue
# (#530 review).
case "$REPO" in
*/*) MQ_OWNER="${REPO%%/*}" MQ_NAME="${REPO#*/}" ;;
*) die "repository must be owner/name, got '$REPO'" ;;
esac
mq_json="$(
  gh api graphql \
    -f query='query($o:String!,$n:String!,$b:String!){repository(owner:$o,name:$n){mergeQueue(branch:$b){id}}}' \
    -F "o=$MQ_OWNER" \
    -F "n=$MQ_NAME" \
    -F "b=$BASE"
)" || die "could not query merge queue for base branch $BASE"
printf '%s' "$mq_json" | python3 -c '
import json, sys
d = json.load(sys.stdin)
repo = (d.get("data") or {}).get("repository") or {}
if repo.get("mergeQueue"):
    sys.stderr.write(
        "merge-pr: base branch requires a merge queue; "
        "deferred/queued merge cannot bind the closing-keyword digest\n"
    )
    sys.exit(1)
' || die "base branch requires a merge queue; deferred merge is not supported"

echo "    branch: $BRANCH"
echo "    base:   $BASE"
echo "    head:   $HEAD"

echo "==> Reconciling the PR head against the remote branch tip"
TIP="$(git ls-remote "$REMOTE" "refs/heads/$BRANCH" | cut -f1)"
[ -n "$TIP" ] || die "$REMOTE has no refs/heads/$BRANCH"
[ "$TIP" = "$HEAD" ] || die "PR head $HEAD != remote tip $TIP -- the branch moved; re-check and retry"

echo "==> Reporting check-run history for every PR head"
python3 -B "$GUARD" --report-pr-heads "$PR" --repo "$REPO" ||
  die "could not reconcile PR head history"

echo "==> Requiring a successful ci.yml run for exactly $HEAD"
python3 -B "$GUARD" --branch "$BRANCH" --head-sha "$HEAD" --pr "$PR" --repo "$REPO" --remote "$REMOTE" ||
  die "no successful CI run for $HEAD"

echo "==> Requiring every check on the PR to have concluded successfully"
# Empty `gh pr checks` is the #202 shape: the command prints "no checks
# reported", which reads like a pass. The evaluator fail-closes on absent,
# and distinguishes pending (in progress) from skip-only from failed.
gh pr checks "$PR" --repo "$REPO" --json name,state,bucket |
  python3 -B "$GUARD" --evaluate-pr-checks ||
  die "checks are not green"

echo "==> Re-reading the head after the receipt"
# The window this closes: everything above described `$HEAD`. A push that lands
# between the receipt and the merge would be merged without any of it.
TIP_AGAIN="$(git ls-remote "$REMOTE" "refs/heads/$BRANCH" | cut -f1)"
[ "$TIP_AGAIN" = "$HEAD" ] ||
  die "branch moved to $TIP_AGAIN during verification -- nothing was merged; re-run"

echo "==> Re-reading merge metadata for negated GitHub closing keywords"
scan_once() {
  python3 -B "$CLOSING_GUARD" --pr "$PR" --repo "$REPO" --print-digest
}
scan_out="$(scan_once)" || die "PR or source commit text contains an unsafe negated closing keyword"
printf '%s\n' "$scan_out"
digest="$(printf '%s\n' "$scan_out" | sed -n 's/^digest: //p')"
[ -n "$digest" ] || die "closing-keyword guard did not emit a metadata digest"

echo "==> Re-reading the base branch immediately before merge"
# A retarget after the initial base/mergeQueue probe would otherwise let
# `gh pr merge` enqueue onto a newly queue-required base (#530 review).
# Reuse the same --json field set as the opening probe so wrappers/tests
# that stub that exact query keep working.
base_again="$(
  gh pr view "$PR" --repo "$REPO" --json headRefName,headRefOid,baseRefName,state,isDraft,autoMergeRequest,id |
    python3 -c 'import json,sys; print((json.load(sys.stdin).get("baseRefName") or ""))'
)" || die "could not re-read PR base branch"
[ -n "$base_again" ] || die "PR #$PR has no base branch on re-read"
[ "$base_again" = "$BASE" ] ||
  die "PR base retargeted from $BASE to $base_again during verification; nothing was merged"
mq_again="$(
  gh api graphql \
    -f query='query($o:String!,$n:String!,$b:String!){repository(owner:$o,name:$n){mergeQueue(branch:$b){id}}}' \
    -F "o=$MQ_OWNER" \
    -F "n=$MQ_NAME" \
    -F "b=$base_again"
)" || die "could not re-query merge queue for base branch $base_again"
printf '%s' "$mq_again" | python3 -c '
import json, sys
d = json.load(sys.stdin)
repo = (d.get("data") or {}).get("repository") or {}
if repo.get("mergeQueue"):
    sys.stderr.write(
        "merge-pr: base branch requires a merge queue on re-read; "
        "deferred/queued merge cannot bind the closing-keyword digest\n"
    )
    sys.exit(1)
' || die "base branch requires a merge queue on re-read; deferred merge is not supported"

echo "==> Merging #$PR (${MERGE_FLAGS[*]})"
# Title/body can change without moving HEAD. Re-scan immediately before
# merge and require the same digest the first scan produced.
scan_out="$(scan_once)" || die "PR or source commit text contains an unsafe negated closing keyword"
digest_again="$(printf '%s\n' "$scan_out" | sed -n 's/^digest: //p')"
[ "$digest_again" = "$digest" ] ||
  die "PR title/body changed after the closing-keyword scan; nothing was merged"
# Bind the merge to the SHA we just verified. A push that lands after
# TIP_AGAIN / the closing-keyword scan would otherwise be merged against
# receipts for a different head (#513 review).
# `gh pr merge` can enqueue into a required merge queue and then exit
# nonzero (lost HTTP body). `set -e` must not skip dequeue (#530 review).
if ! gh pr merge "$PR" --repo "$REPO" --match-head-commit "$HEAD" "${MERGE_FLAGS[@]}"; then
  true
fi
# Squash/rebase rewrite the landed SHA. Tagging and release receipts must
# bind that merge commit, not the pre-merge PR head (#333).
# `gh pr merge` can enqueue into a required merge queue while leaving the
# PR OPEN. A transient failure from this next `gh pr view` must still
# enter the dequeue path; `set -e` would otherwise leave it queued
# (#530 review).
merged_json=""
if ! merged_json="$(gh pr view "$PR" --repo "$REPO" --json mergeCommit,mergedAt,state,headRefOid)"; then
  merged_json=""
fi
if ! merge_sha="$(
  printf '%s' "$merged_json" | python3 -c '
import json, sys
raw = sys.stdin.read()
if not raw.strip():
    sys.exit(2)
try:
    d = json.loads(raw)
except json.JSONDecodeError:
    sys.exit(2)
if d.get("state") != "MERGED":
    sys.exit(2)
# Another actor can merge a newer head after our gh pr merge failed;
# bind success to the head we actually verified (#530 review).
if (d.get("headRefOid") or "") != sys.argv[1]:
    sys.exit(2)
commit = d.get("mergeCommit") or {}
oid = commit.get("oid") or ""
if not oid:
    sys.exit(2)
print(oid)
' "$HEAD"
)"; then
  # `--disable-auto` only cancels auto-merge (`DisablePullRequestAutoMerge`).
  # A merge-queue enqueue leaves the PR OPEN and can still land later after
  # title/body edits, so dequeue the checked PR as well.
  gh pr merge "$PR" --repo "$REPO" --disable-auto >/dev/null 2>&1 || true
  [ -n "$PR_NODE" ] || die "could not dequeue merge-queue entry"
  dequeue_json=""
  dequeue_json="$(
    gh api graphql \
      -f query='mutation($id:ID!){dequeuePullRequest(input:{pullRequestId:$id}){clientMutationId}}' \
      -F "id=$PR_NODE" \
      2>/dev/null
  )" || true
  dequeue_classify=0
  printf '%s' "$dequeue_json" | python3 -c '
import json, sys
raw = sys.stdin.read()
if not raw.strip():
    sys.exit(2)
d = json.loads(raw)
if (d.get("data") or {}).get("dequeuePullRequest"):
    sys.exit(0)
msgs = " ".join(
    str(err.get("message") or "") for err in (d.get("errors") or [])
).lower()
# Not queued is a successful cleanup: the mutation has nothing to remove.
if "queue" in msgs and "not" in msgs:
    sys.exit(0)
sys.exit(1)
' || dequeue_classify=$?
  if [ "$dequeue_classify" -eq 2 ]; then
    die "could not dequeue merge-queue entry"
  fi
  if [ "$dequeue_classify" -ne 0 ]; then
    die "dequeue did not remove the PR from the merge queue"
  fi
  queued_json="$(
    gh api graphql \
      -f query='query($id:ID!){node(id:$id){... on PullRequest{isInMergeQueue}}}' \
      -F "id=$PR_NODE" \
      2>/dev/null
  )" || die "could not dequeue merge-queue entry"
  if ! printf '%s' "$queued_json" | python3 -c '
import json, sys
d = json.load(sys.stdin)
node = (d.get("data") or {}).get("node") or {}
if node.get("isInMergeQueue") is not False:
    sys.exit(1)
'; then
    die "dequeue did not remove the PR from the merge queue"
  fi
  die "PR was not MERGED; deferred/queued merge is not supported"
fi
# Another actor can edit title/body and land the same head after our
# `gh pr merge` failed; headRefOid alone does not prove the scanned
# metadata landed (#530 review).
scan_out="$(scan_once)" ||
  die "merged PR metadata now contains an unsafe negated closing keyword"
digest_final="$(printf '%s\n' "$scan_out" | sed -n 's/^digest: //p')"
[ "$digest_final" = "$digest" ] ||
  die "PR title/body changed after the closing-keyword scan; landed merge metadata was not the scanned digest"
echo "==> Merged #$PR"
echo "    pr_head:      $HEAD"
echo "    merge_commit: ${merge_sha:-unknown}"
[ -n "$merge_sha" ] || die "gh did not report a mergeCommit after merge; do not tag $HEAD"
