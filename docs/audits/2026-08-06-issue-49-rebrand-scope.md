# Issue #49 rebrand-scope inventory

- Date: 2026-08-06
- Worktree: branch `docs/49-rebrand-scope`, `providers` tip `df7e4f0`
- Issue: [`ImL1s/medley#49`](https://github.com/ImL1s/medley/issues/49) — rebrand public identity, isolate runtime state, keep upstream sync cheap
- Method: every count below comes from a command run in this worktree; the command is quoted next to the number. Nothing here is an estimate.
- Tooling note: in this worktree `rg` with an implicit search root silently returns nothing (the cwd sits under the `/tmp` symlink). Every command below passes an explicit path (`.` or a directory). Reproducing one without it yields empty output, not zero matches.

## Settled (not open questions)

1. The cargo bin target stays `xai-grok-pager` (`crates/codegen/xai-grok-pager-bin/Cargo.toml:15`). Renaming it churns every upstream sync.
2. `docs/plans/**` and `docs/audits/**` are historical records and are never rewritten. (All 214 `grok` occurrences under repo-root `docs/` live in those two trees — see category 2 — and they stay exactly as they are.)

## The line the project already drew

`crates/codegen/xai-grok-pager/src/docs.rs` (`rename_commands_to_invoked_program`, docs.rs:294) rewrites the shipped guide at runtime: a `grok …` token in **command position** (backtick span, fenced code block, behind a `$ ` prompt) becomes the invoked program name; everything that is a **real xAI reference** — `grok-4.5`, `grok.com`, `~/.grok`, `GROK_HOME`, `xai-grok-pager`, prose "a fork of Grok Build" — is preserved by word-boundary rules (docs.rs:361-396). The corpus test `no_command_in_the_shipped_guide_still_invokes_grok` (docs.rs:647) asserts **both** directions against the real guide, with an independent detector so the test cannot be satisfied by renaming more or less aggressively. Every phase below stays on that line: invocations of our binary get the fork's name; xAI's hosts, model ids, domains, and product name do not move.

## What has already landed (from issue comments, verified in tree)

- Public name decided: **medley** (issue comment "Name decision: medley", 2026-08-02). Command alias `medley`, state dir `~/.medley/`, env prefix `MEDLEY_*`.
- PR #70: packaging-layer alias — repo-root `install.sh` installs a `medley` command under `~/.medley/bin`, never touches `~/.grok` or an existing `grok` binary, and carries the non-affiliation notice.
- PR #74: state-dir isolation — `crates/codegen/xai-grok-config/src/state_dir.rs` resolves `$MEDLEY_HOME` → `$GROK_HOME` (compat) → existing `~/.medley` → existing `~/.grok` (one-time interactive copy-migration offer via `migrate_copy`, state_dir.rs:251) → `~/.medley`. User-facing path rendering goes through `display_grok_home_prefix()` / `display_user_grok_path()` (`xai-grok-config/src/display.rs`), so messages name the directory actually in use.
- `program_name()` (`xai-grok-config/src/program_name.rs`) reads argv[0]; `--version` and auth instructions already name the invoked program (#117, #125).
- `MEDLEY_*` is already a live namespace: 59 lines, 8 distinct names (`MEDLEY_HOME`, `MEDLEY_CHANNEL`, `MEDLEY_RELEASES_URL`, `MEDLEY_REPO_SLUG`, `MEDLEY_UPSTREAM_BASE`, `MEDLEY_BUILD_TARGET`, `MEDLEY_DEV_INSTALL_REF`).
  - `rg -n 'MEDLEY_' --type rust . | wc -l` → 59
  - `rg -o 'MEDLEY_[A-Z0-9_]+' --type rust . | sed 's/.*://' | sort -u | wc -l` → 8

## Category 1 — crate names / bin targets / package metadata (settled: no rename)

| What | Count | Command | Upstream-conflict risk if renamed | User-visible? |
|---|---|---|---|---|
| Workspace member crates under `crates/` | 76 | `rg -c '^\s+"crates/' Cargo.toml` | — | No |
| Crates named `xai-grok-*` | 44 | `rg -n '^name = "xai-grok' -g 'Cargo.toml' . \| wc -l` | Certain — every upstream Cargo.toml edit conflicts | No (never published) |
| `name = ` lines in Cargo.tomls (crates + explicit bins) | 139 | `rg -n '^name = ' -g 'Cargo.toml' . \| wc -l` | — | No |
| `[[bin]]` targets | 21 | `rg -n '\[\[bin\]\]' -g 'Cargo.toml' . \| wc -l` | Only one is user-facing: `xai-grok-pager` (settled). The rest are dev/probe tools (`ptyctl`, `voice-probe`, `code-graph`, playgrounds) | Only `xai-grok-pager` |
| `xai-grok` references in Cargo.tomls (dependency edges) | 325 lines in 57 files; 407 `xai-grok*` tokens | `rg -n 'xai-grok' -g 'Cargo.toml' . \| wc -l`; `rg -l 'xai-grok' -g 'Cargo.toml' . \| wc -l`; `rg -o 'xai-grok[a-z0-9-]*' -g 'Cargo.toml' . \| wc -l` | Certain | No |

Verdict: leave untouched. This is the largest conflict surface in the repo and buys nothing a user can see.

## Category 2 — user-visible strings

| What | Count | Command | Example paths | Upstream-conflict risk if renamed | User-visible? |
|---|---|---|---|---|---|
| `grok` (any case) in the shipped guide (`xai-grok-pager/docs/`) | 1029 | `rg -o -i 'grok' crates/codegen/xai-grok-pager/docs \| wc -l` | `docs/user-guide/05-configuration.md` (93), `02-authentication.md` (81), `14-headless-mode.md` (73) | None — already handled at runtime by `rename_commands_to_invoked_program`; command positions render as the invoked name | Yes (TUI picker, `<state_dir>/docs/`, model-facing) |
| Rust string literals containing the word `grok`/`Grok` | 4317 lines | `rg -n '"[^"]*\b[Gg]rok\b' --type rust crates/ \| wc -l` | throughout `crates/` | Mixed; most are log/test strings | Partly |
| …of which capitalized `Grok` (chrome/prose register) | 322 lines | `rg -n '"[^"]*\bGrok\b' --type rust crates/ \| wc -l` | see below | Low per-line, but these live in upstream files | Yes |
| Notification title `"Grok"` | 4 sites + OSC 777 payload + 1 doc example | `rg -n '"Grok"\|notify;Grok' --type rust crates/` | `xai-grok-pager/src/notifications/mod.rs:481,496,753`, `notifications/hooks.rs:123`, `notifications/protocol.rs:82` (``\x1b]777;notify;Grok;…``), `notifications/config.rs:195` | Low (small string constants) | Yes — desktop/terminal notifications |
| Diagnostics prose ("Grok can't verify…", "Grok could not…") | ~10 strings | `rg -n '"[^"]*\bGrok\b' --type rust crates/codegen/xai-grok-pager/src/diagnostics/` | `diagnostics/view.rs:391-435`, `diagnostics/fix.rs:27,320,332` | Low | Yes |
| Theme display names `"Grok Night"` / `"Grok Day"` / `"Grok Desktop"` | 3 | `rg -n '"Grok' --type rust crates/codegen/xai-grok-pager-render/src` | `pager-render/src/theme/mod.rs:146-147`, `terminal/mod.rs:94` | Low — but the theme **ids** (`groknight`, `grokday`) are config values in users' `pager.toml`; rename display names only, keep ids (see category 3) | Yes |
| `"Grok Build"` (the product name, prose) | 117 | `rg -o 'Grok Build' --type rust crates/ \| wc -l` | attribution prose, comments | Do not rename — it is xAI's product name; attribution must stay accurate (do-not-rename list) | Yes |
| Upstream in-crate installer output (`~/.grok`, `grok` in echo/Write-Host) | 4 scripts | `rg -n '\.grok' crates/codegen/xai-grok-pager/scripts/install*.sh crates/codegen/xai-grok-pager/scripts/install*.ps1` | `scripts/install.sh:113,156-157`, `install-enterprise.sh:117,160-161,256-312`, `install.ps1:38,125-128` | High if edited (upstream-owned files); superseded for fork users by root `install.sh` | Yes, if anyone runs them |
| Repo-root `docs/` (`grok`, any case) | 214 — all under `docs/plans` + `docs/audits` | `rg -o -i 'grok' docs \| wc -l`; `rg -o -i 'grok' docs/plans docs/audits \| wc -l` | point-in-time records | n/a — frozen by settled constraint 2 | Only to repo readers |

Note the split inside the 4317: only a fraction is chrome a user reads (notifications, diagnostics, help/error text). The rest is log lines, test fixtures, and format strings. Any phase-1 pass should grep the capitalized-`Grok` subset (322 lines) rather than bulk-renaming.

## Category 3 — paths and env vars on disk (migration-bearing)

These differ from display strings: users already have these on disk or in their shell environment **today**, so renaming means migration or compat fallback, not a string edit.

| What | Count | Command | On disk today? | Upstream-conflict risk | User-visible? |
|---|---|---|---|---|---|
| `~/.grok/` legacy state dir | 1653 rust lines mention `.grok` (most are tests/comments) | `rg -n '\.grok' --type rust . \| wc -l` | **Yes** — every pre-medley install. Still resolved as fallback (state_dir.rs:170-181) with copy-migration offered | Already absorbed: resolution is centralized in `state_dir.rs`; remaining literals are comments/tests and the deliberate compat scan of `~/.grok/agents` (`xai-grok-agent/src/discovery.rs:200-204`) | Yes (path in messages, docs) |
| `~/.medley/` current state dir | shipped | `state_dir.rs:24` (`STATE_DIR_NAME`) | **Yes** — current default | — | Yes |
| Per-project `.grok/` dirs (`config.toml`, `hooks/`, `skills/`, `agents/`, `rules/`, `sandbox.toml`, `rewind-checkpoints/`, `worktrees/`) | woven through discovery/permission/sandbox code | e.g. `rg -n '"\.grok"' --type rust crates/` | **Yes** — users' repos contain `.grok/config.toml` etc. Discovery deliberately reads `.grok` alongside `.claude`/`.cursor`/`.agents` (`xai-grok-agent/src/prompt/skills.rs:180`) | Renaming the *read* side breaks interop with upstream-managed repos; keeping `.grok` as a fallback read is the cheap path | Yes (users create these) |
| `GROK_*` env vars | 3016 occurrences, 2925 lines, 409 files, **508 distinct names** | `rg -o 'GROK_[A-Z0-9_]+' --type rust . \| wc -l`; `rg -n 'GROK_' --type rust . \| wc -l`; `rg -l 'GROK_' --type rust . \| wc -l`; `rg -o 'GROK_[A-Z0-9_]+' --type rust . \| sed 's/.*://' \| sort -u \| wc -l` | **Yes** — users have `GROK_HOME`, API keys, feature toggles exported. Note the 508 includes many test-only names (`GROK_TEST_*`, hook-fixture vars); the documented user-facing subset is far smaller | A blanket rename of 508 call sites is a permanent conflict farm; a central prefix-fallback resolver touches ~1 file | Yes (documented subset) |
| `GROK_HOME` specifically | 356 occurrences | `rg -o 'GROK_HOME' --type rust . \| wc -l` | **Yes** | Already migrated: honored as compat fallback after `MEDLEY_HOME` (state_dir.rs:33) | Yes |
| `~/.grok/bin/grok` symlink + `~/.grok/downloads/` (updater/npm layout) | updater + npm postinstall | `crates/codegen/xai-grok-update/src/auto_update.rs:1444,2252-2275`; `crates/codegen/xai-grok-pager/npm/grok/bin/postinstall.js:24-27` | **Yes** — npm-installed users. The updater must keep *parsing* the legacy layout to upgrade those installs | Renaming parse targets breaks upgrades from legacy installs | Yes |
| macOS managed-preferences domain `ai.x.grok` | 1 constant + refs | `crates/codegen/xai-grok-config/src/macos_managed.rs:9` | **Yes** — enterprise MDM deployments push this domain | Renaming breaks every deployed MDM profile; it is also namespaced to xAI's product | Only to MDM admins |
| Example hook scripts writing `${HOME}/.grok/*.log` | 2 files | `crates/codegen/xai-grok-hooks/examples/hooks/bin/session-log.sh:15`, `tool-logger.sh:15` | No (examples until copied) | Low | Yes (docs-grade) |

The two PR-#154-deferred assertions (`crates/codegen/xai-grok-shell/src/config/tests.rs:3327` `validate_hooks_path_rejects_outside_grok_home` and `:3337` `..._rejects_traversal_attack`) assert `msg.contains("must be under ~/.grok/")` while production (`xai-grok-shell/src/config/mod.rs:1857-1862`) now builds that label dynamically via `xai_grok_config::display_grok_home_prefix()` — on a medley install the message says `must be under ~/.medley/ (...)`. What renaming them requires: replace the hardcoded literal in both assertions with the resolved label (build the expected string from `display_grok_home_prefix()`, the same source production uses), or assert on the stable remainder of the message (`"Hook path must be under "` + the canonical path). While there, the sibling test name `validate_hooks_path_accepts_grok_hooks_subdir` (tests.rs:3349) and the doc comment at `config/mod.rs:1822-1826` still say `~/.grok` and should be reworded to the neutral "state dir" phrasing. No production behavior changes — the guard already follows the resolved home.

## Category 4 — real xAI references (renaming breaks routing)

| What | Count | Command | Why it must stay |
|---|---|---|---|
| `api.x.ai` | 151 total, 137 in rust | `rg -o 'api\.x\.ai' . \| wc -l`; `rg -o 'api\.x\.ai' --type rust . \| wc -l` | xAI's API host; `is_xai_api_url`/`is_xai_api_bearer_url` (`xai-grok-shell-base/src/util/mod.rs:88-116`) decide where credentials are attached or refused. Renaming mis-routes tokens |
| `*.grok.com` endpoints | 294 total: 245 `grok.com`, 36 `cli-chat-proxy.grok.com`, 8 `proxy.grok.com`, 2 `computer-hub.grok.com`, 2 `code.grok.com`, 1 `assets.grok.com` | `rg -o '[a-zA-Z0-9.-]+\.grok\.com\|grok\.com' . \| sed 's/.*://' \| sort \| uniq -c \| sort -rn` | Production endpoints compiled into `xai-grok-env/src/lib.rs:23-25`; `is_cli_chat_proxy_url`/`is_prod_cli_chat_proxy_url` (util/mod.rs:62-80) are the second URL predicate pair — a security kill-switch that must keep matching the real host |
| `x.ai` substring in rust (superset) | 2099 | `rg -o 'x\.ai' --type rust . \| wc -l` | Same routing family |
| Model ids `grok-4.5` / `grok-4` / `grok-3` / `grok-3-fast` / `grok-4.3` / `grok-3-mini` | 297 / 290 / 291 / 42 / 24 / 5 | `rg -o 'grok-[0-9][a-z0-9.-]*' --type rust . \| sed 's/.*://' \| sort \| uniq -c \| sort -rn` | Server-side identifiers; the API 404s anything else. The corpus test pins `grok-4.5` surviving the doc rename |
| Upstream repo URLs | `github.com/xai-org/plugin-marketplace` 28; sync pins `xai-org/grok-build` | `rg -o 'github\.com/[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+' -g '!third_party' --type rust . \| sed 's/.*://' \| sort \| uniq -c \| sort -rn \| head`; `scripts/sync-upstream.sh:63`; `SOURCE_REV` | Real locations. The marketplace URL fetches real plugins; the sync remote is how upstream merges arrive |
| Legacy artifact names `grok-0.1.*-<platform>` | ~100 in updater/tests | same `grok-[0-9]` command above (the `grok-0.1.203`, `grok-0.1.141-macos-aarch64` rows) | The updater parses the legacy `~/.grok/downloads/grok-<ver>-<platform>` layout to upgrade old installs (auto_update.rs:1507-1508, 2268-2275); the parser must keep recognizing them |
| Process-name predicates `is_grok_process` / `is_grok_process_strict` | 2 functions, 2 call sites | `xai-grok-shell-base/src/util/mod.rs:222,277`; callers `xai-grok-pager-bin/src/main.rs:391`, `xai-grok-shell/src/leader/mod.rs:1273` | These match the string `grok` in `/proc/<pid>/cmdline` (Linux) / `comm` (macOS strict). They are not xAI references — they are heuristics keyed on *our own* binary's name, and the shipped binary is now `medley`. On Linux the leader-zombie auto-kill name-check silently never matches a medley process. This is a functional coupling to resolve (match both names, or key off something stable), not a rename — see open questions |

## Category 5 — internal identifiers no user sees

| What | Count | Command |
|---|---|---|
| `grok` (any case) in rust | 25,924 occurrences in 1,462 files | `rg -o -i 'grok' --type rust . \| wc -l`; `rg -l -i 'grok' --type rust . \| wc -l` |
| `xai` (any case) in rust | 16,237 | `rg -o -i 'xai' --type rust . \| wc -l` |
| `xai_grok_*` crate-path tokens | 7,829 | `rg -o 'xai_grok[a-z_]*' --type rust . \| wc -l` |
| `grok_home` identifier (function/field/var) | 1,422 | `rg -o 'grok_home' --type rust . \| wc -l` |

Renaming any of this is pure cost: every identifier that diverges from upstream spelling is a merge conflict on every sync, forever, and no user ever sees it. Explicit non-goal per the issue.

## Do-not-rename list (with reasons)

1. **Cargo bin target `xai-grok-pager`** — settled; every upstream sync conflicts.
2. **44 `xai-grok-*` crate names and the 325 Cargo.toml reference lines** — settled non-goal; the single largest conflict surface; invisible to users.
3. **`api.x.ai` / `*.x.ai` host checks** (`util/mod.rs:88-116`) — credential attach/refuse routing; renaming sends tokens to, or withholds them from, the wrong hosts.
4. **`cli-chat-proxy.grok.com` and the `is_*cli_chat_proxy*` predicates** (`util/mod.rs:62-80`, `xai-grok-env/src/lib.rs:23`) — remote kill-switch trust anchor; must keep matching the real host exactly.
5. **`assets.grok.com`, `code.grok.com`, `computer-hub.grok.com`, `proxy.grok.com`** (`xai-grok-env/src/lib.rs:23-25`, `xai-grok-shell-base/src/env.rs:21`) — production endpoints.
6. **Model ids `grok-4.5`, `grok-4`, `grok-3*` (≈949 occurrences)** — server-side identifiers; the API only answers to the real ids.
7. **`grok.com` links and `github.com/xai-org/*` (marketplace, sync remote, `SOURCE_REV`)** — real upstream locations; renaming breaks fetches and the sync pipeline.
8. **"Grok Build" / "a fork of Grok Build" attribution prose (117 occurrences)** — trademark-accurate attribution of xAI's product; Apache-2.0 requires it and the non-affiliation notice depends on naming the thing we are not affiliated with. The docs.rs corpus test pins its survival.
9. **`GROK_HOME` and the `GROK_*` read paths as fallback** — users have these exported today; `MEDLEY_*` must *fall back* to them, never stop reading them (state_dir.rs:33 is the pattern).
10. **Legacy `~/.grok` parsing in the updater and npm postinstall** — upgrade path for existing installs; the parser must keep recognizing the legacy layout it migrates away from.
11. **`ai.x.grok` macOS MDM domain** (`macos_managed.rs:9`) — renaming breaks deployed enterprise profiles; also genuinely xAI-namespaced. (Listed as do-not-rename by default; see open question 3.)
12. **`docs/plans/**` and `docs/audits/**`** — settled; historical records.
13. **Theme ids `groknight` / `grokday`** — config values sitting in users' `pager.toml` today; only the *display names* are free to change.

## Phased proposal (user-visible benefit ÷ upstream conflict cost)

**Phase 0 — landed.** Name (medley), packaging alias (PR #70), state-dir isolation + migration (PR #74), invoked-program plumbing (`program_name`, docs.rs runtime rename). No remaining action.

**Phase 1 — free strings (high benefit, near-zero conflict cost).** No path/env changes; nothing a user has on disk is affected.
- Notification titles: `title: "Grok"` ×4 + OSC 777 payload → the invoked program name or `medley` (notifications/mod.rs:481,496,753, hooks.rs:123, protocol.rs:82).
- Diagnostics prose: "Grok can't verify…" → program-aware (diagnostics/view.rs, fix.rs).
- Theme display names "Grok Night/Day/Desktop" → keep ids `groknight`/`grokday`, change only labels (pager-render theme/mod.rs:146-147, terminal/mod.rs:94).
- Fix the two #154-deferred assertions (tests.rs:3327,3337) to expect the dynamic label — zero production change, unblocks the suite as a gate.
- Sweep the 322 capitalized-`Grok` string-literal lines and reclassify each: chrome (rename), attribution (keep), log/test (ignore).

**Phase 2 — migration-bearing (do separately, each with compat fallback).**
- `MEDLEY_*` env namespace: **not** a 508-site rename. Add a central read helper (`MEDLEY_X` first, `GROK_X` fallback with a one-time deprecation note) and route the *documented* vars through it; leave internal/test-only `GROK_*` names alone. Cost driver is deciding the documented subset — see open question 1.
- Example hook scripts (`session-log.sh`, `tool-logger.sh`) → write under the resolved state dir.
- Decide whether the fork keeps shipping upstream's in-crate installers (`scripts/install*.sh/.ps1`, `npm/grok/`) which write `~/.grok` — keeping them unmodified is the sync-cheap option *if* fork releases never include them.
- Per-project `.grok/` dir: recommend keep reading `.grok` (interop with upstream-managed repos); adding `.medley` as a higher-precedence read is optional and is the only user-visible variant worth considering.

**Phase 3 — never.** Categories 1, 4, 5 in full; `docs/plans`/`docs/audits`; attribution prose; legacy parsers.

## Open questions for the repo owner (short list)

1. **Env-var scope:** generic `MEDLEY_*`→`GROK_*` prefix fallback for all ~508 names, or a curated documented subset? (Determines whether phase 2 is one resolver or a long tail.)
2. **Process-name predicates:** `is_grok_process*` match the literal `grok` in cmdline/comm, but the shipped binary is `medley` — on Linux the leader-zombie auto-kill name-check (leader/mod.rs:1273) silently never matches. Match both names, or key the check off something name-independent?
3. **MDM domain:** keep `ai.x.grok` (enterprise compat; it is xAI's product namespace) or dual-read a medley domain for new deployments?
4. **Upstream installers:** do fork releases ship `crates/codegen/xai-grok-pager/scripts/install*` and `npm/grok/` at all? If yes they need the medley treatment (conflict cost); if no, they stay pristine (sync-cheap).
5. **Per-project config dir:** `.grok` only (interop), or `.medley` preferred with `.grok` fallback?

## Stale items in the issue (as of df7e4f0)

- **"Pick the public name (TBD)"** — decided in the comments: **medley**.
- **Comment's "voice_probe help text names `~/.grok/config.toml`"** — no longer true: `rg -n '\.grok' crates/codegen/xai-grok-voice/` returns nothing; `voice_probe.rs:140` resolves the state dir via `state_dir::resolve_user()`.
- **Comment's npm shim path `npm/grok/bin/grok:25`** — the tree moved: it now lives at `crates/codegen/xai-grok-pager/npm/grok/` and still defaults to `~/.grok` (`bin/postinstall.js:24-27`). Item still open, path stale.
- **Scope item 4 (state-dir isolation)** — delivered per the comment and verified in `state_dir.rs`; only the call-site tail listed there remains (upstream installers, npm shims, example hooks — the voice_probe entry being the stale one).
- **Acceptance criterion "Existing fork users get a one-time migration prompt"** — implemented (`pending_migration` + `migrate_copy`, offered from `xai-grok-pager-bin/src/main.rs:2076-2085`); headless never prompts, by design.
