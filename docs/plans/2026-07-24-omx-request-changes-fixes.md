# Grok providers Critical/Important fix plan (OMX REQUEST CHANGES)

> Execute on `providers`. Do not touch `main`. TDD where feasible. Commit atomically per theme. After all green, re-run OMX gpt-5.6-sol max review until APPROVE or only Minor.

## Critical 1 — Catalog key vs wire routing slug

**Problem:** `SetSessionModel` only carries `SamplerConfig.model` (wire slug). Duplicate slug entries: custom `none` alias loses identity; reconstruct looks up built-in Bearer.

**Fix:**
- Thread authoritative `catalog_model_id` through `SessionCommand::SetSessionModel` / `handle_set_session_model` / session actor state.
- Auth facts, readiness, BYOK, gate lookups use catalog key; wire payload uses routing slug (`entry.model` / sampling config model).
- Resume/persistence must store catalog key; ambiguous slug → fail closed.
- Subagent: prefer `ctx.model_id` exact key before slug fallback.
- Tests: switch→reconstruct→wire with shared slug built-in Bearer + custom none → no Authorization.

## Critical 2 — Invalid auth_scheme fail-closed

**Problem:** Bad `auth_scheme` pruned; entry kept; defaults to Bearer; ambient creds may leak.

**Fix:**
- Parser marks invalid auth_scheme with sentinel / validation error; model `ready=false` with reason.
- Slash/picker/dispatch fail-closed; no request.
- Do NOT auto-map invalid → None.
- Tests: parser, meta, dispatch, wire receives no request.

## Important 1 — Subagent bidirectional inherit

Remove unconditional `if parent_baseline == None { force None }`. Only use baseline when catalog+disk miss. Add reverse test: startup None → switch Bearer → child keeps Bearer.

## Important 2 — Sampler final scrub after HeaderInjector

After injector, if `AuthScheme::None`, strip Authorization/x-api-key again. Hostile injector wire test.

## Important 3 — TUI readiness gate

`model_not_ready_reason` / gate: catalog miss ≠ ready. Apply on SwitchModel, auth-confirm, deferred, dashboard spawn paths. TOCTOU tests.

## Important 4 — First-party metadata

Non-xAI / None hosts: do not send x-grok-user-id / deployment-id (and minimize correlation). Wire test. Document.

## Important 5 — Sync script + branch protection

- sync-upstream: verify remote URLs; ff-only align local providers with origin/providers.
- Document GitHub rules in FORK.md; apply via `gh` if permissions allow.

## Docs

Update `11-custom-models.md` + `FORK.md`: duplicate slug hazard, invalid auth_scheme, metadata policy.

## Ship

`cargo fmt`, clippy lib -D warnings, hot-path tests, CI green, OMX re-review APPROVE, tag `v0.0.0+providers.1` (or next N), push tag + providers.
