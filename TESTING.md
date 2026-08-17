# Testing Plan: Session-Only Model Selection

## Prerequisites

- Rust toolchain (cargo, rustc)
- Protocol Buffers compiler (protoc)
- Git checkout of medley repository
- Working directory: `.worktrees/feat-290-picker-pr4`

## Unit Tests

Run the model command tests:

```powershell
cargo test -p xai-grok-pager --lib slash::commands::model::
```

Expected new tests:
- `run_model_with_session_flag_dispatches_session_only_switch`
- `run_model_session_flag_rejects_unready`
- `run_model_session_flag_rejects_ambiguous`

All should PASS.

## Dispatcher Tests

Existing tests should still pass (verify no regression):

```powershell
cargo test -p xai-grok-pager --lib dispatch::tests::task_result::
```

Key tests that validate session-only behavior:
- Tests checking that `SwitchModel` with `effort: None` doesn't emit `PersistPreferredModel`
- Tests verifying new sessions don't inherit session-only selections

## Lint & Format

```powershell
cargo fmt --all -- --check
cargo clippy -p xai-grok-pager --lib --tests --no-deps -- -D warnings
```

Should have no errors.

## Integration Test (Manual)

### Test 1: Basic Session-Only Selection

1. Build medley:
   ```powershell
   cargo build --release
   ```

2. Start a session:
   ```powershell
   .\target\release\medley
   ```

3. Check current model (should be default):
   ```
   # Note the current model in the footer
   ```

4. Switch model for session only:
   ```
   /model grok-4.5 --session
   ```

5. Verify switch succeeded:
   ```
   # Footer should now show grok-4.5
   # Toast/confirmation message should appear
   ```

6. Check config was NOT modified:
   ```powershell
   # Open a new terminal
   cat $env:USERPROFILE\.grok\config.toml
   # Should NOT contain grok-4.5 as default_model
   ```

7. Start a new session:
   ```
   /new
   ```

8. Verify new session uses default (NOT grok-4.5):
   ```
   # Footer should show original default model
   ```

### Test 2: Session-Only with Unready Model

1. Ensure a model is unready (e.g., missing API key):
   ```powershell
   # Temporarily remove API key
   $env:OPENAI_API_KEY = ""
   ```

2. Try session-only selection:
   ```
   /model gpt-4 --session
   ```

3. Verify error is shown:
   ```
   # Should display readiness error (e.g., "missing OPENAI_API_KEY")
   # Model should NOT switch
   ```

### Test 3: Session-Only with Ambiguous Name

1. If catalog has ambiguous names (same display name, different IDs):
   ```
   /model Shared Model --session
   ```

2. Verify ambiguity error:
   ```
   # Should display "Ambiguous model name" error
   # Should suggest using model id
   ```

### Test 4: Compare with Default Behavior

1. Switch model WITHOUT --session:
   ```
   /model grok-4.5
   ```

2. Check config WAS modified:
   ```powershell
   cat $env:USERPROFILE\.grok\config.toml
   # Should contain grok-4.5 as default_model (after successful switch)
   ```

3. Start new session:
   ```
   /new
   ```

4. Verify new session inherits persisted default:
   ```
   # Footer should show grok-4.5
   ```

## Expected Results

### Pass Criteria

- [ ] All unit tests pass
- [ ] All existing dispatcher tests pass
- [ ] Lint/clippy clean
- [ ] Test 1: Session-only switch works, doesn't persist
- [ ] Test 2: Unready models are rejected
- [ ] Test 3: Ambiguous names are rejected
- [ ] Test 4: Default behavior still persists correctly

### Failure Modes to Check

- Config corruption (malformed TOML after switch)
- Race condition (concurrent model switches)
- Memory leak (session-only state not cleaned up)
- Rollback failure (switch fails, but state is mutated)

## CI Filter Update

Add new tests to `.github/workflows/ci.yml`:

```yaml
# Model command session-only tests
- cargo test -p xai-grok-pager --lib slash::commands::model::run_model_with_session_flag_dispatches_session_only_switch
- cargo test -p xai-grok-pager --lib slash::commands::model::run_model_session_flag_rejects_unready
- cargo test -p xai-grok-pager --lib slash::commands::model::run_model_session_flag_rejects_ambiguous
```

Or use the existing filter pattern if it already matches:
```yaml
- cargo test -p xai-grok-pager --lib slash::commands::model::
```

## Validation Script

Check new tests are covered by CI filter:

```powershell
python scripts/check_new_tests_are_filtered.py --workflow .github/workflows/ci.yml --base origin/feat/290-tui-lifecycle-fallback
```

Should report the 3 new tests are covered.

## Documentation

After tests pass, update:
1. `docs/cli-reference.md` - document `/model --session` flag
2. `docs/user-guide.md` - explain session vs persistent model selection
3. `CHANGELOG.md` - add entry for this feature

## Notes

- This is a focused slice to avoid resource exhaustion
- Full picker UI (modal, mouse, accessibility) deferred to future PRs
- Provider integration (#207) deferred to future PRs
- Generation-bound mutation deferred to future PRs
