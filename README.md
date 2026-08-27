<div align="center">

<img src="docs/assets/logo.png" alt="Medley logo" width="128" height="128" />

# Medley

**A community multi-provider fork of [Grok Build](https://github.com/xai-org/grok-build).**

Medley keeps upstream Grok Build's terminal coding agent — the full-screen TUI
that reads your codebase, edits files, runs shell commands, searches the web,
and manages long-running tasks — and adds provider choice on top of it: a
provider-scoped OpenAI Codex login, any OpenAI-compatible endpoint, and keyless
local models, each with its credentials kept in its own lane.

<p>
<img src="docs/assets/hero.jpg" alt="Medley: many providers, one terminal agent" width="920" />
</p>

[Fork notice](#fork-notice) ·
[What Medley adds](#what-medley-adds) ·
[Quick start](#quick-start-providers-branch) ·
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
> **Three names for one binary.** The release archives and
> [`install.sh`](install.sh) ship it as **`medley`**, installed under
> `~/.medley/bin`. A **source build** produces the upstream cargo bin target
> **`xai-grok-pager`** — crate and bin names deliberately do not change, see
> [Repository layout](#repository-layout). Official **upstream** installs ship
> that same target as **`grok`**. Command examples in this repository and throughout the user
> guide are written as `grok …`; read them as "whatever you invoke your build
> as".
>
> **State lives in `~/.medley`.** It resolves as `$MEDLEY_HOME` → `$GROK_HOME` →
> `~/.medley` when it exists → `~/.grok` when it exists and `~/.medley` does not
> → `~/.medley`, so a fresh install lands in `~/.medley` while an existing
> `~/.grok` keeps working. A documented, user-facing subset of application
> variables also reads `MEDLEY_*` first with `GROK_*` as a permanent fallback
> ([#426](https://github.com/ImL1s/medley/issues/426)); most `GROK_*` variables
> have no `MEDLEY_*` equivalent and never will. See
> [Coexistence](#coexistence-with-official-grok-build) for the migration and the
> remaining sharp edges.

This repository ([ImL1s/medley](https://github.com/ImL1s/medley)) tracks
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
| **Optional native subagent route contract** | Plugin-facing exact / ordered-candidate / receipt types for orchestration consumers. Optional and capability-negotiated; original Grok Build is not claimed to implement it. See [native-subagent-route-contract.md](docs/architecture/native-subagent-route-contract.md). Live spawn wiring and replay-safe fallback are still incomplete. |

Details and copy-pasteable configuration live in
[Custom Models](crates/codegen/xai-grok-pager/docs/user-guide/11-custom-models.md)
and [Authentication](crates/codegen/xai-grok-pager/docs/user-guide/02-authentication.md).

> [!NOTE]
> The Codex transport is compatibility with a pinned public Codex contract — not
> an OpenAI Platform API, not a stability guarantee, and not an endorsement by
> OpenAI. Your account entitlements, workspace policy, and OpenAI's terms still
> govern what works. See the service boundary in [`NOTICE.md`](NOTICE.md).

## Quick start (providers branch)

Use this section for the fork-specific path (`providers`) with no literal
credentials in config files.

### 1) Keyless local model (Ollama example)

`auth_scheme = "none"` keeps local requests credential-free:

```toml
# ~/.medley/config.toml
[model.ollama-qwen]
label = "Ollama Qwen"
model = "qwen2.5-coder:14b"
base_url = "http://127.0.0.1:11434/v1"
api_backend = "chat_completions"
auth_scheme = "none"
context_window = 128000

[models]
default = "ollama-qwen"
```

### 2) External provider (env-var credential)

Keep secrets in the environment, not in `config.toml`:

```sh
export OPENAI_API_KEY="your-api-key-here"
```

```toml
# ~/.medley/config.toml
[model.gpt-4o]
label = "OpenAI GPT-4o"
model = "gpt-4o"
base_url = "https://api.openai.com/v1"
api_backend = "chat_completions"
env_key = "OPENAI_API_KEY"
context_window = 128000

[models]
default = "gpt-4o"
```

Then run whichever command name your install provides (`medley` from release
archives, `grok` in many source-build examples):

```sh
medley --model gpt-4o "Summarize this repository's branch model."
```

> [!WARNING]
> Third-party provider credentials can authorize billable API usage. Prefer
> environment variables, avoid long-lived high-privilege keys, and never commit
> tokens, API keys, or credential-bearing URLs.

For more providers and tested config blocks, see
[Custom Models](crates/codegen/xai-grok-pager/docs/user-guide/11-custom-models.md).

## Coexistence with official Grok Build

Medley now installs under its own command and keeps its state in its own
directory, so the two builds no longer collide by default. What still needs care:

- **Command.** The installer writes a `medley` command into `~/.medley/bin` and
  never touches an existing `grok` binary — it warns when it finds one. A source
  build has no installed name of its own; if you put `target/release/xai-grok-pager`
  on your `PATH` as `grok`, `PATH` order silently decides which build runs.
- **State.** Medley resolves its state directory as `$MEDLEY_HOME` →
  `$GROK_HOME` → `~/.medley` when it exists → `~/.grok` when it exists and
  `~/.medley` does not → `~/.medley`. The installed `medley` launcher pins the
  directory to its install location before the binary starts, so an installed
  Medley does not fall through to `~/.grok` — **unless you have exported
  `MEDLEY_HOME` or `GROK_HOME` yourself**, which the launcher deliberately leaves
  alone so your choice wins. A **source build** has no launcher, so on a machine
  that already has `~/.grok` it does resolve to it until the migration below runs.
- **Migration.** When Medley resolves to `~/.grok`, the first interactive run
  offers a one-time **copy** into `~/.medley`; nothing is deleted and `~/.grok`
  is left as it was. Decline, and Medley writes a `.medley-keep-legacy` marker
  into `~/.grok` and stops asking — it then keeps sharing that directory with
  official Grok Build, which can corrupt both: Medley writes provider-scoped
  credentials and config fields (`model_provider`, `auth_scheme`) that upstream
  does not know, and upstream schema changes can likewise confuse Medley.
  Non-interactive runs never prompt; they keep using `~/.grok` and log one line
  saying so.
- **Environment.** Both builds still honour the same `GROK_*` variables,
  including `GROK_HOME`. Exporting `GROK_HOME` globally therefore points both at
  one directory again — set `MEDLEY_HOME` instead, which only Medley reads.
  A documented, user-facing subset of the other application variables (auth,
  telemetry, theme, and similar) also reads a `MEDLEY_*` alias first, with
  `GROK_*` as a permanent fallback — see the "Environment Variables" reference
  in [05-configuration.md](crates/codegen/xai-grok-pager/docs/user-guide/05-configuration.md)
  for the enumerated set and [#426](https://github.com/ImL1s/medley/issues/426)
  for why it isn't every `GROK_*` variable.

> [!NOTE]
> **Medley does not self-update.** The inherited updater points at upstream's
> release channel, so running it would replace Medley with an official Grok
> Build binary and silently drop the fork's features. Every one of its entry
> points now refuses instead, and says so — no configuration required. To
> upgrade, re-run [`install.sh`](install.sh). See
> [Updates and the release channel](crates/codegen/xai-grok-pager/docs/user-guide/05-configuration.md#updates-and-the-release-channel).

## Installing

> [!CAUTION]
> **The official installer does not install Medley.** `x.ai/cli/install.sh`,
> `x.ai/cli/install.ps1`, and `grok update` all fetch xAI's official build. Use
> them only if upstream Grok Build is what you want.

### From a release (macOS and Linux)

[`install.sh`](install.sh) downloads the release archive for your platform,
verifies its SHA-256 against the release checksums file, unpacks it under
`~/.medley/versions/<version>/`, and writes a `medley` launcher into
`~/.medley/bin` that supplies `~/.medley` as the state directory whenever you
have not exported `MEDLEY_HOME` or `GROK_HOME` yourself:

```sh
curl -fsSL https://raw.githubusercontent.com/ImL1s/medley/providers/install.sh | sh
```

| Variable | Effect |
|----------|--------|
| `MEDLEY_VERSION` | Version or tag to install (default: latest published release) |
| `MEDLEY_INSTALL_DIR` | Where the `medley` launcher goes (default: `~/.medley/bin`) |
| `MEDLEY_HOME` | Where unpacked versions and state live (default: `~/.medley`) |
| `MEDLEY_TARGET` | Force a target triple instead of detecting one |
| `MEDLEY_REPO` | Source repository (default: `ImL1s/medley`) |
| `MEDLEY_DRYRUN` | Set to `1` to print the plan and skip the download, extraction, and install. The release version is still resolved first, so this queries the GitHub API unless `MEDLEY_VERSION` is also set |

Verify what you installed:

```sh
medley --version
medley version --json
```

`medley version --json` includes the build identity fields used by this fork:

- `distChannel` (packaged product/channel; expected `medley` for fork releases)
- `channel` (configured update channel name)
- `upstreamBase` (upstream commit this build is based on)
- `buildTarget` (target triple baked into the binary)

The packaged product/channel is `medley`; the git branch and tag suffix remain `providers`.

Releases are published for `aarch64`/`x86_64` macOS and Linux. The installer
refuses to install into `~/.grok`, never touches an existing `grok` binary, and
warns when it finds one. Windows is not covered — build from source there.

The `*-unknown-linux-gnu` archives are dynamically linked and need **glibc 2.35
or newer** — Ubuntu 22.04+, Debian 12+, Fedora 36+. The release job measures
that floor out of each binary and fails rather than publishing an archive that
would not start, so the number is asserted, not aspirational.

For hosts below it there is a **static `x86_64-unknown-linux-musl` archive**,
which requires no libc at all: RHEL 9, Rocky 9, Alma 9 and Amazon Linux 2023
(all glibc 2.34), Debian 11 and Ubuntu 20.04 (2.31), and Alpine, where there is
no glibc to be below. `install.sh` reads the host's glibc version and picks it
automatically; `MEDLEY_LIBC=gnu` or `MEDLEY_LIBC=musl` overrides the choice.
Prefer the gnu archives where they run — they use the system's NSS and locale
support, which a static binary cannot load.

There is **no static aarch64 build yet**, so an arm64 host below the floor
still needs a build from source; the installer says so rather than downloading
an archive that would not start. That gap is
[#424](https://github.com/ImL1s/medley/issues/424).

### Install upstream Grok Build (official xAI)

If you explicitly want the upstream product rather than this fork, use xAI's
installer:

```sh
curl -fsSL https://x.ai/cli/install.sh | bash
```

Or on Windows PowerShell:

```powershell
irm https://x.ai/cli/install.ps1 | iex
```

Those commands install and update the official build, not Medley.

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
git clone -b providers https://github.com/ImL1s/medley.git
cd medley
cargo run -p xai-grok-pager-bin              # build + launch the TUI
cargo build -p xai-grok-pager-bin --release  # release binary: target/release/xai-grok-pager
cargo check -p xai-grok-pager-bin            # fast validation
cargo run -p xai-grok-pager-bin -- version --json
```

Check out `providers` — `main` is the pristine upstream mirror and contains none
of the fork's changes.

The binary artifact is named `xai-grok-pager`; official upstream installs ship it
as `grok`, and the release archives ship it as `medley`. A source build keeps the
cargo name, so what you invoke it as is up to you. On first launch it opens your
browser to authenticate — see the
[authentication guide](crates/codegen/xai-grok-pager/docs/user-guide/02-authentication.md).
If the machine already has a `~/.grok` directory, that first interactive launch
also offers the one-time copy into `~/.medley` described under
[Coexistence](#coexistence-with-official-grok-build).

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

The optional native subagent route contract is
[`docs/architecture/native-subagent-route-contract.md`](docs/architecture/native-subagent-route-contract.md)
(#287 / #289). It is a Medley extension, not an upstream Grok Build claim.

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
<https://github.com/ImL1s/medley/issues>. Feature branches target
`providers`, never `main`.

- Contribution workflow and branch targeting: [`CONTRIBUTING.md`](CONTRIBUTING.md)
- Vulnerability reporting for this fork: [`SECURITY.md`](SECURITY.md)

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
