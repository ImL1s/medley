# Medley — ImL1s fork of xai-org/grok-build

This repository is **[ImL1s/medley](https://github.com/ImL1s/medley)**, published as **Medley**: a community fork of **[xai-org/grok-build](https://github.com/xai-org/grok-build)** with a long-lived multi-provider / local LLM product line. It is not affiliated with or endorsed by xAI — see [`NOTICE.md`](NOTICE.md) for the trademark and non-affiliation statement.

This file doubles as the Apache-2.0 §4(b) statement of modification: the "What's different from upstream" table below is the record of changes made to the upstream work.

Release archives and [`install.sh`](install.sh) ship the binary as `medley` and install it under `~/.medley/bin`; a source build keeps the upstream cargo bin target name `xai-grok-pager`, which upstream installs ship as `grok`. State resolves in this order: `$MEDLEY_HOME`, `$GROK_HOME`, `~/.medley` when it exists, an existing `~/.grok` (which the first interactive run offers to copy across), then `~/.medley`. The application's own `GROK_*` environment variables are not renamed — most have no `MEDLEY_*` equivalent and never will — but a documented, user-facing subset reads `MEDLEY_*` first with `GROK_*` as a permanent fallback; see [#426](https://github.com/ImL1s/medley/issues/426) for the enumerated set and `crates/codegen/xai-grok-config/src/env_alias.rs` for the mechanism.

Remotes:

| Remote     | URL                                         | Role                          |
|------------|---------------------------------------------|-------------------------------|
| `upstream` | `https://github.com/xai-org/grok-build.git` | Read-only upstream mirror     |
| `origin`   | `https://github.com/ImL1s/medley.git`   | Fork: PRs, releases, default  |

## Branch model

| Branch       | Role                                                                 |
|--------------|----------------------------------------------------------------------|
| `main`       | **Pristine upstream mirror.** Only fast-forward from `upstream/main`. Never land provider/custom commits here. |
| `providers`  | **Product line.** Multi-provider credentials, keyless local LLMs, fork docs/UX. Default branch for users and releases. |

### GitHub branch protection (recommended)

Configure these on **[ImL1s/medley](https://github.com/ImL1s/medley)** so fork workflow stays safe:

| Branch | Rules |
|--------|-------|
| `main` | No force-push. No product PRs — only fast-forward mirrors from `upstream/main` (via `scripts/sync-upstream.sh`). |
| `providers` | Require CI checks to pass before merge. Product features and sync PRs land here. |

Feature topic branches (e.g. `feat/…`) merge into `providers`, not into `main`.

## What's different from upstream

Relative to `xai-org/grok-build` `main`, this fork's `providers` branch adds:

| Area | Behavior |
|------|----------|
| `AuthScheme::None` | Sampler sends **no** `Authorization` / `x-api-key`. Required for keyless local servers (Ollama, LM Studio, vLLM, …). |
| `[model.*] auth_scheme` | Per-model override: `"bearer"` (default), `"x_api_key"` (Anthropic-style), or `"none"`. |
| Credential isolation | No-auth models never inherit ambient session / `XAI_API_KEY` / env credentials. Invalid `auth_scheme` is unready (fail-closed), not silent Bearer. |
| ACP `local.none` | Advertised only when the **startup-selected** model is no-auth. |
| Session safety | Catalog key vs wire slug kept separate; readiness gated in `model_switch::apply` (covers new/load/switch); unready defaults fall back to a bundled sentinel; turn-time `reconstruct_full_config` re-checks `ModelAuthFacts.ready` and strips ambient Bearer / identity for unready or `None` models. |
| TUI readiness | `/model` and Ctrl+M show `ready` / `missing` / `none` badges; hard-block unready; soft-confirm auth-class changes. |
| Optional native subagent route contract | Generic exact / ordered-candidate / receipt types plus `/agents` route-status text. Lives on `providers`. Not an upstream Grok Build claim. Spawn wiring and #18 fallback remain incomplete. See [`docs/architecture/native-subagent-route-contract.md`](docs/architecture/native-subagent-route-contract.md). |
| State directory | State resolves to `~/.medley` instead of upstream's `~/.grok`, honouring `MEDLEY_HOME` ahead of `GROK_HOME`. An existing `~/.grok` is still read, and an interactive run offers a one-time copy into `~/.medley`; declining writes a `.medley-keep-legacy` marker so the prompt does not repeat. |
| Session-ID locks | Per-session leases live in `$MEDLEY_HOME/.locks/session-ids` as owner-only (`0700`/`0600`) files opened through retained handles (`O_NOFOLLOW`). v0.2.119 shipped no lock protocol — drain every Medley process before upgrading; there is no rolling coexistence with already-running 0.2.119 processes, and no compatibility for unshipped PR #332 hex lock names. |
| Packaging | [`install.sh`](install.sh) and [`.github/workflows/release.yml`](.github/workflows/release.yml) publish the binary as `medley` with SHA-256 checksums and build provenance, installing a launcher that supplies the install location as the state directory unless the caller exported `MEDLEY_HOME` or `GROK_HOME`. Upstream's `x.ai/cli` installers are not used. |
| Fork ops | [`FORK.md`](FORK.md), [`scripts/sync-upstream.sh`](scripts/sync-upstream.sh), and providers-only [CI](.github/workflows/ci.yml). |

Provider setup examples (Anthropic, OpenAI, Ollama, …): [11-custom-models.md](crates/codegen/xai-grok-pager/docs/user-guide/11-custom-models.md).

## Weekly upstream sync

Do **not** force-push `main`. Prefer merge (not rebase) when integrating upstream into the published `providers` line.

1. Ensure a clean tracked working tree (untracked files such as local notes are OK).
2. Run:

   ```bash
   ./scripts/sync-upstream.sh
   ```

   The script:

   - Fetches `upstream` and `origin`
   - Fast-forward updates local `main` from `upstream/main` and pushes `origin main`
   - Creates `sync/upstream-YYYYMMDD` from `providers` (or creates `providers` from the current tip if missing)
   - Merges `main` into the sync branch
   - Prints next steps for opening a PR into `providers` (uses `gh` when available; does not require it)

3. Review the sync PR, resolve conflicts carefully on the watchlist, run auth/config smoke tests, then merge into `providers`.

Optional hygiene (local git config, not enforced by the script):

```bash
git config rerere.enabled true
git config merge.conflictstyle zdiff3
```

## Watchlist (auth / config / picker)

On every sync PR, review upstream diffs that touch:

- `crates/codegen/xai-grok-sampler/` — especially `AuthScheme`, `client.rs`, credential headers
- `crates/codegen/xai-grok-shell/src/agent/` — `ConfigModelOverride`, `resolve_credentials`, `auth_method.rs`
- `crates/codegen/xai-grok-shell/src/session/` — ACP session reconstruct / model switch
- `crates/codegen/xai-grok-shell/src/terminal/pty_session.rs` — #132: cancellable `poll(2)` reader (no `O_NONBLOCK`), hangup-before-cancel teardown, reader/writer on `std::thread` not `spawn_blocking`; tests give the child a clean `HOME` so Fig/iTerm bashrc cannot mask the hang. Bare `spawn_blocking` `read` wedged runtime drop when descendants outlived the session; Darwin `close`/`dup2` of the reader fd mid-`read` also hangs.
- `crates/codegen/xai-grok-tools/src/implementations/grok_build/task/coordinator/` — #271: nested spawns flatten lifecycle ownership to the root, but `spawn_parent_session_id` must be captured before reparenting and carried through `QueuedSpawn`; otherwise a dequeued grandchild inherits the root session's capabilities instead of its immediate spawner's ceiling.
- `crates/codegen/xai-grok-config/src/paths.rs` — #420: `grok_home()` now consults a fork-owned thread-local pin (`state_home::pinned_state_home()`) before its `OnceLock`. Four lines inside a function upstream owns, so a sync that rewrites the body will drop them silently: the cache still works, tests still compile, and every test that isolates its state directory quietly starts reading the developer's real one again. The pin is what makes `MEDLEY_HOME` / `GROK_HOME` guards mean anything after the first resolution; `state_home.rs` itself is fork-only and never conflicts.
- `crates/codegen/xai-grok-pager/` — `/model` slash command, model picker (`Ctrl+M`), `available_models` rendering
- Custom models docs: `crates/codegen/xai-grok-pager/docs/user-guide/11-custom-models.md`
- `crates/codegen/xai-fast-worktree/src/db/mod.rs` — #405: the fork owns the **body** of `resolve_grok_home()`, routing it through `xai_grok_config::state_dir::resolve_user()` so worktree checkouts and `worktrees.db` land in the same state directory as trust, hooks and everything else — including which of `~/.medley` and `~/.grok` is live. Keeping upstream's definition and adding a fork wrapper alongside it does **not** work, because upstream calls the function itself: in `eb267fef`, at `auto_gc.rs:592`, `auto_gc.rs:640` and `db/mod.rs:240`, and it is re-exported from `lib.rs:60`. Those internal call sites would keep resolving upstream's path no matter what a wrapper did. So this conflict is **irreducible** — a known, recurring cost, not a mistake to be fixed. `git config rerere.enabled true` (above) earns its keep here: the resolution is identical every sync — keep *ours* for the definition, then re-apply upstream's non-state-dir changes on top.
- `crates/codegen/xai-grok-update/src/auto_update.rs` — #405: conflicts every sync, and its hunks resolve **in three different directions**, so read each one before reaching for *ours*. Against `eb267fef` the file has five conflict hunks:

  **Three are keep-ours.** The fork's `dist_channel::self_update_refusal()` guards, inserted into upstream functions — `run_update_if_available`, `run_install_script`, `install_internal_from_bases` — so a medley build never runs the inherited updater. **Irreducible**: they live inside code upstream owns and keeps editing. Keep *ours*, re-apply upstream's changes on top. This is the resolution that is identical every sync, and the reason `rerere` (above) is worth enabling on this file.

  **One is take-upstream** — `reinstall_hint()`, below.

  **One is neither, and blindly keeping *ours* on it would be a real loss:** upstream has extracted the whole inline `mod tests { .. }` into `auto_update_tests.rs` (`#[path = "auto_update_tests.rs"] mod tests;`, two lines) while the fork still has ~2500 lines of tests inline. Git reports that as one enormous hunk. Take upstream's declaration and move the fork's own tests into the new file; keeping *ours* silently discards upstream's refactor **and** every upstream test that now lives in `auto_update_tests.rs`, which is a new file and so never appears as a conflict to warn you. Measured against `eb267fef`: 7 upstream-only tests would be lost that way, and 4 fork-only tests need porting the other way. Tracked with the name lists in #425. One-time, not recurring — and not a rebrand conflict at all, which is why it is worth naming here.

  The fifth hunk is `reinstall_hint()`, and there the rule is **take upstream**. The fork's override reads as load-bearing and is not. `reinstall_hint` has exactly one non-test caller, inside `run_install_script`, and an unconditional `self_update_refusal()` `bail!` returns before it; `decide()` yields `Allowed` only for `DistIdentity::Upstream`, which a stamped medley binary can never resolve to (*any stamp at all wins*). So the fork's medley branch is unreachable in production, and accepting upstream's definition — including its `(installer, channel)` signature change — cannot tell a fork user to install Grok. What actually shows a fork user the medley installer is `dist_channel::refusal_message()`, in a **fork-only file upstream does not have**, so that guarantee costs nothing at sync time. Two consequences worth knowing before you take it: the fork's `reinstall_hint_for` unit tests go with it (they exercise a state that call site cannot reach), and the check that could invalidate this rule is a grep — confirm every `reinstall_hint` call site is still dominated by a `self_update_refusal()` guard. If upstream ever adds one that is not, add the guard at that call site rather than re-forking the hint.

#405 measured three rebrand-caused conflicts against upstream `eb267fef`, and calling them all irreducible would have been wrong. One is gone: the alphabetised `pub use paths::{ .. }` block in `crates/codegen/xai-grok-config/src/lib.rs` conflicted only because the fork inserted `pin_grok_home` into it, so `pin_grok_home` now sits on its own re-export line and the block stays byte-identical to the merge base. A comment in that file says so; do not fold it back in. One is irreducible: `resolve_grok_home()`. The third is mixed, as the entry above sets out — the file keeps conflicting on the update guards no matter what is done with the hint.

Prefer keeping fork intent on auth hotspots (`AuthScheme::None`, `local.none`, no ambient xAI credential leak) rather than blindly taking upstream.

### Quarantined upstream features — take *ours*, and know what would change that

Two upstream features are deliberately switched off in a way that reads, at
conflict time, exactly like the fork having lost something. Both resolutions
are **keep ours**. Neither was written down before #485, and the 2026-08-04
sync survived them on memory rather than record.

The tell for both is a **bare block wrapping a whole function body**, or a
`check-cfg` entry naming a feature no `[features]` table declares — the residue
of a removed `#[cfg]`.

- **`local-workspace`** — upstream declares the feature in four `Cargo.toml`
  files (`xai-grok-pager`, `-pager-bin`, `xai-grok-shell`, `-shell-base`); the
  fork declares it in **none**, and adds
  `unexpected_cfgs = { level = "warn", check-cfg = ['cfg(feature, values("local-workspace"))'] }`
  at `Cargo.toml:399` so the compiler stays quiet about the **several hundred**
  now-permanently-false gates across the tree. Quarantined by `7b512227`.
  (Counting them is itself a trap: `cfg(feature = "local-workspace")` matches
  far fewer than the `all(..)` / `any(..)` forms that also enable on it, and a
  grep wide enough to catch those also catches `not(feature = ..)` gates, which
  are permanently *true*. Grep for the mechanism, not for a number.)
  **Why:** `gateway_bridge` — the module the gated code imports — exists in
  **neither** tree, so upstream's own Cargo build cannot enable it either.
  **At conflict time:** take ours; do not restore the declarations.
  **What would change it:** upstream shipping `gateway_bridge` in the public
  extraction.

- **chat-kind sessions** — `reject_chat_kind_without_feature`
  (`xai-grok-shell/src/agent/mvp_agent/mod.rs:433`) rejects every
  `kind: "chat"` request unconditionally, with no `cfg` variant anywhere. It is
  literally the first line of `new_session` (`acp_agent.rs:1110`). On the other
  two paths it is `?`-propagated one line *after* `begin_session_load`, which is
  itself fallible — `load_session` (`:1674`/`:1675`) and `attach_session`
  (`session_setup.rs:254`/`:255`) — so a chat request there can surface a
  load-claim error instead of the chat rejection. The gate is early on every
  path and nothing chat-specific runs ahead of it, but "first line" is true of
  `new_session` alone. `is_chat_kind` has exactly one non-test binding
  (`acp_agent.rs:1202`), after the gate.
  **Why:** this is a build-only binary; the chat product is grok.com's.
  **At conflict time:** take ours.
  **What would change it:** this fork wanting a chat-enabled binary.

Both mean several hundred lines of synced upstream code that **nothing
executes**. That is not harmless: #384 found `session_token_auth_gate`'s
production check *and its own guard test* reverted together by an auto-merge, in
a file that never appeared in the conflict list — nothing went red because the
test travelled with the code. Unexecuted code has strictly less protection than
that, so these two paths are where a sync can quietly take upstream's side
wholesale. Read their diffs on every sync even though no test will complain.

## Conflicts git does not report

A textual conflict is not the only kind, and it is not the expensive kind. On
PR #383 a merge produced a tree where **both halves were individually correct
and jointly wrong**, with no conflict markers, because the halves lived in
*different files*.

`d6d096ce` (#343, on the feature branch) stopped `resolved_auth_path` reading
`GROK_AUTH_PATH` under `cfg!(test)`, so in-process tests could not clobber each
other through a process-global, and moved its own copy of the fresh-process
regression onto the new `CodexAuthPathGuard`. Production still honours the env.
`providers` meanwhile carried a version of that same test which still did
`EnvGuard::set("GROK_AUTH_PATH", ..)` — the channel the other side had just
closed. The merge kept both (renaming one to avoid a name collision) and
reported no conflict. The `providers` variant then read `state_home/auth.json`,
which nothing wrote, and failed an hour into CI.

The generalisation, and the thing to check on every merge into `providers`:
**when one side changes how a value is *plumbed*, grep the other side for the
old channel by name.** Merge-base reasoning answers *who deleted this?*; it does
not answer *who is still calling it?* A closed channel usually has surviving
callers, and they compile.

In order, cheapest first:

- `git log --merge` cannot answer this, and it is worth knowing why before you
  reach for it. It requires an *unfinished* merge (`MERGE_HEAD`) and is scoped to
  the files git recorded as **conflicted** — of which a semantic conflict has
  none. Once the merge is committed it does not fall quiet, it fails:
  `fatal: --merge requires one of the pseudorefs MERGE_HEAD, CHERRY_PICK_HEAD,
  REVERT_HEAD or REBASE_HEAD`. Neither the empty listing nor the error is
  evidence of safety.
- After resolving, grep the whole tree for every env var, setter, or flag the
  incoming side removed or gated — including `#[cfg(test)]` code, which
  `cargo check --lib` and clippy on `--lib` never compile.
- `cargo test --workspace --no-run` compiles those targets without running them.
  That catches callers which stopped *compiling*. It cannot catch the ones that
  still compile and now read the wrong thing — only the grep does. **And it does
  not compile everything.** Two silent omissions, both measured (#474):
  - Targets whose `required-features` are unmet are skipped with no error and no
    warning. All six `[[test]]` targets in `xai-grok-shell` are gated on
    `test-support`; pass the feature, or they are not in the check.
  - `[[bench]]` targets are not built at all without `--benches`, feature or no
    feature. `cargo test -p xai-grok-shell --tests --benches --features
    test-support --no-run` yields 51 executables; drop `--benches` and three
    disappear from the output without comment.

  So `--workspace` is not the completeness guarantee its name suggests, and the
  failure mode is an empty space in a list nobody counts.

#409 later closed the other half of the same channel. `AuthManager::new` read
`GROK_AUTH_PATH` unconditionally, so the env was shut out of the Codex resolver
and still wired into the xAI one: setting it moved one manager, pinning
`CodexAuthPathGuard` moved the other, and nothing said so. Both resolvers now
follow one rule — thread-local pin, else `grok_home/auth.json` under
`cfg(test)`, else the env in production. The two seams stay *separate*
(`XaiAuthPathGuard` in `auth::manager`, `CodexAuthPathGuard` in
`auth::openai_codex`) because fixtures construct an unredirected manager of the
opposite kind on purpose; `codex_and_xai_auth_path_resolvers_agree_on_a_shared_home`
pins both and fails if either drifts back onto the env. Under `cfg(test)`,
`GROK_AUTH_PATH` no longer influences path resolution anywhere — it survives
only in the `AuthManager::new` telemetry line, which reports the raw env value
and does not feed resolution. Integration targets (`tests/*.rs`, which link the
lib built without `cfg(test)`) are what still exercise the production branch.

### The neighbouring failure that is *not* this

PR #383's other late CI failure looks identical from the outside and is not.
`d6d096ce` also taught `resolve_credentials` to drop an ambient xAI credential
bound for a non-first-party origin and migrated three sibling call sites,
missing a fourth — but that fourth site was in **its own branch**, broken from
the moment that commit landed. It stayed invisible because
`a_key_passed_as_session_key_is_relabelled_a_session_token` is named in no
filter in `ci.yml`, so the hot path never ran it; only a fuller run surfaced it.

Both failures cost an hour of CI and both were found late, so it is tempting to
file them together. Keep them apart, because the check that catches each one is
different: the first needs the grep above, the second needs a test to be
*enrolled* (#408). Attributing an unenrolled-test blindspot to the merge would
send the next reader looking for a conflict that was never there.

### The third shape: two copies of one user-facing string

The two above are about *plumbing*. The third is about *duplication*, and it is
the one a merge is most likely to produce silently.

`5f63802e` (#335/#346/#358/#359) centralised the changed-catalog toast on
`CATALOG_CHANGED_TOAST` and left a comment above the call site saying so:
"Single source of truth for the wording". `c113580a` (#332) independently added
`set_default_model_confirmed`, a second live path for the same
`!available_has_new` condition, carrying its own literal — and a test pinning
that literal. Neither side touched the other's lines, so the merge produced one
file with **two different messages for the same condition**, a comment claiming
single-sourcing that was now false, and a test pinning whichever wording its own
side had written.

CI reported it as a plain assertion mismatch, which is the cheap outcome. The
expensive part is that it survived at all: `set_default_model_confirmed` had **no
test covering its toast**, so nothing pinned what it said. The tested half was
tested; the untested half is the half that drifted.

Generalisation, distinct from the plumbing one: **when one side introduces a
constant for a literal, grep the other side for the literal's text, not for the
constant's name.** A new duplicate cannot mention a constant it never knew about,
so a name-based search finds nothing and reads as clean. And when a merge leaves
two paths for one condition, ask which of them a test actually pins — the answer
is usually "one".

## Tagging

Release tags track upstream plus a fork counter:

```text
v{upstream}+providers.N
```

Examples: `v0.0.0+providers.1`, or `v1.2.3+providers.1` when upstream publishes a real SemVer. Put the upstream `main` SHA in the release notes. Prefer SemVer **build metadata** (`+providers.N`) over a prerelease suffix (`-providers`), which sorts older than the base version.

## CI / CD

GitHub Actions live only on **`providers`** (see [`.github/workflows/ci.yml`](.github/workflows/ci.yml)). Style matches our other repos: named `CI`, `pull_request`/`workflow_dispatch` runs cancel superseded attempts, `push` runs are SHA-scoped and uncancelled so every merged commit can satisfy the release gate, and jobs stay split into **Format / Clippy / Tests** lanes.

Triggers:

- `push` to `providers`
- `pull_request` targeting `providers` (feature and `sync/upstream-*` branches)
- `workflow_dispatch`

Scope is the fork hot path (not full workspace):

- `cargo fmt --all -- --check`
- `clippy --all-targets -D warnings` on `xai-grok-sampler`, `xai-grok-shell`,
  `xai-grok-tools` (with `--features pi`), `xai-grok-pager`, `xai-grok-pager-bin`,
  and `xai-grok-workspace` (enrolled after it sat failing while `providers`
  looked green, #439). Two things this still does **not** cover, both of
  which have read as coverage before:
  - **The rest of the workspace.** `xai-grok-config` is not on that named
    list. Omitting `--no-deps` still *builds* it as a library dependency of
    `xai-grok-shell`, but Cargo invokes that dependency through `rustc`,
    not `clippy-driver`, and does not forward `-D warnings`. A warning in
    config therefore stays green. Config is on the test list (`display::`,
    `validation::`, `state_home::`) and on
    `tests/ci/unlinted-crates.allowlist`; it has no clippy
    `--manifest-path` of its own. The remaining unnamed crates are
    recorded by `check_unlinted_crates.py`; triaging them is #457.
    Test coverage and lint coverage are answered by different lists:
    `xai-grok-workspace` is named by dozens of `run_nonzero` test filters
    *and* by a clippy `--manifest-path` of its own.
  - **`required-features` targets used to be skipped silently.**
    `--all-targets` omits them with no error and no warning, so the six
    `[[test]]` targets in `xai-grok-shell` gated on `test-support` — the
    memory/OOM regression suite — and the sibling `[[bench]] fork_copy`
    were invisible to the plain `--all-targets` line (#474). CI now has
    dedicated invocations for that gate: `cargo clippy ... --all-targets
    --features test-support` in the clippy job, and `cargo test -p
    xai-grok-shell --tests --benches --features test-support --no-run` in
    compile-tests. The `--features pi` line above exists because somebody
    hit the same skip for one crate.

    Feature unification is no longer the only compile path for those six
    tests: `xai-grok-pager`'s `[dev-dependencies]` still names
    `xai-grok-shell = { features = ["test-support"] }`
    (`crates/codegen/xai-grok-pager/Cargo.toml:168`), so
    `cargo test --workspace --no-run` also turns the gate on, but dropping
    that one dev-dependency feature no longer silently stops the dedicated
    compile-tests / clippy steps from building or linting them.
- Targeted auth / readiness / model-picker tests (subagent None credential strip, session model-switch credential clear, pager unready hard-blocks)

**Merge pull requests with `scripts/merge-pr.sh`, not `gh pr merge`** (issue #202):

```bash
scripts/merge-pr.sh <number>            # --squash --delete-branch by default
```

It reconciles the PR head against the remote branch tip, requires a successful run of
*this* repository's `ci.yml` for exactly that SHA, requires every check to have concluded,
and then **re-reads the branch tip** before merging — a push that lands between the receipt
and the merge would otherwise be merged with none of it verified. That last read is the
step a human skips.

The receipt check underneath can still be run alone, for diagnostics:

```bash
python3 -B scripts/check_pr_head_ci_run.py --pr <number> --repo ImL1s/medley
```

Direct branch/SHA probe (for diagnostics and no-run repros):

```bash
python3 -B scripts/check_pr_head_ci_run.py \
  --branch <branch> --head-sha <sha> --repo ImL1s/medley
```

This guard runs from the developer/orchestrator host (outside GitHub Actions),
so it can fail closed when the thing being watched is "no run created". For
feature PR branches it resolves the branch head with `git ls-remote`, lists
`pull_request` runs via `gh run list --workflow ci.yml --branch <branch>
--event pull_request`, and matches the target commit against the run detail's
`pull_requests[].head.sha` (not top-level `head_sha`, which is the ephemeral
merge commit). For `providers`, where CI push runs actually exist, it keeps the
release-gate identity shape (`event == "push"`, `head_branch`, exact
`head_sha`, and workflow `path == ".github/workflows/ci.yml"`). It
intentionally does **not** use `gh pr checks` to answer "did this SHA get CI".

`gh pr checks` is the wrong probe for that question: an empty result prints
**"no checks reported"**, which looks unfinished rather than fail-closed, and
cannot tell a dropped webhook from a run that has not started. The guard
prints a `verdict:` line so those states stay distinct:

- `success` — completed successful `ci.yml` run for this exact head
- `absent` — zero runs for this head. Three causes, and they need opposite
  responses: a dropped webhook, a run that was never created, or **the PR is
  `CONFLICTING`**, in which case GitHub cannot compute `refs/pull/<n>/merge` and
  never dispatches the `pull_request` run at all. Check `mergeable` before
  assuming one of the first two — those you wait out, the third you fix by
  merging the base in. Observed on #383 on 2026-08-23: merging #404 into
  `providers` flipped it to `CONFLICTING`, and the next push produced zero check
  runs while a PR opened minutes later ran normally. It reads exactly like a
  slow queue. Corollary: **merging anything into `providers` can flip every
  other open PR to `CONFLICTING` in the same instant**, so sweep
  `gh pr list --json number,mergeable` after each merge rather than waiting to
  notice that a PR's CI "stopped moving".
- `in_progress` — a run exists and is queued / in progress / waiting
- `skipped` — a run completed as skipped / cancelled, not success
- `failed` / `identity_rejected` — finished unsuccessfully, or the path/event
  identity check rejected the run

`scripts/merge-pr.sh` still reads `gh pr checks --json` as a second gate
(`python3 -B scripts/check_pr_head_ci_run.py --evaluate-pr-checks`): empty is
`absent` and fail-closed; pending is `in_progress`, not absent; skip-only is
not success. Do not treat `gh pr checks` with no rows as green. To answer
"did this push get CI" yourself:

```bash
gh run list --repo ImL1s/medley --workflow ci.yml --branch <branch> \
  --limit 5 --json headSha,status,conclusion,event,url
```

`main` has no fork workflows — it stays an upstream fast-forward mirror. Do not merge `providers` into `main`.

## Docs

- Implementation plan: [docs/plans/2026-07-23-multi-provider-local-llm.md](docs/plans/2026-07-23-multi-provider-local-llm.md)
- Custom models / providers guide: [crates/codegen/xai-grok-pager/docs/user-guide/11-custom-models.md](crates/codegen/xai-grok-pager/docs/user-guide/11-custom-models.md)

## TUI and config

- **Source of truth for providers:** `config.toml` in the resolved state directory (`~/.medley/config.toml` by default — see the resolution order above) — `[model.*]` entries (`auth_scheme`, `env_key`, `api_backend`, `base_url`, …) and optional `[models].default`.
- **Day-to-day switching:** TUI `/model` (or `/m`) and **Ctrl+M** (model picker from scrollback). The picker selects from the catalog; it does not replace editing `config.toml` for adding providers.

Do not commit secrets, local agent scratch (`.omc/`), or scratch notes into this fork’s product branch.
