# Medley — ImL1s fork of xai-org/grok-build

This repository is **[ImL1s/medley](https://github.com/ImL1s/medley)**, published as **Medley**: a community fork of **[xai-org/grok-build](https://github.com/xai-org/grok-build)** with a long-lived multi-provider / local LLM product line. It is not affiliated with or endorsed by xAI — see [`NOTICE.md`](NOTICE.md) for the trademark and non-affiliation statement.

This file doubles as the Apache-2.0 §4(b) statement of modification: the "What's different from upstream" table below is the record of changes made to the upstream work.

Release archives and [`install.sh`](install.sh) ship the binary as `medley` and install it under `~/.medley/bin`; a source build keeps the upstream cargo bin target name `xai-grok-pager`, which upstream installs ship as `grok`. State resolves in this order: `$MEDLEY_HOME`, `$GROK_HOME`, `~/.medley` when it exists, an existing `~/.grok` (which the first interactive run offers to copy across), then `~/.medley`. Renaming the application's own `GROK_*` environment variables is remaining scope on [#49](https://github.com/ImL1s/medley/issues/49).

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
- `crates/codegen/xai-grok-pager/` — `/model` slash command, model picker (`Ctrl+M`), `available_models` rendering
- Custom models docs: `crates/codegen/xai-grok-pager/docs/user-guide/11-custom-models.md`

Prefer keeping fork intent on auth hotspots (`AuthScheme::None`, `local.none`, no ambient xAI credential leak) rather than blindly taking upstream.

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

- `git log --merge -p -- <file>` shows nothing for conflicts of this shape, by
  definition. Do not read that silence as safety.
- After resolving, grep the whole tree for every env var, setter, or flag the
  incoming side removed or gated — including `#[cfg(test)]` code, which
  `cargo check --lib` and clippy on `--lib` never compile.
- `cargo test --workspace --no-run` compiles those targets without running them.
  That catches callers which stopped *compiling*. It cannot catch the ones that
  still compile and now read the wrong thing — only the grep does.

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
- `clippy --lib -D warnings` on `xai-grok-sampler`, `xai-grok-shell`, `xai-grok-pager` (lib only; avoids unrelated upstream bench/test lints)
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
intentionally does **not** use `gh pr checks`.

`main` has no fork workflows — it stays an upstream fast-forward mirror. Do not merge `providers` into `main`.

## Docs

- Implementation plan: [docs/plans/2026-07-23-multi-provider-local-llm.md](docs/plans/2026-07-23-multi-provider-local-llm.md)
- Custom models / providers guide: [crates/codegen/xai-grok-pager/docs/user-guide/11-custom-models.md](crates/codegen/xai-grok-pager/docs/user-guide/11-custom-models.md)

## TUI and config

- **Source of truth for providers:** `config.toml` in the resolved state directory (`~/.medley/config.toml` by default — see the resolution order above) — `[model.*]` entries (`auth_scheme`, `env_key`, `api_backend`, `base_url`, …) and optional `[models].default`.
- **Day-to-day switching:** TUI `/model` (or `/m`) and **Ctrl+M** (model picker from scrollback). The picker selects from the catalog; it does not replace editing `config.toml` for adding providers.

Do not commit secrets, local agent scratch (`.omc/`), or scratch notes into this fork’s product branch.
