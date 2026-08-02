<div align="center">

<h1>Medley</h1>

**A community multi-provider fork of [Grok Build](https://github.com/xai-org/grok-build).**

Medley keeps upstream Grok Build's terminal coding agent — the full-screen TUI
that reads your codebase, edits files, runs shell commands, searches the web,
and manages long-running tasks — and adds provider choice on top of it: a
provider-scoped OpenAI Codex login, any OpenAI-compatible endpoint, and keyless
local models, each with its credentials kept in its own lane.

[Fork notice](#fork-notice) ·
[What Medley adds](#what-medley-adds) ·
[Coexistence](#coexistence-with-official-grok-build) ·
[Installing](#installing) ·
[Documentation](#documentation) ·
[Repository layout](#repository-layout) ·
[Development](#development) ·
[Support and contributions](#support-and-contributions) ·
[License and notices](#license-and-notices)

</div>

---

## Fork notice

> [!IMPORTANT]
> **Medley is a community fork. It is not affiliated with, endorsed by,
> sponsored by, or supported by xAI.**
>
> "Grok" and "Grok Build" are trademarks of xAI; the Apache-2.0 license that
> covers the upstream source grants no trademark rights. Medley uses those names
> only to identify the project it is derived from. xAI provides no warranty,
> support, or security response for this distribution — see
> [`NOTICE.md`](NOTICE.md) for the full trademark and non-affiliation statement.

> [!NOTE]
> **The command is still `grok`.** Medley currently builds and runs as the
> upstream binary name, so every command example in this repository — here and
> throughout the user guide — reads `grok …`, and state still lives in
> `~/.grok/`. The `medley` command alias, the `~/.medley/` state directory, and
> the `MEDLEY_*` environment prefix arrive with the packaging change tracked in
> [#49](https://github.com/ImL1s/grok-build/issues/49). Until then, read
> [Coexistence](#coexistence-with-official-grok-build) before installing Medley
> on a machine that already has official Grok Build.

This repository ([ImL1s/grok-build](https://github.com/ImL1s/grok-build)) tracks
upstream on `main` (a pristine fast-forward mirror) and ships the fork's product
line on `providers`, which is the default branch for users and releases. See
[`FORK.md`](FORK.md) for the branch model, the upstream sync process, and the
full list of divergences. [`SOURCE_REV`](SOURCE_REV) records the upstream commit
this tree was synced from.

## What Medley adds

Relative to upstream Grok Build, the `providers` branch adds:

| Capability | What it means |
|------------|---------------|
| **Provider-scoped OpenAI Codex OAuth** | `grok login --provider openai-codex` signs in with a ChatGPT account through a Codex-compatible browser OAuth flow with PKCE. The credential is stored under its own `openai::codex` scope, refreshes and rotates independently, and `grok logout --provider openai-codex` removes only that scope — the xAI session and any credential owned by the official Codex CLI are left untouched. Check state with `grok auth status --provider openai-codex`. |
| **Custom OpenAI-compatible endpoints** | `[model.*]` entries take `base_url`, `api_backend` (`chat_completions`, `responses`, `messages`), `env_key`, per-request `query_params`, and `env_http_headers` so a secret can come from the environment instead of `config.toml`. Anthropic, Gemini, OpenRouter, Together AI, and generic gateways are worked examples. |
| **Keyless local models** | `auth_scheme = "none"` sends no `Authorization` / `x-api-key` at all, which is what Ollama, LM Studio, llama.cpp, and vLLM expect. Without it a local server can receive an ambient xAI Bearer token it never asked for. |
| **Strict credential isolation** | An unknown or malformed `auth_scheme` marks the model **unready** and fails closed rather than silently falling back to Bearer; unready models are rejected by the picker, `/model`, new sessions, session restore, and ACP model switches. On third-party endpoints and `auth_scheme = "none"`, the `x-grok-user-id` / `x-grok-deployment-id` identity headers are omitted. Custom model-catalog discovery uses an explicit key rather than your signed-in session. |
| **Readiness surfaced in the TUI** | `/model` and `Ctrl+M` show `ready` / `missing` / `none` badges, hard-block unready models, and ask for confirmation when a switch crosses auth classes. |

Details and copy-pasteable configuration live in
[Custom Models](crates/codegen/xai-grok-pager/docs/user-guide/11-custom-models.md)
and [Authentication](crates/codegen/xai-grok-pager/docs/user-guide/02-authentication.md).

> [!NOTE]
> The Codex transport is compatibility with a pinned public Codex contract — not
> an OpenAI Platform API, not a stability guarantee, and not an endorsement by
> OpenAI. Your account entitlements, workspace policy, and OpenAI's terms still
> govern what works. See the service boundary in [`NOTICE.md`](NOTICE.md).

## Coexistence with official Grok Build

Until the packaging change lands, Medley and official Grok Build **share
runtime identity**, and installing both on one machine is not yet safe:

- both provide a `grok` command, so `PATH` order silently decides which one runs;
- both read and write `~/.grok/` — Medley writes provider-scoped credentials and
  config fields (`model_provider`, `auth_scheme`) that upstream does not know,
  and upstream schema changes can likewise confuse Medley. Alternating between
  the two can corrupt that state in ways that look like random bugs;
- both honour the same `GROK_*` environment variables.

Distinct commands, a distinct state directory, and a one-time copy migration are
tracked in [#49](https://github.com/ImL1s/grok-build/issues/49).

> [!WARNING]
> The inherited auto-updater points at upstream's release channel
> (`x.ai/cli/install.sh`). Letting it run on a Medley build will replace it with
> an official Grok Build binary and silently drop the fork's features. Set
> `auto_update = false` under `[cli]` in `~/.grok/config.toml` on a
> source-built Medley install. Fork-published release artifacts are tracked in
> [#29](https://github.com/ImL1s/grok-build/issues/29).

## Installing

> [!CAUTION]
> **The official installer does not install Medley.** `x.ai/cli/install.sh`,
> `x.ai/cli/install.ps1`, and `grok update` all fetch xAI's official build. Use
> them only if upstream Grok Build is what you want. Fork-specific installer
> guidance is tracked in [#28](https://github.com/ImL1s/grok-build/issues/28);
> until it lands, building from source is the supported path for Medley.

### Building from source

Requirements:

- **Rust** — the toolchain is pinned by [`rust-toolchain.toml`](rust-toolchain.toml);
  `rustup` installs it automatically on first build.
- **[DotSlash](https://dotslash-cli.com)** — required so hermetic tools under
  [`bin/`](bin/) (notably [`bin/protoc`](bin/protoc)) can download and run.
  Install it and ensure `dotslash` is on your `PATH` **before** building:

  ```sh
  cargo install dotslash
  # or: prebuilt packages — https://dotslash-cli.com/docs/installation/
  /usr/bin/env dotslash --help   # sanity check
  ```

- **protoc** — proto codegen resolves [`bin/protoc`](bin/protoc) via DotSlash,
  or falls back to a `protoc` on `PATH` / `$PROTOC`.
- macOS and Linux are supported build hosts; Windows builds are best-effort
  and not currently tested from this tree.

```sh
git clone -b providers https://github.com/ImL1s/grok-build.git
cd grok-build
cargo run -p xai-grok-pager-bin              # build + launch the TUI
cargo build -p xai-grok-pager-bin --release  # release binary: target/release/xai-grok-pager
cargo check -p xai-grok-pager-bin            # fast validation
```

Check out `providers` — `main` is the pristine upstream mirror and contains none
of the fork's changes.

The binary artifact is named `xai-grok-pager`; official upstream installs ship it
as `grok`, and the fork's own `medley` alias arrives with the packaging change.
On first launch it opens your browser to authenticate — see the
[authentication guide](crates/codegen/xai-grok-pager/docs/user-guide/02-authentication.md).

## Documentation

The user guide ships with the pager crate:
[`crates/codegen/xai-grok-pager/docs/user-guide/`](crates/codegen/xai-grok-pager/docs/user-guide/)
— getting started, keyboard shortcuts, slash commands, configuration, theming,
MCP servers, skills, plugins, hooks, headless mode, sandboxing, and more. Pages
that describe fork-specific behaviour carry a fork note; the rest is upstream
documentation carried along by the sync, so treat installation, update, and
support instructions in it as describing the **official** build.

xAI's online documentation for the upstream product is at
[docs.x.ai/build/overview](https://docs.x.ai/build/overview). It does not cover
this fork.

## Repository layout

| Path | Contents |
|------|----------|
| `crates/codegen/xai-grok-pager-bin` | Composition-root package; builds the `xai-grok-pager` binary |
| `crates/codegen/xai-grok-pager` | The TUI: scrollback, prompt, modals, rendering |
| `crates/codegen/xai-grok-shell` | Agent runtime + leader/stdio/headless entry points |
| `crates/codegen/xai-grok-tools` | Tool implementations (terminal, file edit, search, ...) |
| `crates/codegen/xai-grok-workspace` | Host filesystem, VCS, execution, checkpoints |
| `crates/codegen/...` | The rest of the CLI crate closure (config, MCP, markdown, sandbox, ...) |
| `crates/common/`, `crates/build/`, `prod/mc/` | Small shared leaf crates pulled in by the closure |
| `third_party/` | Vendored upstream source (Mermaid diagram stack) — see below |

Crate names, module paths, and the cargo bin target deliberately keep their
`xai-grok-*` names: renaming them would make every upstream sync a conflict.
The rebrand is an outward one.

> [!IMPORTANT]
> The root `Cargo.toml` (workspace members, dependency versions, lints,
> profiles) is **generated** — treat it as read-only. Prefer editing per-crate
> `Cargo.toml` files.

## Development

```sh
cargo check -p <crate>        # always target specific crates; full-workspace builds are slow
cargo test -p xai-grok-config # per-crate tests
cargo clippy -p <crate>       # lint config: clippy.toml at the repo root
cargo fmt --all               # rustfmt.toml at the repo root
```

CI runs on `providers` only, over the fork's hot path. See [`FORK.md`](FORK.md)
for the sync workflow, the auth/config watchlist, and the release tagging scheme.

## Support and contributions

Report Medley bugs and request features on the fork's tracker:
<https://github.com/ImL1s/grok-build/issues>. Feature branches target
`providers`, never `main`.

> [!WARNING]
> [`CONTRIBUTING.md`](CONTRIBUTING.md) and [`SECURITY.md`](SECURITY.md) are
> inherited upstream documents describing **xAI's** policies for the official
> project — including a HackerOne program that does not cover this fork. Do not
> send Medley bugs or vulnerability reports there. The fork's own contribution
> and security policy is tracked in
> [#28](https://github.com/ImL1s/grok-build/issues/28).

## License and notices

First-party code in this repository — upstream's and the fork's — is licensed
under the **Apache License, Version 2.0**; see [`LICENSE`](LICENSE). The fork's
modifications relative to upstream are described in [`FORK.md`](FORK.md), which
serves as the Apache-2.0 §4(b) change notice.

[`NOTICE.md`](NOTICE.md) carries the upstream attribution, the trademark
statement, the non-affiliation notice, and the third-party service boundary for
the OpenAI Codex transport.

Third-party and vendored code remains under its original licenses. See:

- [`THIRD-PARTY-NOTICES`](THIRD-PARTY-NOTICES) — crates.io / git dependencies,
  bundled UI themes, and **in-tree source ports** (including openai/codex and
  sst/opencode tool implementations)
- [`crates/codegen/xai-grok-tools/THIRD_PARTY_NOTICES.md`](crates/codegen/xai-grok-tools/THIRD_PARTY_NOTICES.md)
  — crate-local notice for the codex and opencode ports (license texts +
  Apache §4(b) change notice)
- [`third_party/NOTICE`](third_party/NOTICE) — vendored Mermaid-stack index
