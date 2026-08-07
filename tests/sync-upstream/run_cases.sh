#!/bin/sh
# Exercise scripts/sync-upstream.sh against local bare git remotes.
#
# Run from repository root:
#   sh tests/sync-upstream/run_cases.sh
#
# Optional:
#   CASE_FILTER=<case-name> sh tests/sync-upstream/run_cases.sh
set -eu

CASE_FILTER="${CASE_FILTER:-}"
KEEP_WORK="${KEEP_WORK:-0}"
REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
SCRIPT_UNDER_TEST="${REPO_ROOT}/scripts/sync-upstream.sh"
WORK="$(mktemp -d)"
PASS=0
FAIL=0
RAN=0

cleanup() {
  if [ "$KEEP_WORK" = 1 ]; then
    printf 'kept test workspace: %s\n' "$WORK"
    return 0
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

ok() {
  PASS=$((PASS + 1))
  printf '  ok   %s\n' "$1"
}

bad() {
  FAIL=$((FAIL + 1))
  printf '  FAIL %s\n' "$1"
}

run_in_repo() {
  repo="$1"
  out="$2"
  shift 2
  set +e
  (
    cd "$repo"
    "$@"
  ) >"$out" 2>&1
  status=$?
  set -e
  printf '%s\n' "$status"
}

assert_exit() {
  got="$1"
  want="$2"
  label="$3"
  if [ "$got" = "$want" ]; then
    ok "$label: exit $got"
  else
    bad "$label: expected exit $want, got $got"
  fi
}

assert_contains() {
  file="$1"
  needle="$2"
  label="$3"
  if grep -Fq "$needle" "$file"; then
    ok "$label: output contains '$needle'"
  else
    bad "$label: output missing '$needle'"
    sed 's/^/       /' "$file"
  fi
}

extract_sync_branch() {
  file="$1"
  sed -n 's/^==> Sync branch ready: //p' "$file" | tail -n 1
}

extract_state_file() {
  file="$1"
  state_path="$(sed -n 's/^state: //p; s/^resume: //p' "$file" | tail -n 1)"
  printf '%s\n' "$state_path"
}

rev_in_repo() {
  repo="$1"
  ref="$2"
  (
    cd "$repo"
    git rev-parse "$ref"
  )
}

rev_in_bare() {
  bare="$1"
  ref="$2"
  git --git-dir "$bare" rev-parse "$ref"
}

branch_exists_in_bare() {
  bare="$1"
  branch="$2"
  git --git-dir "$bare" show-ref --verify --quiet "refs/heads/$branch"
}

setup_fixture() {
  name="$1"
  CASE_DIR="${WORK}/${name}"
  UPSTREAM_SEED="${CASE_DIR}/upstream-seed"
  ORIGIN_SEED="${CASE_DIR}/origin-seed"
  REMOTES_DIR="${CASE_DIR}/remotes"
  UPSTREAM_BARE="${REMOTES_DIR}/xai-org/grok-build.git"
  ORIGIN_BARE="${REMOTES_DIR}/ImL1s/medley.git"
  WORK_REPO="${CASE_DIR}/work"

  mkdir -p "${REMOTES_DIR}/xai-org" "${REMOTES_DIR}/ImL1s"
  git init "${UPSTREAM_SEED}" >/dev/null
  (
    cd "${UPSTREAM_SEED}"
    git config user.name "Sync Test"
    git config user.email "sync-test@example.com"
    printf 'base-line\n' > shared.txt
    git add shared.txt
    git commit -m "upstream base" >/dev/null
    git branch -M main
  )

  git clone --bare "${UPSTREAM_SEED}" "${UPSTREAM_BARE}" >/dev/null

  git clone "${UPSTREAM_SEED}" "${ORIGIN_SEED}" >/dev/null
  (
    cd "${ORIGIN_SEED}"
    git config user.name "Sync Test"
    git config user.email "sync-test@example.com"
    git checkout -b providers >/dev/null
    printf 'providers-line\n' > providers.txt
    git add providers.txt
    git commit -m "providers base" >/dev/null
    git checkout main >/dev/null
  )

  git clone --bare "${ORIGIN_SEED}" "${ORIGIN_BARE}" >/dev/null
  git clone "${ORIGIN_BARE}" "${WORK_REPO}" >/dev/null
  (
    cd "${WORK_REPO}"
    git config user.name "Sync Test"
    git config user.email "sync-test@example.com"
    git remote set-url origin "file://${ORIGIN_BARE}"
    git remote add upstream "file://${UPSTREAM_BARE}"
  )
}

resolve_repo_relative_path() {
  repo="$1"
  path="$2"
  case "$path" in
  /*) printf '%s\n' "$path" ;;
  *) printf '%s/%s\n' "$repo" "$path" ;;
  esac
}

commit_on_upstream_main() {
  file="$1"
  content="$2"
  message="$3"
  (
    cd "${UPSTREAM_SEED}"
    git checkout main >/dev/null
    printf '%s\n' "$content" > "$file"
    git add "$file"
    git commit -m "$message" >/dev/null
    git push "file://${UPSTREAM_BARE}" main >/dev/null
  )
}

commit_on_origin_branch() {
  branch="$1"
  file="$2"
  content="$3"
  message="$4"
  admin="${CASE_DIR}/origin-admin-${branch}"
  rm -rf "$admin"
  git clone "${ORIGIN_BARE}" "$admin" >/dev/null
  (
    cd "$admin"
    git config user.name "Sync Test"
    git config user.email "sync-test@example.com"
    git checkout "$branch" >/dev/null
    printf '%s\n' "$content" > "$file"
    git add "$file"
    git commit -m "$message" >/dev/null
    git push origin "$branch" >/dev/null
  )
  rm -rf "$admin"
}

run_case_if_selected() {
  case_name="$1"
  if [ -n "$CASE_FILTER" ] && [ "$CASE_FILTER" != "$case_name" ]; then
    return 0
  fi
  RAN=$((RAN + 1))
  printf '== case: %s ==\n' "$case_name"
  "$case_name"
}

case_no_drift() {
  setup_fixture "no_drift"
  out="${CASE_DIR}/out.txt"
  origin_main_before="$(rev_in_bare "${ORIGIN_BARE}" main)"
  status="$(run_in_repo "${WORK_REPO}" "$out" env "SYNC_UPSTREAM_ROOT_OVERRIDE=${WORK_REPO}" "${SCRIPT_UNDER_TEST}")"
  assert_exit "$status" 0 "no_drift"
  sync_branch="$(extract_sync_branch "$out")"
  if [ -n "$sync_branch" ]; then
    ok "no_drift: reported sync branch ${sync_branch}"
  else
    bad "no_drift: did not report sync branch"
    sed 's/^/       /' "$out"
  fi
  origin_main_after="$(rev_in_bare "${ORIGIN_BARE}" main)"
  if [ "$origin_main_before" = "$origin_main_after" ]; then
    ok "no_drift: origin/main unchanged"
  else
    bad "no_drift: origin/main changed unexpectedly"
  fi
  if branch_exists_in_bare "${ORIGIN_BARE}" "$sync_branch"; then
    ok "no_drift: pushed sync branch exists on origin"
  else
    bad "no_drift: sync branch missing on origin"
  fi
}

case_fast_forward_drift() {
  setup_fixture "fast_forward_drift"
  out="${CASE_DIR}/out.txt"
  origin_main_before="$(rev_in_bare "${ORIGIN_BARE}" main)"
  commit_on_upstream_main "shared.txt" "upstream-fast-forward" "upstream drift"
  upstream_main_after="$(rev_in_bare "${UPSTREAM_BARE}" main)"
  status="$(run_in_repo "${WORK_REPO}" "$out" env "SYNC_UPSTREAM_ROOT_OVERRIDE=${WORK_REPO}" "${SCRIPT_UNDER_TEST}")"
  assert_exit "$status" 0 "fast_forward_drift"
  origin_main_after="$(rev_in_bare "${ORIGIN_BARE}" main)"
  if [ "$origin_main_after" = "$upstream_main_after" ] && [ "$origin_main_after" != "$origin_main_before" ]; then
    ok "fast_forward_drift: origin/main fast-forwarded to upstream/main"
  else
    bad "fast_forward_drift: origin/main did not fast-forward correctly"
  fi
  sync_branch="$(extract_sync_branch "$out")"
  if branch_exists_in_bare "${ORIGIN_BARE}" "$sync_branch"; then
    ok "fast_forward_drift: pushed sync branch exists on origin"
  else
    bad "fast_forward_drift: sync branch missing on origin"
  fi
}

case_merge_conflict() {
  setup_fixture "merge_conflict"
  out="${CASE_DIR}/out.txt"
  commit_on_origin_branch "providers" "shared.txt" "providers-side" "providers conflict"
  commit_on_upstream_main "shared.txt" "upstream-side" "upstream conflict"
  origin_main_before="$(rev_in_bare "${ORIGIN_BARE}" main)"
  upstream_main_after="$(rev_in_bare "${UPSTREAM_BARE}" main)"
  status="$(run_in_repo "${WORK_REPO}" "$out" env "SYNC_UPSTREAM_ROOT_OVERRIDE=${WORK_REPO}" "${SCRIPT_UNDER_TEST}")"
  if [ "$status" -ne 0 ]; then
    ok "merge_conflict: exits non-zero"
  else
    bad "merge_conflict: expected non-zero exit"
  fi
  assert_contains "$out" "merge conflict while merging main" "merge_conflict"
  origin_main_after="$(rev_in_bare "${ORIGIN_BARE}" main)"
  if [ "$origin_main_before" = "$origin_main_after" ] && [ "$origin_main_after" != "$upstream_main_after" ]; then
    ok "merge_conflict: origin/main not updated on conflict"
  else
    bad "merge_conflict: origin/main changed despite conflict"
  fi
}

case_same_day_retries() {
  setup_fixture "same_day_retries"
  out1="${CASE_DIR}/out1.txt"
  out2="${CASE_DIR}/out2.txt"
  status1="$(run_in_repo "${WORK_REPO}" "$out1" env "SYNC_UPSTREAM_ROOT_OVERRIDE=${WORK_REPO}" "${SCRIPT_UNDER_TEST}" --no-push)"
  status2="$(run_in_repo "${WORK_REPO}" "$out2" env "SYNC_UPSTREAM_ROOT_OVERRIDE=${WORK_REPO}" "${SCRIPT_UNDER_TEST}" --no-push)"
  assert_exit "$status1" 0 "same_day_retries first run"
  assert_exit "$status2" 0 "same_day_retries second run"
  branch1="$(extract_sync_branch "$out1")"
  branch2="$(extract_sync_branch "$out2")"
  if [ "$branch1" != "$branch2" ] && [ -n "$branch1" ] && [ -n "$branch2" ]; then
    ok "same_day_retries: branch names are collision-free"
  else
    bad "same_day_retries: branch names collided"
  fi
  case "$branch2" in
  "${branch1}"-2) ok "same_day_retries: second branch uses sequence suffix" ;;
  *) bad "same_day_retries: expected '${branch1}-2', got '${branch2}'" ;;
  esac
}

case_dirty_untracked_in_progress() {
  setup_fixture "dirty_state"
  out_dirty="${CASE_DIR}/out-dirty.txt"
  (
    cd "${WORK_REPO}"
    printf 'dirty\n' >> shared.txt
  )
  status_dirty="$(run_in_repo "${WORK_REPO}" "$out_dirty" env "SYNC_UPSTREAM_ROOT_OVERRIDE=${WORK_REPO}" "${SCRIPT_UNDER_TEST}")"
  if [ "$status_dirty" -ne 0 ]; then
    ok "dirty_state: tracked dirty tree rejected"
  else
    bad "dirty_state: expected tracked dirty tree rejection"
  fi
  assert_contains "$out_dirty" "tracked working tree is dirty" "dirty_state"

  setup_fixture "untracked_collision"
  out_untracked="${CASE_DIR}/out-untracked.txt"
  (
    cd "${WORK_REPO}"
    printf 'collision\n' > providers.txt
  )
  status_untracked="$(run_in_repo "${WORK_REPO}" "$out_untracked" env "SYNC_UPSTREAM_ROOT_OVERRIDE=${WORK_REPO}" "${SCRIPT_UNDER_TEST}")"
  if [ "$status_untracked" -ne 0 ]; then
    ok "untracked_collision: collision rejected"
  else
    bad "untracked_collision: expected rejection"
  fi
  assert_contains "$out_untracked" "would be overwritten by checkout" "untracked_collision"

  setup_fixture "in_progress_state"
  out_in_progress="${CASE_DIR}/out-in-progress.txt"
  (
    cd "${WORK_REPO}"
    printf '%s\n' "$(git rev-parse HEAD)" > .git/MERGE_HEAD
  )
  status_in_progress="$(run_in_repo "${WORK_REPO}" "$out_in_progress" env "SYNC_UPSTREAM_ROOT_OVERRIDE=${WORK_REPO}" "${SCRIPT_UNDER_TEST}")"
  if [ "$status_in_progress" -ne 0 ]; then
    ok "in_progress_state: in-progress operation rejected"
  else
    bad "in_progress_state: expected rejection"
  fi
  assert_contains "$out_in_progress" "in-progress git operation" "in_progress_state"
}

case_malicious_remote() {
  setup_fixture "malicious_remote"
  out="${CASE_DIR}/out.txt"
  (
    cd "${WORK_REPO}"
    git remote set-url upstream "file://${CASE_DIR}/remotes/xai-org/grok-build-malicious.git"
  )
  status="$(run_in_repo "${WORK_REPO}" "$out" env "SYNC_UPSTREAM_ROOT_OVERRIDE=${WORK_REPO}" "${SCRIPT_UNDER_TEST}")"
  if [ "$status" -ne 0 ]; then
    ok "malicious_remote: exits non-zero"
  else
    bad "malicious_remote: expected non-zero exit"
  fi
  assert_contains "$out" "expected 'xai-org/grok-build'" "malicious_remote"
}

case_failed_validation() {
  setup_fixture "failed_validation"
  out="${CASE_DIR}/out.txt"
  commit_on_upstream_main "shared.txt" "upstream-validation" "upstream drift for validation"
  origin_main_before="$(rev_in_bare "${ORIGIN_BARE}" main)"
  status="$(run_in_repo "${WORK_REPO}" "$out" env "SYNC_UPSTREAM_ROOT_OVERRIDE=${WORK_REPO}" SYNC_UPSTREAM_VALIDATE_CMD=false "${SCRIPT_UNDER_TEST}")"
  if [ "$status" -ne 0 ]; then
    ok "failed_validation: exits non-zero"
  else
    bad "failed_validation: expected non-zero exit"
  fi
  assert_contains "$out" "validation command failed" "failed_validation"
  origin_main_after="$(rev_in_bare "${ORIGIN_BARE}" main)"
  if [ "$origin_main_before" = "$origin_main_after" ]; then
    ok "failed_validation: no remote ref updated"
  else
    bad "failed_validation: remote main changed despite failed validation"
  fi
}

case_interruption_resume() {
  setup_fixture "interruption_resume"
  out1="${CASE_DIR}/out1.txt"
  out2="${CASE_DIR}/out2.txt"
  commit_on_upstream_main "shared.txt" "upstream-resume" "upstream drift for resume"
  status1="$(run_in_repo "${WORK_REPO}" "$out1" env "SYNC_UPSTREAM_ROOT_OVERRIDE=${WORK_REPO}" SYNC_UPSTREAM_TEST_INTERRUPT_AFTER_PHASE=prepare_mirror "${SCRIPT_UNDER_TEST}")"
  assert_exit "$status1" 130 "interruption_resume first run"
  state_file="$(extract_state_file "$out1")"
  state_file_abs="$(resolve_repo_relative_path "${WORK_REPO}" "$state_file")"
  if [ -n "$state_file" ] && [ -f "$state_file_abs" ]; then
    ok "interruption_resume: state file captured"
  else
    bad "interruption_resume: missing state file after interruption"
  fi
  status2="$(run_in_repo "${WORK_REPO}" "$out2" env "SYNC_UPSTREAM_ROOT_OVERRIDE=${WORK_REPO}" "${SCRIPT_UNDER_TEST}" --resume "$state_file" --no-push)"
  assert_exit "$status2" 0 "interruption_resume resume run"
  assert_contains "$out2" "sync finished successfully" "interruption_resume"
}

case_push_rejection() {
  setup_fixture "push_rejection"
  out="${CASE_DIR}/out.txt"
  commit_on_upstream_main "shared.txt" "upstream-push-reject" "upstream drift for push rejection"
  upstream_main_after="$(rev_in_bare "${UPSTREAM_BARE}" main)"
  race_script="${CASE_DIR}/origin-main-race.sh"
  cat > "$race_script" <<EOF
#!/bin/sh
set -eu
tmp_repo=\$(mktemp -d)
git clone "file://${ORIGIN_BARE}" "\$tmp_repo/repo" >/dev/null 2>&1
(
  cd "\$tmp_repo/repo"
  git config user.name "Sync Test"
  git config user.email "sync-test@example.com"
  git checkout main >/dev/null
  printf 'origin-race\\n' > race.txt
  git add race.txt
  git commit -m "origin race during validation" >/dev/null
  git push origin main >/dev/null
)
rm -rf "\$tmp_repo"
EOF
  chmod 755 "$race_script"

  status="$(run_in_repo "${WORK_REPO}" "$out" env "SYNC_UPSTREAM_ROOT_OVERRIDE=${WORK_REPO}" "SYNC_UPSTREAM_VALIDATE_CMD=${race_script}" "${SCRIPT_UNDER_TEST}")"
  if [ "$status" -ne 0 ]; then
    ok "push_rejection: exits non-zero"
  else
    bad "push_rejection: expected non-zero exit"
  fi
  assert_contains "$out" "push failed" "push_rejection"
  origin_main_after="$(rev_in_bare "${ORIGIN_BARE}" main)"
  if [ "$origin_main_after" != "$upstream_main_after" ]; then
    ok "push_rejection: origin/main did not advance to local main after rejection"
  else
    bad "push_rejection: origin/main unexpectedly advanced to local main"
  fi
}

case_resume_abort() {
  setup_fixture "resume_abort"
  out1="${CASE_DIR}/out1.txt"
  out2="${CASE_DIR}/out2.txt"
  status1="$(run_in_repo "${WORK_REPO}" "$out1" env "SYNC_UPSTREAM_ROOT_OVERRIDE=${WORK_REPO}" SYNC_UPSTREAM_TEST_INTERRUPT_AFTER_PHASE=fetch_verify "${SCRIPT_UNDER_TEST}")"
  assert_exit "$status1" 130 "resume_abort interrupted run"
  state_file="$(extract_state_file "$out1")"
  state_file_abs="$(resolve_repo_relative_path "${WORK_REPO}" "$state_file")"
  if [ -n "$state_file" ] && [ -f "$state_file_abs" ]; then
    ok "resume_abort: state file captured"
  else
    bad "resume_abort: missing state file after interruption"
  fi
  status2="$(run_in_repo "${WORK_REPO}" "$out2" env "SYNC_UPSTREAM_ROOT_OVERRIDE=${WORK_REPO}" "${SCRIPT_UNDER_TEST}" --resume "$state_file" --abort)"
  assert_exit "$status2" 0 "resume_abort abort command"
  assert_contains "$out2" "aborted state" "resume_abort"
  if [ ! -e "$state_file_abs" ]; then
    ok "resume_abort: state file removed"
  else
    bad "resume_abort: state file still exists"
  fi
}

case_check_dry_run_json() {
  setup_fixture "check_dry_run_json"
  out_check="${CASE_DIR}/out-check.txt"
  out_dry_json="${CASE_DIR}/out-dry-json.txt"
  commit_on_upstream_main "shared.txt" "upstream-check" "upstream drift for check"
  status_check="$(run_in_repo "${WORK_REPO}" "$out_check" env "SYNC_UPSTREAM_ROOT_OVERRIDE=${WORK_REPO}" "${SCRIPT_UNDER_TEST}" --check)"
  assert_exit "$status_check" 2 "check mode drift exit"
  assert_contains "$out_check" "drift detected" "check mode drift output"

  status_dry_json="$(run_in_repo "${WORK_REPO}" "$out_dry_json" env "SYNC_UPSTREAM_ROOT_OVERRIDE=${WORK_REPO}" "${SCRIPT_UNDER_TEST}" --dry-run --json --no-push)"
  assert_exit "$status_dry_json" 0 "dry-run json exit"
  assert_contains "$out_dry_json" "\"phase\":\"dry_run\"" "dry-run json phase"
  assert_contains "$out_dry_json" "\"next_action\":" "dry-run json next action"
  if [ -z "$(cd "${WORK_REPO}" && git for-each-ref --format='%(refname:short)' 'refs/heads/sync/upstream-*')" ]; then
    ok "dry-run json: no sync branch created"
  else
    bad "dry-run json: sync branch was created unexpectedly"
  fi
}

echo "== sync-upstream cases =="

run_case_if_selected case_no_drift
run_case_if_selected case_fast_forward_drift
run_case_if_selected case_merge_conflict
run_case_if_selected case_same_day_retries
run_case_if_selected case_dirty_untracked_in_progress
run_case_if_selected case_malicious_remote
run_case_if_selected case_failed_validation
run_case_if_selected case_interruption_resume
run_case_if_selected case_push_rejection
run_case_if_selected case_resume_abort
run_case_if_selected case_check_dry_run_json

if [ "$RAN" = 0 ]; then
  echo "No cases ran. CASE_FILTER='${CASE_FILTER}' did not match."
  exit 1
fi

echo
echo "== ${PASS} passed, ${FAIL} failed =="
[ "$FAIL" = 0 ]
