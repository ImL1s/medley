# ImL1s fork of xai-org/grok-build

This repository is **[ImL1s/grok-build](https://github.com/ImL1s/grok-build)**, a friendly fork of **[xai-org/grok-build](https://github.com/xai-org/grok-build)** with a long-lived multi-provider / local LLM product line.

Remotes:

| Remote     | URL                                         | Role                          |
|------------|---------------------------------------------|-------------------------------|
| `upstream` | `https://github.com/xai-org/grok-build.git` | Read-only upstream mirror     |
| `origin`   | `https://github.com/ImL1s/grok-build.git`   | Fork: PRs, releases, default  |

## Branch model

| Branch       | Role                                                                 |
|--------------|----------------------------------------------------------------------|
| `main`       | **Pristine upstream mirror.** Only fast-forward from `upstream/main`. Never land provider/custom commits here. |
| `providers`  | **Product line.** Multi-provider credentials, keyless local LLMs, fork docs/UX. Default branch for users and releases. |

### GitHub branch protection (recommended)

Configure these on **[ImL1s/grok-build](https://github.com/ImL1s/grok-build)** so fork workflow stays safe:

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
| Session safety | Catalog key vs wire slug kept separate; readiness gated in `model_switch::apply` (covers new/load/switch); strip stale `api_key` / no bearer resolver for `None`. |
| TUI readiness | `/model` and Ctrl+M show `ready` / `missing` / `none` badges; hard-block unready; soft-confirm auth-class changes. |
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
- `crates/codegen/xai-grok-pager/` — `/model` slash command, model picker (`Ctrl+M`), `available_models` rendering
- Custom models docs: `crates/codegen/xai-grok-pager/docs/user-guide/11-custom-models.md`

Prefer keeping fork intent on auth hotspots (`AuthScheme::None`, `local.none`, no ambient xAI credential leak) rather than blindly taking upstream.

## Tagging

Release tags track upstream plus a fork counter:

```text
v{upstream}+providers.N
```

Examples: `v0.0.0+providers.1`, or `v1.2.3+providers.1` when upstream publishes a real SemVer. Put the upstream `main` SHA in the release notes. Prefer SemVer **build metadata** (`+providers.N`) over a prerelease suffix (`-providers`), which sorts older than the base version.

## CI / CD

GitHub Actions live only on **`providers`** (see [`.github/workflows/ci.yml`](.github/workflows/ci.yml)). Style matches our other repos: named `CI`, concurrency cancel-in-progress, separate **Format / Clippy / Tests** jobs.

Triggers:

- `push` to `providers`
- `pull_request` targeting `providers` (feature and `sync/upstream-*` branches)
- `workflow_dispatch`

Scope is the fork hot path (not full workspace):

- `cargo fmt --all -- --check`
- `clippy --lib -D warnings` on `xai-grok-sampler`, `xai-grok-shell`, `xai-grok-pager` (lib only; avoids unrelated upstream bench/test lints)
- Targeted auth / readiness / model-picker tests (subagent None credential strip, session model-switch credential clear, pager unready hard-blocks)

`main` has no fork workflows — it stays an upstream fast-forward mirror. Do not merge `providers` into `main`.

## Docs

- Implementation plan: [docs/plans/2026-07-23-multi-provider-local-llm.md](docs/plans/2026-07-23-multi-provider-local-llm.md)
- Custom models / providers guide: [crates/codegen/xai-grok-pager/docs/user-guide/11-custom-models.md](crates/codegen/xai-grok-pager/docs/user-guide/11-custom-models.md)

## TUI and config

- **Source of truth for providers:** `~/.grok/config.toml` — `[model.*]` entries (`auth_scheme`, `env_key`, `api_backend`, `base_url`, …) and optional `[models].default`.
- **Day-to-day switching:** TUI `/model` (or `/m`) and **Ctrl+M** (model picker from scrollback). The picker selects from the catalog; it does not replace editing `config.toml` for adding providers.

Do not commit secrets, local agent scratch (`.omc/`), or scratch notes into this fork’s product branch.
