# medley

A community fork of [xai-org/grok-build](https://github.com/xai-org/grok-build) that adds multi-provider support. Rust cargo workspace. Not affiliated with xAI.

Read [FORK.md](FORK.md) first — it owns the branch model, the release contract, and the upstream-sync watchlist. This file is the parts that are easy to get wrong and expensive to rediscover.

## Names: three of them, deliberately

| Layer | Name |
|---|---|
| cargo bin target | `xai-grok-pager` |
| shipped command | `medley` |
| what the program calls itself in help and messages | still `grok` — see #117 |

The rename happens at packaging (`release.yml`) and again in `install.sh`, **never in the cargo target** — renaming it would churn every upstream sync. Crates stay `xai-grok-*` for the same reason.

State lives in `~/.medley`. `grok_home()` falls back to `~/.grok` when `~/.medley` is absent, which is documented coexistence — and a trap when testing (below).

## Branches

- `main` — pristine upstream mirror. Fast-forward only, never product PRs.
- `providers` — the product line. Everything lands here; releases are cut from here.

CI runs on `providers` only. `main` moving triggers nothing.

## Verification habits this repo earned the hard way

**A green test suite can mean a filter matched nothing.** CI wraps its hot-path filters in `run_nonzero`, which fails when a filter matches zero tests. `cargo test --exact <short-name>` matches nothing and exits 0 — it needs the full module path.

**`cargo check --lib` does not compile `#[cfg(test)]` code.** Neither does clippy on `--lib`. A workspace can pass check, clippy, fmt, and a filtered hot-path run while the test build of the largest crate is broken — that is exactly what the 2026-08-04 upstream sync did, and only CI caught it. Run `cargo test --workspace --no-run` before pushing anything that touches shared types: it compiles every test target without running them, which is the cheapest way to find a fork test still calling something upstream deleted.

**Fork fields on `SamplingConfig` / `SamplerConfig` are a per-sync tax (#121).** Rust struct literals must name every field. The 2026-08-04 sync paid **11 of 25** test-build errors for a single fork field (`endpoint_trust`) on `SamplingConfig` — all upstream constructors that did not know the field existed. Today `SamplingConfig` still carries **one** fork field (`endpoint_trust`); `SamplerConfig` carries **two** (`endpoint_trust`, `credential_source`). Before adding another:

1. Prefer a **fork-owned type** (as #136 step 1 did with `Credentials` / `StoredAuth`) over a new field on either of those two structs.
2. If the field must live on `SamplingConfig`, set it in `SamplingConfig::for_test` and use FRU (`..for_test(..)`) at test sites. Do **not** add `#[derive(Default)]` / empty-URL defaults — that trades a compile error for a distant runtime one — and do **not** put `api_key` on `SamplingConfig` (provenance is bound on `Credentials` / `SamplerConfig`, #180).
3. The structural cap (one fork-namespaced field absorbing the rest) is sequenced behind #136's envelope, not a standalone refactor.

**Isolating the install does not isolate the run.** When checking a built binary, pass `HOME` (or `MEDLEY_HOME`) to *every invocation*, not just to `install.sh`. Otherwise `grok_home()` falls back to the developer's `~/.grok` and answers plausibly wrong — `channel` reads `stable` instead of `unknown`, and nothing errors.

**`~/.medley/bin/medley` is a launcher script, not the binary.** It bakes `MEDLEY_HOME` at install time. To test binary behaviour, run `~/.medley/versions/<v>/medley` directly. `file <path>` tells you which you have.

**Silence from a verifier is not a pass.** `gh attestation verify` prints nothing on success in a non-TTY. Validate such checks against a known-bad input first, or a no-op looks like a green.

**Inserting code above a `fn` steals its attributes.** Anchor on the attribute block (`    #[test]\n    fn name`), not on `fn name`. A stolen `#[test]` compiles fine and silently stops running; only clippy's `duplicated attribute` notices.

## Credentials must never reach diagnostics

This is the fork's sharpest divergence from upstream (#33), and every upstream sync collides with it — six conflicts in the 2026-08-04 sync alone.

There are **~145 hand-written `Debug` impls** whose only job is to report `*_present` booleans or `<redacted>` instead of values. Upstream keeps adding `#[derive(Debug)]` to the same types. When a conflict shows nothing but a visibility change plus a `Debug` derive, **check whether the type has a manual impl outside the hunk** — the diff will not show it, and taking the derive re-introduces the leak. E0119 catches it only when both end up in the same file.

Same rule for log fields: log `command_present`, not `cmd = %command`.

CI's hot path is exactly this suite (`*_never_emit_credential_bytes`, `*_is_secret_free_*`, `omits_xai_identity`, `hostile_injector`, `none_auth_scheme_`). If a change is going to break something, that is where it shows.

## Paths in user-facing strings

Never write `~/.grok` in a message. Use `xai_grok_config::display_user_grok_path("config.toml")` or `display_grok_home_prefix()`, which resolve the directory this install actually uses.

Two guard tests enforce it and both scan **source, not rendered output** — rendering resolves the *developer's* state directory, so on a machine with a live `~/.grok` a correct message contains `~/.grok` and a naive assertion fails for being right:

- `no_clap_doc_comment_hardcodes_the_state_directory` — a clap `///` comment *becomes* help text and cannot interpolate, so paths in one are frozen. Use `#[arg(long_help = ...)]`.
- `no_user_facing_message_hardcodes_the_state_directory` — string literals outside comments.

Deliberate exceptions: `.grok/config.toml` and `.grok/sandbox.toml` are **project-local** files that really are named that.

## Build

`protoc` is pinned per platform in `bin/protoc`. Build scripts emit `cargo:rerun-if-changed=<path>` — two rules, both learned from a 122-minute CI run:

- The path is resolved **relative to the package root**, and a path that does not exist marks the crate permanently dirty. Emitting a bare `protoc` rebuilt the world on every job.
- Cargo parses build-script stdout **one directive per line**, so a newline inside a value injects further directives. Validate before printing (`xai-grok-version/build.rs` shows the shape).

## Releases

Tags are `v<upstream>+providers.<N>`; the workflow rejects a bare upstream tag. Pushing the tag builds four targets, then creates a **draft** — the `### Changes` section is a placeholder someone must write before publishing. Publish with `--latest` explicitly; `+providers.N` build metadata is not something to trust GitHub to order.

The pre-release gate accepts only a completed, successful run of *this* repo's `ci.yml`, `event=push`, `head_branch=providers`, at exactly the release SHA — matched on `.path`, so another workflow calling itself "CI" cannot satisfy it.

## Upstream sync

`./scripts/sync-upstream.sh`, then resolve against FORK.md's watchlist. Two things it does not tell you:

- The script does `git checkout providers`, which **fails if another worktree holds that branch**. Fast-forward the holding worktree first, then the align step becomes a no-op.
- When judging a conflict, the decisive question is almost always *what does the merge base have?* Base-and-fork-but-not-upstream means upstream deleted it; fork-only means the fork added it. Guessing from the hunk alone is how a fork feature gets dropped.

Watch `rust-toolchain.toml`: `ci.yml` pins the toolchain in three places and must follow it. `release.yml` does not pin.
