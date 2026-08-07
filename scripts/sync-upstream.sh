#!/usr/bin/env bash
# Sync pristine main from upstream, then merge into a sync branch off providers.
# See FORK.md for the branch model and weekly workflow.
set -euo pipefail

ROOT_DEFAULT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ROOT="${SYNC_UPSTREAM_ROOT_OVERRIDE:-$ROOT_DEFAULT}"
cd "$ROOT"

PRODUCT_BRANCH="providers"
UPSTREAM_REMOTE="upstream"
ORIGIN_REMOTE="origin"
EXPECTED_UPSTREAM_REPO="xai-org/grok-build"
EXPECTED_ORIGIN_REPO="ImL1s/medley"
VALIDATE_CMD="${SYNC_UPSTREAM_VALIDATE_CMD:-:}"
INTERRUPT_AFTER_PHASE="${SYNC_UPSTREAM_TEST_INTERRUPT_AFTER_PHASE:-}"

CHECK_ONLY=0
DRY_RUN=0
JSON_MODE=0
NO_PUSH=0
OPEN_PR=0
ABORT_RUN=0
RESUME_TARGET=""
REPO_OVERRIDE=""
PHASE="inspect"

STATE_DIR="$(git rev-parse --git-dir)/sync-upstream-state"
STATE_FILE=""
LOCK_DIR="$(git rev-parse --git-dir)/sync-upstream.lock"
LOCK_HELD=0

SYNC_BRANCH=""
ORIGINAL_BRANCH=""
ORIGINAL_HEAD=""
UPSTREAM_MAIN_SHA=""
MAIN_TARGET_SHA=""
PUSH_MAIN_DONE=0
PUSH_SYNC_DONE=0
REPO_SLUG=""

usage() {
  cat <<'EOF'
Usage: ./scripts/sync-upstream.sh [options]

Options:
  --check                 Report drift only (no mutations)
  --dry-run               Print the full plan (no mutations)
  --resume <state|branch> Resume an interrupted run using a state file or sync branch
  --abort                 Abort a resumed run and remove its state file
  --json                  Emit final status as JSON
  --no-push               Skip remote updates (prepare/validate only)
  --open-pr               Create PR with gh when available
  --repo <owner/repo>     Override PR repository slug (default: parsed from origin)
  --validate-cmd <cmd>    Validation command before push (default: SYNC_UPSTREAM_VALIDATE_CMD or ':')
  -h, --help              Show this help
EOF
}

json_escape() {
  local value="$1"
  value="${value//\\/\\\\}"
  value="${value//\"/\\\"}"
  value="${value//$'\n'/\\n}"
  value="${value//$'\r'/\\r}"
  value="${value//$'\t'/\\t}"
  printf '%s' "$value"
}

log() {
  if [[ "$JSON_MODE" -eq 0 ]]; then
    printf '%s\n' "$*"
  fi
}

emit_result() {
  local ok="$1"
  local phase_name="$2"
  local message="$3"
  local next_action="$4"
  if [[ "$JSON_MODE" -eq 1 ]]; then
    printf '{'
    printf '"ok":%s' "$ok"
    printf ',"phase":"%s"' "$(json_escape "$phase_name")"
    printf ',"message":"%s"' "$(json_escape "$message")"
    printf ',"next_action":"%s"' "$(json_escape "$next_action")"
    printf ',"state_file":"%s"' "$(json_escape "$STATE_FILE")"
    printf ',"sync_branch":"%s"' "$(json_escape "$SYNC_BRANCH")"
    printf ',"main_target":"%s"' "$(json_escape "$MAIN_TARGET_SHA")"
    printf '}\n'
    return 0
  fi

  if [[ "$ok" == "true" ]]; then
    printf '%s\n' "$message"
    if [[ -n "$next_action" ]]; then
      printf 'next: %s\n' "$next_action"
    fi
    if [[ -n "$STATE_FILE" ]]; then
      printf 'state: %s\n' "$STATE_FILE"
    fi
    return 0
  fi

  printf 'error: [%s] %s\n' "$phase_name" "$message" >&2
  if [[ -n "$next_action" ]]; then
    printf 'next: %s\n' "$next_action" >&2
  fi
  if [[ -n "$STATE_FILE" ]]; then
    printf 'resume: %s\n' "$STATE_FILE" >&2
  fi
}

phase_error() {
  local message="$1"
  local next_action="$2"
  emit_result false "$PHASE" "$message" "$next_action"
  exit 1
}

tracked_tree_is_clean() {
  git diff --quiet && git diff --cached --quiet
}

has_unmerged_paths() {
  git ls-files -u | grep -q .
}

git_path() {
  git rev-parse --git-path "$1"
}

has_in_progress_operation() {
  [[ -f "$(git_path MERGE_HEAD)" ]] ||
    [[ -f "$(git_path CHERRY_PICK_HEAD)" ]] ||
    [[ -f "$(git_path REVERT_HEAD)" ]] ||
    [[ -f "$(git_path REBASE_HEAD)" ]] ||
    [[ -d "$(git_path rebase-apply)" ]] ||
    [[ -d "$(git_path rebase-merge)" ]] ||
    [[ -f "$(git_path BISECT_LOG)" ]]
}

current_branch() {
  git symbolic-ref --short -q HEAD || true
}

safe_to_restore() {
  if has_in_progress_operation; then
    return 1
  fi
  tracked_tree_is_clean
}

restore_original_context() {
  local now
  now="$(current_branch)"
  if [[ -n "$ORIGINAL_BRANCH" ]]; then
    if [[ "$now" != "$ORIGINAL_BRANCH" ]]; then
      git checkout "$ORIGINAL_BRANCH" >/dev/null 2>&1 || return 1
    fi
    return 0
  fi
  if [[ -n "$ORIGINAL_HEAD" && "$(git rev-parse HEAD)" != "$ORIGINAL_HEAD" ]]; then
    git checkout --detach "$ORIGINAL_HEAD" >/dev/null 2>&1 || return 1
  fi
}

release_lock() {
  if [[ "$LOCK_HELD" -eq 1 ]]; then
    rm -rf "$LOCK_DIR"
    LOCK_HELD=0
  fi
}

cleanup_on_exit() {
  local status="$?"
  if [[ "$status" -ne 0 && "$CHECK_ONLY" -eq 0 && "$DRY_RUN" -eq 0 ]]; then
    if safe_to_restore; then
      restore_original_context || true
    fi
  fi
  release_lock
}
trap cleanup_on_exit EXIT

write_state() {
  if [[ -z "$STATE_FILE" ]]; then
    return 0
  fi
  mkdir -p "$(dirname "$STATE_FILE")"
  local tmp="${STATE_FILE}.tmp.$$"
  {
    echo "VERSION=1"
    echo "PHASE=${PHASE}"
    echo "PRODUCT_BRANCH=${PRODUCT_BRANCH}"
    echo "SYNC_BRANCH=${SYNC_BRANCH}"
    echo "ORIGINAL_BRANCH=${ORIGINAL_BRANCH}"
    echo "ORIGINAL_HEAD=${ORIGINAL_HEAD}"
    echo "UPSTREAM_MAIN_SHA=${UPSTREAM_MAIN_SHA}"
    echo "MAIN_TARGET_SHA=${MAIN_TARGET_SHA}"
    echo "PUSH_MAIN_DONE=${PUSH_MAIN_DONE}"
    echo "PUSH_SYNC_DONE=${PUSH_SYNC_DONE}"
    echo "REPO_SLUG=${REPO_SLUG}"
    echo "UPDATED_AT=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  } >"$tmp"
  mv "$tmp" "$STATE_FILE"
}

load_state_file() {
  local path="$1"
  [[ -f "$path" ]] || phase_error "state file '${path}' does not exist" "pass --resume with an existing state file or sync branch"
  STATE_FILE="$path"
  while IFS='=' read -r key value; do
    case "$key" in
    PHASE) PHASE="$value" ;;
    PRODUCT_BRANCH) PRODUCT_BRANCH="$value" ;;
    SYNC_BRANCH) SYNC_BRANCH="$value" ;;
    ORIGINAL_BRANCH) ORIGINAL_BRANCH="$value" ;;
    ORIGINAL_HEAD) ORIGINAL_HEAD="$value" ;;
    UPSTREAM_MAIN_SHA) UPSTREAM_MAIN_SHA="$value" ;;
    MAIN_TARGET_SHA) MAIN_TARGET_SHA="$value" ;;
    PUSH_MAIN_DONE) PUSH_MAIN_DONE="$value" ;;
    PUSH_SYNC_DONE) PUSH_SYNC_DONE="$value" ;;
    REPO_SLUG) REPO_SLUG="$value" ;;
    esac
  done <"$path"
  [[ -n "$PHASE" ]] || phase_error "state file '${path}' is invalid (missing phase)" "start a new run without --resume"
}

state_file_for_branch() {
  local branch="$1"
  local candidate
  [[ -d "$STATE_DIR" ]] || return 1
  for candidate in "$STATE_DIR"/*.state; do
    [[ -e "$candidate" ]] || continue
    if grep -q "^SYNC_BRANCH=${branch}$" "$candidate"; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done
  return 1
}

resolve_resume_target() {
  local target="$1"
  if [[ -f "$target" ]]; then
    printf '%s\n' "$target"
    return 0
  fi
  if [[ -f "${STATE_DIR}/${target}.state" ]]; then
    printf '%s\n' "${STATE_DIR}/${target}.state"
    return 0
  fi
  if git show-ref --verify --quiet "refs/heads/${target}"; then
    state_file_for_branch "$target"
    return $?
  fi
  return 1
}

acquire_lock() {
  if mkdir "$LOCK_DIR" >/dev/null 2>&1; then
    printf '%s\n' "$$" >"${LOCK_DIR}/pid"
    LOCK_HELD=1
    return 0
  fi

  local pid=''
  if [[ -f "${LOCK_DIR}/pid" ]]; then
    pid="$(cat "${LOCK_DIR}/pid" 2>/dev/null || true)"
  fi
  if [[ -n "$pid" ]] && ! kill -0 "$pid" >/dev/null 2>&1; then
    rm -rf "$LOCK_DIR"
    mkdir "$LOCK_DIR"
    printf '%s\n' "$$" >"${LOCK_DIR}/pid"
    LOCK_HELD=1
    return 0
  fi

  phase_error "another sync run appears active (lock: ${LOCK_DIR})" "wait for the active run to finish or remove a stale lock after confirming no run is active"
}

require_remote() {
  local name="$1"
  git remote get-url "$name" >/dev/null 2>&1 || phase_error "missing remote '${name}'" "configure '${name}' and rerun"
}

owner_repo_from_url() {
  local url="$1"
  local path owner repo
  url="${url%%\?*}"
  url="${url%%#*}"
  if [[ "$url" == git@*:* ]]; then
    path="${url#*:}"
  elif [[ "$url" == *://* ]]; then
    local without_scheme="${url#*://}"
    [[ "$without_scheme" == */* ]] || return 1
    path="${without_scheme#*/}"
  else
    path="$url"
  fi
  path="${path#/}"
  path="${path%/}"
  path="${path%.git}"
  [[ "$path" == */* ]] || return 1
  repo="${path##*/}"
  owner="${path%/*}"
  owner="${owner##*/}"
  [[ -n "$owner" && -n "$repo" ]] || return 1
  printf '%s/%s\n' "$owner" "$repo"
}

validate_remote_repo() {
  local name="$1"
  local expected="$2"
  local url actual
  url="$(git remote get-url "$name")"
  actual="$(owner_repo_from_url "$url" || true)"
  if [[ -z "$actual" ]]; then
    phase_error "remote '${name}' URL '${url}' cannot be parsed as owner/repo" "set '${name}' to a canonical repo URL and rerun"
  fi
  if [[ "$actual" != "$expected" ]]; then
    phase_error "remote '${name}' resolves to '${actual}' (expected '${expected}')" "fix '${name}' remote URL before syncing"
  fi
}

probe_origin_push_permissions() {
  local output status
  output="$(mktemp)"
  set +e
  git push --dry-run "$ORIGIN_REMOTE" "HEAD:refs/heads/${PRODUCT_BRANCH}" >"$output" 2>&1
  status=$?
  set -e
  if [[ "$status" -ne 0 ]] && grep -Eqi 'permission denied|access denied|not authorized|authentication failed|403' "$output"; then
    local msg
    msg="$(cat "$output")"
    rm -f "$output"
    phase_error "cannot push to '${ORIGIN_REMOTE}' (${msg})" "verify push permission and credentials, then rerun"
  fi
  rm -f "$output"
}

detect_untracked_collision_for_ref() {
  local ref="$1"
  local path
  while IFS= read -r path; do
    [[ -z "$path" ]] && continue
    if git cat-file -e "${ref}:${path}" >/dev/null 2>&1; then
      phase_error "untracked path '${path}' would be overwritten by checkout of '${ref}'" "move/remove the untracked path and rerun"
    fi
  done < <(git ls-files --others --exclude-standard)
}

detect_untracked_collisions() {
  if git show-ref --verify --quiet refs/heads/main; then
    detect_untracked_collision_for_ref main
  fi
  if git show-ref --verify --quiet "refs/heads/${PRODUCT_BRANCH}"; then
    detect_untracked_collision_for_ref "${PRODUCT_BRANCH}"
  fi
}

detect_stale_sync_branches() {
  local stale=0
  local branch
  while IFS= read -r branch; do
    [[ -z "$branch" ]] && continue
    if ! state_file_for_branch "$branch" >/dev/null 2>&1; then
      stale=$((stale + 1))
    fi
  done < <(git for-each-ref --format='%(refname:short)' 'refs/heads/sync/upstream-*')
  if [[ "$stale" -gt 0 ]]; then
    log "==> Found ${stale} local sync/upstream-* branches without state files (stale candidates)"
  fi
}

align_local_providers_with_origin() {
  if ! git show-ref --verify --quiet refs/remotes/origin/providers; then
    return 0
  fi

  local origin_tip local_tip
  origin_tip="$(git rev-parse origin/providers)"
  if ! git show-ref --verify --quiet refs/heads/providers; then
    return 0
  fi

  local_tip="$(git rev-parse providers)"
  if [[ "$local_tip" == "$origin_tip" ]]; then
    log "==> Local providers matches origin/providers ($(git rev-parse --short providers))"
    return 0
  fi

  if git merge-base --is-ancestor "$local_tip" "$origin_tip"; then
    log "==> Fast-forwarding local providers from origin/providers"
    git checkout providers >/dev/null
    git merge --ff-only origin/providers >/dev/null
    return 0
  fi

  phase_error \
    "local providers diverged from origin/providers (local=$(git rev-parse --short providers) origin=$(git rev-parse --short origin/providers))" \
    "resolve providers divergence before syncing"
}

advance_phase() {
  local completed="$PHASE"
  PHASE="$1"
  write_state
  if [[ -n "$INTERRUPT_AFTER_PHASE" && "$INTERRUPT_AFTER_PHASE" == "$completed" ]]; then
    emit_result false "$completed" "interrupted after phase '${completed}' (test hook)" "resume with --resume '${STATE_FILE}'"
    exit 130
  fi
}

ensure_local_product_branch() {
  if git show-ref --verify --quiet "refs/heads/${PRODUCT_BRANCH}"; then
    return 0
  fi
  if git show-ref --verify --quiet "refs/remotes/origin/${PRODUCT_BRANCH}"; then
    log "==> Creating local ${PRODUCT_BRANCH} from origin/${PRODUCT_BRANCH}"
    git branch "${PRODUCT_BRANCH}" "origin/${PRODUCT_BRANCH}" >/dev/null
    return 0
  fi
  log "==> ${PRODUCT_BRANCH} missing; creating from current HEAD ($(git rev-parse --short HEAD))"
  git branch "${PRODUCT_BRANCH}" HEAD >/dev/null
}

pick_sync_branch_name() {
  if [[ -n "$SYNC_BRANCH" ]]; then
    return 0
  fi
  local date short base candidate seq
  date="$(date +%Y%m%d)"
  short="$(git rev-parse --short "$MAIN_TARGET_SHA")"
  base="sync/upstream-${date}-${short}"
  candidate="$base"
  seq=2
  while git show-ref --verify --quiet "refs/heads/${candidate}" ||
    git show-ref --verify --quiet "refs/remotes/origin/${candidate}"; do
    candidate="${base}-${seq}"
    seq=$((seq + 1))
  done
  SYNC_BRANCH="$candidate"
  write_state
}

ensure_sync_branch_checked_out() {
  if git show-ref --verify --quiet "refs/heads/${SYNC_BRANCH}"; then
    git checkout "${SYNC_BRANCH}" >/dev/null
    return 0
  fi
  if git show-ref --verify --quiet "refs/remotes/origin/${SYNC_BRANCH}"; then
    git branch "${SYNC_BRANCH}" "origin/${SYNC_BRANCH}" >/dev/null
    git checkout "${SYNC_BRANCH}" >/dev/null
    return 0
  fi
  git checkout -b "${SYNC_BRANCH}" "${PRODUCT_BRANCH}" >/dev/null
}

derive_repo_slug() {
  if [[ -n "$REPO_OVERRIDE" ]]; then
    REPO_SLUG="$REPO_OVERRIDE"
    return 0
  fi
  local origin_url parsed
  origin_url="$(git remote get-url "$ORIGIN_REMOTE")"
  parsed="$(owner_repo_from_url "$origin_url" || true)"
  if [[ -z "$parsed" ]]; then
    phase_error "cannot derive origin repository slug from '${origin_url}'" "pass --repo <owner/repo> explicitly"
  fi
  REPO_SLUG="$parsed"
}

run_phase_inspect() {
  log "==> Phase: inspect/preflight"

  local branch
  branch="$(current_branch)"
  [[ -n "$branch" ]] || phase_error "detached HEAD is not supported for sync runs" "checkout a branch and rerun"
  if [[ -z "$ORIGINAL_BRANCH" ]]; then
    ORIGINAL_BRANCH="$branch"
  fi
  if [[ -z "$ORIGINAL_HEAD" ]]; then
    ORIGINAL_HEAD="$(git rev-parse HEAD)"
  fi

  if ! tracked_tree_is_clean; then
    phase_error "tracked working tree is dirty; sync refuses to overwrite user work" "commit or stash tracked changes, then rerun"
  fi
  if has_in_progress_operation; then
    phase_error "repository has an in-progress git operation (merge/rebase/cherry-pick/revert)" "finish or abort the in-progress operation, then rerun"
  fi

  require_remote "$UPSTREAM_REMOTE"
  require_remote "$ORIGIN_REMOTE"
  validate_remote_repo "$UPSTREAM_REMOTE" "$EXPECTED_UPSTREAM_REPO"
  validate_remote_repo "$ORIGIN_REMOTE" "$EXPECTED_ORIGIN_REPO"

  detect_untracked_collisions
  detect_stale_sync_branches
  probe_origin_push_permissions

  advance_phase fetch_verify
}

run_phase_fetch_verify() {
  log "==> Phase: fetch/verify"
  git fetch "$UPSTREAM_REMOTE"
  git fetch "$ORIGIN_REMOTE"
  UPSTREAM_MAIN_SHA="$(git rev-parse "${UPSTREAM_REMOTE}/main" 2>/dev/null || true)"
  [[ -n "$UPSTREAM_MAIN_SHA" ]] || phase_error "missing '${UPSTREAM_REMOTE}/main' after fetch" "verify upstream remote heads and rerun"
  MAIN_TARGET_SHA="$UPSTREAM_MAIN_SHA"
  align_local_providers_with_origin
  advance_phase prepare_mirror
}

run_phase_prepare_mirror() {
  log "==> Phase: prepare mirror update"
  git show-ref --verify --quiet refs/heads/main || phase_error "local branch 'main' is missing" "create local main tracking upstream/main, then rerun"
  git checkout main >/dev/null
  local main_now
  main_now="$(git rev-parse main)"
  if [[ "$main_now" != "$MAIN_TARGET_SHA" ]]; then
    if ! git merge --ff-only "${UPSTREAM_REMOTE}/main" >/dev/null; then
      phase_error "could not fast-forward main from ${UPSTREAM_REMOTE}/main" "repair main divergence manually (never force), then rerun --resume"
    fi
  fi
  MAIN_TARGET_SHA="$(git rev-parse main)"
  advance_phase prepare_product_merge
}

run_phase_prepare_product_merge() {
  log "==> Phase: prepare product merge"
  ensure_local_product_branch
  pick_sync_branch_name
  ensure_sync_branch_checked_out

  if has_unmerged_paths; then
    phase_error "sync branch has unresolved conflicts" "resolve conflicts on '${SYNC_BRANCH}', commit, then rerun --resume"
  fi

  if git merge-base --is-ancestor main "${SYNC_BRANCH}"; then
    log "==> ${SYNC_BRANCH} already contains main"
    advance_phase validate
    return 0
  fi

  set +e
  git merge --no-edit main
  local merge_status="$?"
  set -e
  if [[ "$merge_status" -ne 0 ]]; then
    phase_error \
      "merge conflict while merging main into ${SYNC_BRANCH}" \
      "resolve conflicts (see FORK.md watchlist), commit, then rerun --resume"
  fi

  advance_phase validate
}

run_phase_validate() {
  log "==> Phase: validate"
  git checkout "${SYNC_BRANCH}" >/dev/null
  if ! git merge-base --is-ancestor main HEAD; then
    phase_error "sync branch does not contain local main after merge" "inspect branch history and rerun --resume"
  fi

  if [[ -n "$VALIDATE_CMD" && "$VALIDATE_CMD" != ":" ]]; then
    log "==> Running validation command: ${VALIDATE_CMD}"
    set +e
    bash -lc "$VALIDATE_CMD"
    local validation_status="$?"
    set -e
    if [[ "$validation_status" -ne 0 ]]; then
      phase_error "validation command failed with exit ${validation_status}" "fix validation failures and rerun --resume"
    fi
  else
    log "==> Validation command not configured (set SYNC_UPSTREAM_VALIDATE_CMD or --validate-cmd)"
  fi

  if ! tracked_tree_is_clean; then
    phase_error "validation left tracked changes in working tree" "clean validation side effects and rerun --resume"
  fi

  advance_phase push
}

push_ref_or_fail() {
  local refspec="$1"
  local label="$2"
  set +e
  git push "$ORIGIN_REMOTE" "$refspec"
  local status="$?"
  set -e
  if [[ "$status" -ne 0 ]]; then
    phase_error "push failed for ${label}" "inspect remote rejection, then rerun --resume"
  fi
}

run_phase_push() {
  log "==> Phase: push"
  if [[ "$NO_PUSH" -eq 1 ]]; then
    log "==> --no-push enabled; skipping remote updates"
    advance_phase open_pr
    return 0
  fi

  if git show-ref --verify --quiet refs/remotes/origin/main; then
    local origin_main local_main
    origin_main="$(git rev-parse origin/main)"
    local_main="$(git rev-parse main)"
    if ! git merge-base --is-ancestor "$origin_main" "$local_main"; then
      phase_error "local main cannot fast-forward origin/main" "git fetch origin and reconcile main before rerunning --resume"
    fi
  fi

  local sync_ref="refs/heads/${SYNC_BRANCH}:refs/heads/${SYNC_BRANCH}"
  local main_ref="refs/heads/main:refs/heads/main"
  local output status
  output="$(mktemp)"

  if [[ "$PUSH_MAIN_DONE" -eq 0 && "$PUSH_SYNC_DONE" -eq 0 ]]; then
    set +e
    git push --atomic "$ORIGIN_REMOTE" "$main_ref" "$sync_ref" >"$output" 2>&1
    status="$?"
    set -e
    if [[ "$status" -eq 0 ]]; then
      PUSH_MAIN_DONE=1
      PUSH_SYNC_DONE=1
      write_state
      rm -f "$output"
      advance_phase open_pr
      return 0
    fi
    if ! grep -Eqi 'atomic push|does not support --atomic|atomic pushes are not supported' "$output"; then
      local msg
      msg="$(cat "$output")"
      rm -f "$output"
      phase_error "atomic push failed (${msg})" "inspect remote rejection, then rerun --resume"
    fi
    log "==> Remote does not support --atomic; using explicit non-force ref pushes"
  fi
  rm -f "$output"

  if [[ "$PUSH_SYNC_DONE" -eq 0 ]]; then
    push_ref_or_fail "$sync_ref" "${SYNC_BRANCH}"
    PUSH_SYNC_DONE=1
    write_state
  fi
  if [[ "$PUSH_MAIN_DONE" -eq 0 ]]; then
    push_ref_or_fail "$main_ref" "main"
    PUSH_MAIN_DONE=1
    write_state
  fi

  advance_phase open_pr
}

run_phase_open_pr() {
  log "==> Phase: open/print PR"
  derive_repo_slug
  write_state

  local date short title
  date="$(date +%Y%m%d)"
  short="$(git rev-parse --short main)"
  title="chore: sync upstream ${date} (${short})"

  if [[ "$OPEN_PR" -eq 1 && "$NO_PUSH" -eq 0 && -n "$REPO_SLUG" ]] &&
    command -v gh >/dev/null 2>&1 &&
    gh auth status >/dev/null 2>&1; then
    gh pr create --repo "$REPO_SLUG" --base "$PRODUCT_BRANCH" --head "$SYNC_BRANCH" --title "$title" --body "$(cat <<PRBODY
## Upstream sync
- Mirror: \`main\` fast-forward from upstream
- Strategy: merge into \`${PRODUCT_BRANCH}\`

## Hotspot review
- [ ] sampler AuthScheme / client
- [ ] shell config / credentials / auth_method
- [ ] pager /model + picker

## Verify
- [ ] multi-provider auth
- [ ] local auth_scheme=none
PRBODY
)"
  fi

  if [[ "$JSON_MODE" -eq 0 ]]; then
    echo
    echo "==> Sync branch ready: ${SYNC_BRANCH}"
    echo "    main tip:      $(git rev-parse --short main)"
    echo "    providers tip: $(git rev-parse --short "${PRODUCT_BRANCH}")"
    echo "    sync tip:      $(git rev-parse --short HEAD)"
    echo
    echo "Next steps:"
    echo "  1. Review hotspot diffs: git log --oneline ${PRODUCT_BRANCH}..main -- \\"
    echo "       'crates/codegen/xai-grok-sampler/**' \\"
    echo "       'crates/codegen/xai-grok-shell/src/agent/**' \\"
    echo "       'crates/codegen/xai-grok-pager/**'"
    echo "  2. Run auth/config smoke tests (sampler none_scheme_ / shell auth_method)."
    if [[ "$NO_PUSH" -eq 1 ]]; then
      echo "  3. Push explicit refs (never force):"
      echo "       git push --atomic origin refs/heads/main refs/heads/${SYNC_BRANCH}"
    fi
    echo "  4. Open a PR into ${PRODUCT_BRANCH}:"
    echo "       gh pr create --repo ${REPO_SLUG} --base ${PRODUCT_BRANCH} --head ${SYNC_BRANCH}"
    echo
    echo "Done. Do not force-push main."
  fi

  advance_phase complete
}

compare_refs() {
  local local_ref="$1"
  local remote_ref="$2"
  if [[ -z "$local_ref" || -z "$remote_ref" ]]; then
    printf 'missing\n'
    return 0
  fi
  if [[ "$local_ref" == "$remote_ref" ]]; then
    printf 'equal\n'
    return 0
  fi
  if git merge-base --is-ancestor "$local_ref" "$remote_ref"; then
    printf 'behind\n'
    return 0
  fi
  if git merge-base --is-ancestor "$remote_ref" "$local_ref"; then
    printf 'ahead\n'
    return 0
  fi
  printf 'diverged\n'
}

remote_head_sha() {
  local remote="$1"
  local branch="$2"
  local line
  line="$(git ls-remote --exit-code "$remote" "refs/heads/${branch}" 2>/dev/null || true)"
  if [[ -z "$line" ]]; then
    printf '\n'
    return 0
  fi
  printf '%s\n' "${line%%[[:space:]]*}"
}

plan_sync_branch_name_readonly() {
  local upstream_sha="$1"
  local date short base candidate seq
  date="$(date +%Y%m%d)"
  short="$(git rev-parse --short "$upstream_sha")"
  base="sync/upstream-${date}-${short}"
  candidate="$base"
  seq=2
  while git show-ref --verify --quiet "refs/heads/${candidate}" ||
    [[ -n "$(git ls-remote "$ORIGIN_REMOTE" "refs/heads/${candidate}")" ]]; do
    candidate="${base}-${seq}"
    seq=$((seq + 1))
  done
  printf '%s\n' "$candidate"
}

run_check_or_dry_run() {
  PHASE="inspect"
  local branch
  branch="$(current_branch)"
  [[ -n "$branch" ]] || phase_error "detached HEAD is not supported for check/dry-run" "checkout a branch and rerun"
  require_remote "$UPSTREAM_REMOTE"
  require_remote "$ORIGIN_REMOTE"
  validate_remote_repo "$UPSTREAM_REMOTE" "$EXPECTED_UPSTREAM_REPO"
  validate_remote_repo "$ORIGIN_REMOTE" "$EXPECTED_ORIGIN_REPO"

  local local_main local_providers remote_upstream_main remote_origin_main remote_origin_providers
  local_main="$(git rev-parse main 2>/dev/null || true)"
  local_providers="$(git rev-parse "${PRODUCT_BRANCH}" 2>/dev/null || true)"
  remote_upstream_main="$(remote_head_sha "$UPSTREAM_REMOTE" main)"
  remote_origin_main="$(remote_head_sha "$ORIGIN_REMOTE" main)"
  remote_origin_providers="$(remote_head_sha "$ORIGIN_REMOTE" "${PRODUCT_BRANCH}")"
  [[ -n "$remote_upstream_main" ]] || phase_error "could not read ${UPSTREAM_REMOTE}/main" "verify remote access and rerun"

  local main_status providers_status drift planned_branch
  main_status="$(compare_refs "$local_main" "$remote_upstream_main")"
  providers_status="$(compare_refs "$local_providers" "$remote_origin_providers")"
  planned_branch="$(plan_sync_branch_name_readonly "$remote_upstream_main")"
  drift=0
  [[ "$main_status" == "equal" ]] || drift=1

  if [[ "$JSON_MODE" -eq 1 ]]; then
    printf '{'
    printf '"ok":true'
    printf ',"phase":"%s"' "$([[ "$CHECK_ONLY" -eq 1 ]] && echo check || echo dry_run)"
    printf ',"main_status":"%s"' "$main_status"
    printf ',"providers_status":"%s"' "$providers_status"
    printf ',"planned_sync_branch":"%s"' "$planned_branch"
    printf ',"upstream_main":"%s"' "$remote_upstream_main"
    printf ',"origin_main":"%s"' "$remote_origin_main"
    printf ',"drift":%s' "$([[ "$drift" -eq 1 ]] && echo true || echo false)"
    printf ',"next_action":"%s"' "$([[ "$drift" -eq 1 ]] && echo "run sync to prepare branch" || echo "no drift; nothing to sync")"
    printf '}\n'
  else
    if [[ "$CHECK_ONLY" -eq 1 ]]; then
      echo "==> check"
      echo "    main vs upstream/main: ${main_status}"
      echo "    providers vs origin/providers: ${providers_status}"
      echo "    planned sync branch: ${planned_branch}"
      if [[ "$drift" -eq 1 ]]; then
        echo "drift detected: upstream/main differs from local main"
      else
        echo "no drift detected"
      fi
    else
      echo "==> dry-run"
      echo "    phase inspect/preflight: would validate remotes, cleanliness, and operation state"
      echo "    phase fetch/verify: would fetch ${UPSTREAM_REMOTE} and ${ORIGIN_REMOTE}"
      echo "    phase prepare mirror update: would fast-forward main to ${remote_upstream_main}"
      echo "    phase prepare product merge: would use branch ${planned_branch}"
      echo "    phase validate: would run '${VALIDATE_CMD}'"
      if [[ "$NO_PUSH" -eq 1 ]]; then
        echo "    phase push: skipped (--no-push)"
      else
        echo "    phase push: would push refs/heads/main and refs/heads/${planned_branch} (atomic when supported)"
      fi
      echo "    phase open/print PR: would target ${PRODUCT_BRANCH}"
    fi
  fi

  if [[ "$CHECK_ONLY" -eq 1 && "$drift" -eq 1 ]]; then
    return 2
  fi
  return 0
}

run_abort() {
  log "==> Aborting resume state"
  if has_in_progress_operation || ! tracked_tree_is_clean; then
    phase_error "cannot abort while repository has in-progress operations or tracked changes" "finish cleanup, then rerun --resume <state> --abort"
  fi
  if [[ -n "$ORIGINAL_BRANCH" ]]; then
    git checkout "$ORIGINAL_BRANCH" >/dev/null
  fi
  rm -f "$STATE_FILE"
  emit_result true "abort" "aborted state '${STATE_FILE}'" "sync branch left in place; delete manually when no longer needed"
}

parse_args() {
  while [[ "$#" -gt 0 ]]; do
    case "$1" in
    --check) CHECK_ONLY=1 ;;
    --dry-run) DRY_RUN=1 ;;
    --resume)
      [[ "$#" -ge 2 ]] || phase_error "--resume requires an argument" "pass a state file or sync branch"
      RESUME_TARGET="$2"
      shift
      ;;
    --abort) ABORT_RUN=1 ;;
    --json) JSON_MODE=1 ;;
    --no-push) NO_PUSH=1 ;;
    --open-pr) OPEN_PR=1 ;;
    --repo)
      [[ "$#" -ge 2 ]] || phase_error "--repo requires owner/repo" "pass a repo slug"
      REPO_OVERRIDE="$2"
      shift
      ;;
    --validate-cmd)
      [[ "$#" -ge 2 ]] || phase_error "--validate-cmd requires a command string" "pass a command string"
      VALIDATE_CMD="$2"
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      phase_error "unknown option '$1'" "run with --help for supported options"
      ;;
    esac
    shift
  done

  if [[ "$CHECK_ONLY" -eq 1 && "$DRY_RUN" -eq 1 ]]; then
    phase_error "--check and --dry-run are mutually exclusive" "run one mode at a time"
  fi
  if [[ -n "$RESUME_TARGET" ]] && [[ "$CHECK_ONLY" -eq 1 || "$DRY_RUN" -eq 1 ]]; then
    phase_error "--resume cannot be combined with --check/--dry-run" "resume in mutation mode or run check/dry-run without resume"
  fi
  if [[ "$ABORT_RUN" -eq 1 && -z "$RESUME_TARGET" ]]; then
    phase_error "--abort requires --resume <state|branch>" "pass a resume target to abort"
  fi
}

init_state_for_new_run() {
  mkdir -p "$STATE_DIR"
  local stamp
  stamp="$(date -u +%Y%m%dT%H%M%SZ)-$$"
  STATE_FILE="${STATE_DIR}/${stamp}.state"
  PHASE="inspect"
  ORIGINAL_BRANCH="$(current_branch)"
  ORIGINAL_HEAD="$(git rev-parse HEAD)"
  write_state
}

resume_state_or_die() {
  local resolved
  resolved="$(resolve_resume_target "$RESUME_TARGET" || true)"
  [[ -n "$resolved" ]] || phase_error "could not resolve resume target '${RESUME_TARGET}'" "pass a valid state file path or sync branch"
  load_state_file "$resolved"
}

run_phase_loop() {
  while true; do
    case "$PHASE" in
    inspect) run_phase_inspect ;;
    fetch_verify) run_phase_fetch_verify ;;
    prepare_mirror) run_phase_prepare_mirror ;;
    prepare_product_merge) run_phase_prepare_product_merge ;;
    validate) run_phase_validate ;;
    push) run_phase_push ;;
    open_pr) run_phase_open_pr ;;
    complete)
      emit_result true "complete" "sync finished successfully" "review and merge PR into ${PRODUCT_BRANCH}"
      return 0
      ;;
    *)
      phase_error "unknown phase '${PHASE}' in state file" "fix or delete state file, then start a new sync run"
      ;;
    esac
  done
}

main() {
  parse_args "$@"

  if [[ "$CHECK_ONLY" -eq 1 || "$DRY_RUN" -eq 1 ]]; then
    run_check_or_dry_run
    return "$?"
  fi

  if [[ -n "$RESUME_TARGET" ]]; then
    resume_state_or_die
  else
    init_state_for_new_run
  fi

  acquire_lock

  if [[ "$ABORT_RUN" -eq 1 ]]; then
    run_abort
    return 0
  fi

  run_phase_loop
}

main "$@"
