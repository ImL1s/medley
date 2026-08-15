# Session-Only Model Selection (PR4 Slice)

## Feature

Added explicit session-only model selection via the `--session` flag.

## Usage

```bash
# Default: switches model and persists as default for new sessions
/model grok-4.5

# Session-only: switches model for this session only (does not persist)
/model grok-4.5 --session

# Session-only with effort is already session-only (no change)
/model grok-4.5 high
```

## Behavior

- `/model <name>` → `Action::SetDefaultModel` → switches + persists on success
- `/model <name> --session` → `Action::SwitchModel { effort: None }` → session-only (no persist)
- `/model <name> <effort>` → `Action::SwitchModel { effort: Some(...) }` → session-only (existing behavior)

## Implementation

### Modified Files

1. `crates/codegen/xai-grok-pager/src/slash/commands/model.rs`
   - Updated `usage()` and `arg_placeholder()` to document `--session` flag
   - Modified `run()` to parse `--session` and dispatch appropriate action
   - Added 3 tests: basic session-only, unready rejection, ambiguous rejection

### Dispatcher Behavior

The dispatcher already correctly handles `Action::SwitchModel { effort: None }`:
- Emits `Effect::SwitchModel` (session mutation)
- Does NOT emit `Effect::PersistPreferredModel` (no persistence)
- New sessions will not inherit the session-only selection

See `crates/codegen/xai-grok-pager/src/app/dispatch/tests/task_result.rs` for existing test coverage of this behavior.

## Testing

### Unit Tests (added)
- `run_model_with_session_flag_dispatches_session_only_switch` - verifies `--session` dispatches `SwitchModel`
- `run_model_session_flag_rejects_unready` - verifies unready models are blocked
- `run_model_session_flag_rejects_ambiguous` - verifies ambiguous names are rejected

### Integration Tests (existing)
The dispatcher tests already cover `Action::SwitchModel { effort: None }` behavior:
- No `PersistPreferredModel` effect is emitted
- Session-only switches don't affect config
- New sessions don't inherit session-only selections

### Manual Testing (required - no Rust toolchain available)

```bash
# Start a session
grok

# Select a model for session only
/model grok-4.5 --session

# Verify it's selected
# (model should be active in current session)

# Start a new session
/new

# Verify the selection was NOT persisted
# (new session should use default, not grok-4.5)

# Check config
cat ~/.grok/config.toml
# (should NOT contain grok-4.5 as default_model)
```

## Future Work

From #290 PR4 acceptance criteria:
- Interactive picker modal with "Use for this session" vs "Switch and set as default" buttons
- Generation-bound mutation admission
- Provider/route detail integration
- Responsive layout tests
- Accessibility matrix

From #207 picker requirements:
- Session-only vs persistent actions must be separate and explicitly labeled
- Material route changes show persistence intent
- Route/readiness revalidated after confirmation

This slice provides the foundational command-line interface. The full picker UI is deferred to avoid resource exhaustion.

## Refs

- Issue #290 (TUI route-aware agent policy)
- Issue #207 (provider control plane epic)
- PR #385 (native route contract)
- PR #386 (live spawn receipts)
- PR #387 (lifecycle cards + fallback planner)
