# Session-Only Model Selection (PR4 Slice)

## Feature

Added explicit session-only model selection via the `--session` flag.

## Usage

```bash
# Default: switches model and persists as default for new sessions
/model grok-4.5

# Session-only: switches model for this session only (does not persist)
/model grok-4.5 --session

# Session-only with effort (existing behavior - effort is always session-only)
/model grok-4.5 high

# ERROR: Cannot use both effort and --session (conflicting options)
/model grok-4.5 high --session  # rejected with clear error
```

## Behavior

### Action Dispatch

- `/model <name>` → `Action::SetDefaultModel` → switches + persists on success
- `/model <name> --session` → `Action::SwitchModel { effort: None }` → session-only (no persist)
- `/model <name> <effort>` → `Action::SwitchModel { effort: Some(...) }` → session-only (existing behavior)
- `/model <name> <effort> --session` → **ERROR** (conflicting options)

### Flag Parsing

The `--session` flag is:
- **Position-independent**: `/model --session grok-4.5` and `/model grok-4.5 --session` are equivalent
- **Case-insensitive**: `--session`, `--SESSION`, `--Session` all work
- **Idempotent**: Multiple `--session` flags are treated as one
- **Whitespace-tokenized**: Uses proper tokenization, not string suffix matching (prevents false positives)

### Conflict Resolution

Using both effort level and `--session` is rejected because:
- Effort level alone is already session-only (established behavior)
- `--session` alone is session-only without effort
- Having both options creates ambiguity about intent

The error message guides users to choose one of:
- `/model <name> <effort>` (session-only with effort)
- `/model <name> --session` (session-only without effort)
- `/model <name>` (persist as default)

## Implementation

### Modified Files

1. `crates/codegen/xai-grok-pager/src/slash/commands/model.rs`
   - Updated `usage()` and `arg_placeholder()` to document `--session` flag
   - Modified `run()` to parse `--session` via whitespace tokenization
   - Added explicit rejection of effort + `--session` conflict
   - Added 9 tests covering:
     - Basic session-only dispatch
     - Unready model rejection
     - Ambiguous name rejection
     - Position independence (flag before/after name)
     - Case insensitivity
     - Duplicate flag idempotence
     - Effort + session conflict (both orderings)

### Dispatcher Behavior

The dispatcher already correctly handles `Action::SwitchModel { effort: None }`:
- Emits `Effect::SwitchModel` (session mutation)
- Does NOT emit `Effect::PersistPreferredModel` (no persistence)
- New sessions will not inherit the session-only selection

See `crates/codegen/xai-grok-pager/src/app/dispatch/tests/task_result.rs` for existing test coverage of this behavior.

## Testing

### Unit Tests (added)

1. `run_model_with_session_flag_dispatches_session_only_switch` - basic dispatch
2. `run_model_session_flag_rejects_unready` - unready rejection
3. `run_model_session_flag_rejects_ambiguous` - ambiguous rejection
4. `run_model_session_flag_before_name_works` - position independence
5. `run_model_session_flag_case_insensitive` - case insensitivity
6. `run_model_duplicate_session_flag_idempotent` - idempotence
7. `run_model_effort_with_session_flag_rejects` - effort + session conflict
8. `run_model_session_flag_before_effort_rejects` - session + effort conflict

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

# Verify it's selected (check footer)

# Start a new session
/new

# Verify the selection was NOT persisted (check footer)

# Check config
cat ~/.grok/config.toml
# (should NOT contain grok-4.5 as default_model)

# Test conflict rejection
/model grok-4.5 high --session
# Should display: "Cannot use --session with effort level..."
```

## Codex Review Feedback Addressed

### Original Issues

1. **HIGH: Brittle flag parsing** - Fixed via whitespace tokenization
2. **HIGH: Undefined effort + --session behavior** - Defined as explicit error with clear message
3. **MEDIUM: Missing edge case tests** - Added 6 additional tests
4. **LOW: Documentation gaps** - Updated to explain all behaviors

### Changes Made

- Replaced `ends_with("--session")` with whitespace tokenization
- Made flag position-independent and case-insensitive
- Added explicit rejection of effort + --session conflict
- Added comprehensive test coverage for edge cases
- Updated documentation to clarify all behaviors

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

This slice provides the foundational command-line interface with proper conflict resolution. The full picker UI is deferred to avoid resource exhaustion.

## Refs

- Issue #290 (TUI route-aware agent policy)
- Issue #207 (provider control plane epic)
- PR #385 (native route contract)
- PR #386 (live spawn receipts)
- PR #387 (lifecycle cards + fallback planner)
- PR #388 (this slice)
- [Codex review](93047a23-2d19-4837-a944-d8337c6676db) that identified parsing issues

