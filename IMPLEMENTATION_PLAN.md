# Implementation Plan: Session-Only Model Policy PR4

## Context

This is PR4 from issue #290's delivery slices, focusing on:
- Session-only vs persistent model selection
- Making the distinction explicit in the UX

## Current State Analysis

### Existing Actions
- `Action::SetDefaultModel(ModelId)` - Optimistically switches session + persists on success
- `Action::SwitchModel { model_id, effort }` - Session-only switch (no persist)

### Current Behavior
- `/model <name>` → `SetDefaultModel` → switches + persists
- `/model <name> <effort>` → `SwitchModel { effort: Some(...) }` → session-only

## Problem

The session-only path only works when specifying an effort level. There's no way to select a model for the session only without specifying effort.

## Proposed Solution

### Option 1: Add Session-Only Slash Command
Add `/model-session <name>` or `/model <name> --session` to explicitly request session-only.

**Pros:**
- Minimal changes
- Clear intent
- Easy to test

**Cons:**
- New command/flag to learn
- Less discoverable

### Option 2: Interactive Picker with Choice
When using `/model` (or Ctrl+M), show a picker that asks:
- "Use for this session only"
- "Switch and set as default"

**Pros:**
- More discoverable
- Matches #207 requirements for explicit labeling
- Better UX

**Cons:**
- More complex implementation
- Needs modal/picker UI work

### Option 3: Modify `/model` Behavior
Change `/model <name>` to be session-only by default, require explicit `--persist` flag for persistence.

**Pros:**
- Safer default (doesn't accidentally persist)
- Clear when persistence happens

**Cons:**
- Breaking change
- May confuse existing users

## Recommended Approach for This Slice

Given constraints (no Rust toolchain, resource exhaustion risk), implement **Option 1** as a minimal slice:

1. Add `SessionOnlyModelSwitch(ModelId)` action
2. Add `/model-session` command that dispatches it
3. Update dispatcher to handle it (similar to SwitchModel with effort: None)
4. Add tests

### Files to Modify

1. `crates/codegen/xai-grok-pager/src/app/actions.rs`
   - Add action variant

2. `crates/codegen/xai-grok-pager/src/slash/commands/model.rs`
   - Add session-only flag parsing or create separate command

3. `crates/codegen/xai-grok-pager/src/slash/commands/mod.rs`
   - Register new command

4. `crates/codegen/xai-grok-pager/src/app/dispatch/router.rs`
   - Handle new action

5. Add tests in `crates/codegen/xai-grok-pager/src/slash/commands/model.rs`

## Testing Requirements

- Unit tests for slash command parsing
- Dispatcher tests verifying no PersistPreferredModel effect
- E2E test showing session-only behavior
- Test that new session doesn't inherit the session-only selection

## Future Work (Option 2)

For a more complete implementation matching #207 requirements:
- Interactive picker modal
- "Use for this session" vs "Switch and set as default" buttons
- Generation-bound mutation admission
- Provider/route details in picker
- Responsive layout tests

## Non-Goals for This Slice

- Full picker UI redesign
- Provider detail integration
- Mouse interaction
- Accessibility matrix
- Large catalog performance

## Notes

Two prior agents died with resource_exhausted on this issue. This minimal slice aims to deliver concrete value without over-reaching.
