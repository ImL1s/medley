//! Leader-side emission of the durable `TurnCompleted` terminal.
//!
//! These drive the SHIPPED handlers (`handle_completion` for normal + error
//! completions, `cancel_running_task` for cancellation) and assert on the
//! notification the real `send_xai_notification` persists — not a
//! re-implementation. The terminal is the persisted + replayed twin of the
//! fire-and-forget `prompt_complete`, so a re-attaching viewer can finalize
//! from replay.

use super::support::*;
use super::*;

use tokio::sync::mpsc;

/// Drain every persistence message queued so far.
fn drain_persistence(rx: &mut mpsc::UnboundedReceiver<PersistenceMsg>) -> Vec<PersistenceMsg> {
    let mut out = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        out.push(msg);
    }
    out
}

fn is_turn_completed(m: &PersistenceMsg) -> bool {
    matches!(
        m,
        PersistenceMsg::Update(crate::session::storage::SessionUpdate::Xai(n))
            if matches!(n.update, XaiSessionUpdate::TurnCompleted { .. })
    )
}

fn is_agent_message_delta(m: &PersistenceMsg) -> bool {
    matches!(
        m,
        PersistenceMsg::Update(crate::session::storage::SessionUpdate::Acp(n))
            if matches!(n.update, acp::SessionUpdate::AgentMessageChunk(_))
    )
}

/// Pull the `(prompt_id, stop_reason, agent_result)` of the first persisted
/// `TurnCompleted`, if any.
fn turn_completed_fields(msgs: &[PersistenceMsg]) -> Option<(String, String, Option<String>)> {
    msgs.iter().find_map(|m| match m {
        PersistenceMsg::Update(crate::session::storage::SessionUpdate::Xai(n)) => match &n.update {
            XaiSessionUpdate::TurnCompleted {
                prompt_id,
                stop_reason,
                agent_result,
                ..
            } => Some((prompt_id.clone(), stop_reason.clone(), agent_result.clone())),
            _ => None,
        },
        _ => None,
    })
}

/// A minimal front pending input matching `prompt_id`. `queue_meta` is `None`
/// so `handle_completion` does not also broadcast a `queue/changed`. The
/// completion receiver is returned so the caller keeps it alive.
fn pending_input(prompt_id: &str) -> (InputItem, oneshot::Receiver<PromptTurnResult>) {
    let (respond_to, rx) = oneshot::channel();
    let item = InputItem {
        prompt_id: prompt_id.to_string(),
        prompt_blocks: vec![],
        prompt_mode: PromptMode::Agent,
        trace_gcs_config: None,
        artifact_tracker: None,
        client_identifier: None,
        screen_mode: None,
        verbatim: false,
        json_schema: None,
        origin: crate::session::PromptOrigin::User,
        task_wake_fallback: None,
        tool_overrides_update: None,
        respond_to,
        persist_ack: None,
        parsed_prompt_tx: None,
        queue_meta: None,
        send_now: false,
    };
    (item, rx)
}

fn agent_msg_update(text: &str) -> acp::SessionUpdate {
    acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
        acp::TextContent::new(text.to_string()),
    )))
}

#[tokio::test(flavor = "current_thread")]
async fn normal_completion_persists_turn_completed_after_buffered_delta_flush() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            // Buffering enabled with a long window so a streamed delta is HELD
            // in the replay buffer until an explicit flush — the exact state the
            // actor loop is in when a turn's completion arrives.
            let (mut actor, mut event_rx) =
                create_test_actor_ex(0, 256_000, 85, gateway_tx, persistence_tx).await;
            actor.buffering_settings = Some(BufferingSettings {
                max_items: 100,
                max_bytes: 1_000_000,
                max_duration_ms: 3_600_000,
            });

            // A running turn with its prompt queued at the front.
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("p1".to_string());
            let (item, _rx) = pending_input("p1");
            {
                let mut state = actor.state.lock().await;
                state.pending_inputs.push_back(item);
            }

            // Stream the turn's last delta through the BUFFERED path: `send_update`
            // enqueues it on `event_tx`, and the replay buffer merges/HOLDS it —
            // it is NOT persisted yet.
            actor
                .send_update(agent_msg_update("last delta"), Some(1))
                .await;

            // Mirror the actor-owned replay buffer: drain the queued event(s)
            // into it so the delta is stranded exactly as it is when a completion
            // reaches `run_session`'s completion branch.
            let mut replay_buffer = ReplayBuffer::new(actor.buffering_settings.clone());
            while let Ok(event) = event_rx.try_recv() {
                if let SessionEvent::Notification(notification) = event {
                    let _ = replay_buffer.consume_chunk(notification);
                }
            }
            assert!(
                persistence_rx.try_recv().is_err(),
                "the buffered delta must not be persisted before the flush"
            );

            // This is the exact flush `run_session`'s completion branch performs
            // before calling `handle_completion` (mirrors the Cancel/Shutdown
            // arms). Removing it leaves the held delta stranded and the terminal
            // would be the only persisted update — the ordering this PR fixes.
            if let Some(notification) = replay_buffer.flush() {
                actor.emit_buffered(notification).await;
            }

            actor
                .handle_completion(
                    "p1".to_string(),
                    Ok(PromptTurnOk {
                        stop_reason: acp::StopReason::EndTurn,
                        total_tokens: 0,
                        turn_snapshot: None,
                        completion_kind: PromptCompletionKind::Completed,
                        structured_output: None,
                        usage: None,
                        tool_overrides: None,
                    }),
                )
                .await;

            let msgs = drain_persistence(&mut persistence_rx);

            // The terminal is persisted with the right fields...
            let (prompt_id, stop_reason, agent_result) = turn_completed_fields(&msgs)
                .expect("a normal completion must persist a TurnCompleted");
            assert_eq!(prompt_id, "p1");
            assert_eq!(stop_reason, "end_turn");
            assert_eq!(agent_result, None);

            // ...after the flushed buffered delta on the same persistence stream.
            let delta_idx = msgs
                .iter()
                .position(is_agent_message_delta)
                .expect("the flushed buffered delta must be persisted");
            let terminal_idx = msgs
                .iter()
                .position(is_turn_completed)
                .expect("the terminal must be persisted");
            assert!(
                delta_idx < terminal_idx,
                "TurnCompleted must land in updates.jsonl after the flushed buffered delta"
            );

            // Limitation: this drives `handle_completion` directly and mirrors
            // the completion-branch flush rather than driving the full
            // `run_session` loop (injecting a real completion would need a
            // mock-model turn). The negative case — a buffered delta NEVER
            // reaching persistence without the flush — is covered by
            // `buffered_chunk_does_not_reach_persistence_without_explicit_flush`
            // in `replay_buffer_send_update_tests`.
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn error_completion_persists_turn_completed_with_error_detail() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("p-err".to_string());
            let (item, _rx) = pending_input("p-err");
            {
                let mut state = actor.state.lock().await;
                state.pending_inputs.push_back(item);
            }

            actor
                .handle_completion(
                    "p-err".to_string(),
                    Err(acp::Error::internal_error().data("boom")),
                )
                .await;

            let msgs = drain_persistence(&mut persistence_rx);
            let (prompt_id, stop_reason, agent_result) = turn_completed_fields(&msgs)
                .expect("a failed completion must persist a TurnCompleted");
            assert_eq!(prompt_id, "p-err");
            assert_eq!(stop_reason, "error");
            assert_eq!(agent_result.as_deref(), Some("boom"));
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_persists_turn_completed_cancelled() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            // A running turn in flight.
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("running".to_string());
            let (item, _rx) = pending_input("running");
            {
                let mut state = actor.state.lock().await;
                state.running_task = Some(AgentTask {
                    prompt_id: "running".into(),
                    handle: tokio::task::spawn_local(async {
                        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    })
                    .abort_handle(),
                });
                state.pending_inputs.push_back(item);
            }

            actor
                .cancel_running_task(true, false, false, Some("ctrl_c".to_string()))
                .await;

            let msgs = drain_persistence(&mut persistence_rx);
            let (prompt_id, stop_reason, agent_result) =
                turn_completed_fields(&msgs).expect("a cancel must persist a TurnCompleted");
            assert_eq!(prompt_id, "running");
            assert_eq!(stop_reason, "cancelled");
            assert_eq!(agent_result, None);
            // Ctrl+C also stamps a trigger (informational; only send_now changes client behavior).
            assert_eq!(
                turn_completed_meta(&msgs).and_then(|m| m
                    .get("cancelTrigger")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)),
                Some("ctrl_c".to_string()),
                "a Ctrl+C cancel stamps its trigger on the terminal meta"
            );
        })
        .await;
}

/// Pull the first persisted `TurnCompleted`'s notification `_meta`, if any.
fn turn_completed_meta(msgs: &[PersistenceMsg]) -> Option<serde_json::Value> {
    msgs.iter().find_map(|m| match m {
        PersistenceMsg::Update(crate::session::storage::SessionUpdate::Xai(n))
            if matches!(n.update, XaiSessionUpdate::TurnCompleted { .. }) =>
        {
            n.meta.clone()
        }
        _ => None,
    })
}

/// Completion-race window: `current_prompt_id` is already cleared (scope-guard
/// drop / `handle_completion` ran first) while the finished front and its task
/// slot are still queued. The cancel identity must come from
/// `running_task.prompt_id`, so the durable `TurnCompleted` (with
/// `cancelTrigger=send_now`) is still persisted — otherwise viewers strand on
/// "Waiting…" with no terminal at all.
#[tokio::test(flavor = "current_thread")]
async fn send_now_cancel_in_completion_race_window_still_persists_turn_completed() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            // The pin is already cleared — only the task slot knows the turn.
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = None;
            let (item, _rpc_rx) = pending_input("running");
            {
                let mut state = actor.state.lock().await;
                state.running_task = Some(AgentTask {
                    prompt_id: "running".into(),
                    handle: tokio::task::spawn_local(async {
                        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    })
                    .abort_handle(),
                });
                state.pending_inputs.push_back(item);
            }

            let mut replay_buffer = ReplayBuffer::new(None);
            actor.cancel_turn_for_send_now(&mut replay_buffer).await;

            let msgs = drain_persistence(&mut persistence_rx);
            let (prompt_id, stop_reason, _) = turn_completed_fields(&msgs)
                .expect("a send-now cancel with a cleared pin must still persist a TurnCompleted");
            assert_eq!(prompt_id, "running");
            assert_eq!(stop_reason, "cancelled");
            let meta = turn_completed_meta(&msgs).expect("terminal must carry _meta");
            assert_eq!(
                meta.get("cancelTrigger").and_then(|v| v.as_str()),
                Some("send_now"),
            );
        })
        .await;
}

/// A send-now cancel stamps `_meta.cancelTrigger == "send_now"` on both the
/// durable `TurnCompleted` terminal and the cancelled turn's resolved RPC.
#[tokio::test(flavor = "current_thread")]
async fn send_now_cancel_stamps_cancel_trigger_on_turn_end() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, mut gateway_rx) =
                mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("running".to_string());
            let (item, rpc_rx) = pending_input("running");
            {
                let mut state = actor.state.lock().await;
                state.running_task = Some(AgentTask {
                    prompt_id: "running".into(),
                    handle: tokio::task::spawn_local(async {
                        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    })
                    .abort_handle(),
                });
                state.pending_inputs.push_back(item);
            }

            // The shipped send-now cancel path.
            let mut replay_buffer = ReplayBuffer::new(None);
            actor.cancel_turn_for_send_now(&mut replay_buffer).await;

            let msgs = drain_persistence(&mut persistence_rx);
            let (prompt_id, stop_reason, _) = turn_completed_fields(&msgs)
                .expect("a send-now cancel must persist a TurnCompleted");
            assert_eq!(prompt_id, "running");
            assert_eq!(stop_reason, "cancelled");
            let meta = turn_completed_meta(&msgs).expect("terminal must carry _meta");
            assert_eq!(
                meta.get("cancelTrigger").and_then(|v| v.as_str()),
                Some("send_now"),
                "the terminal `_meta` must carry cancelTrigger=send_now"
            );

            let mut wire_meta = None;
            while let Ok(msg) = gateway_rx.try_recv() {
                if let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg
                    && args.request.method.as_ref() == "x.ai/session_notification"
                    && let Ok(v) =
                        serde_json::from_str::<serde_json::Value>(args.request.params.get())
                    && v["update"]["sessionUpdate"] == "turn_completed"
                {
                    wire_meta = Some(v["_meta"].clone());
                }
            }
            let wire_meta = wire_meta.expect("the TurnCompleted terminal must reach the wire");
            assert_eq!(
                wire_meta["cancelTrigger"], "send_now",
                "wire `_meta.cancelTrigger` must be send_now"
            );

            let result = rpc_rx.await.expect("running turn RPC must resolve");
            match result {
                Ok(PromptTurnOk {
                    stop_reason: acp::StopReason::Cancelled,
                    completion_kind: PromptCompletionKind::Cancelled { context, .. },
                    ..
                }) => {
                    assert_eq!(
                        context.and_then(|c| c.trigger).as_deref(),
                        Some("send_now"),
                        "the running turn's completion context must carry the send-now trigger"
                    );
                }
                other => panic!("expected a Cancelled completion, got {other:?}"),
            }
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn pristine_rewind_cancel_emits_no_turn_completed() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            // A pristine, rewindable in-flight turn at the front of the queue.
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("rw".to_string());
            let (item, _rx) = pending_input("rw");
            {
                let mut state = actor.state.lock().await;
                state.rewindable = true;
                state.running_task = Some(AgentTask {
                    prompt_id: "rw".into(),
                    handle: tokio::task::spawn_local(async {
                        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    })
                    .abort_handle(),
                });
                state.pending_inputs.push_back(item);
            }

            // rewind_if_pristine = true on a rewindable turn takes the rewind
            // path: the turn is treated as UNSENT, so — in lock-step with the
            // legacy emit_turn_ended — NO durable terminal is emitted (else
            // replay would finalize a turn that was rewound, not completed).
            actor.cancel_running_task(false, false, true, None).await;

            let msgs = drain_persistence(&mut persistence_rx);
            assert!(
                turn_completed_fields(&msgs).is_none(),
                "a pristine rewind cancel treats the turn as unsent and must persist no TurnCompleted"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn removed_from_queue_completion_emits_no_turn_completed() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("p-removed".to_string());
            let (item, _rx) = pending_input("p-removed");
            {
                let mut state = actor.state.lock().await;
                state.pending_inputs.push_back(item);
            }

            // A removed queued prompt never started a turn — it must emit no
            // durable terminal even though it resolves with `Cancelled`.
            actor
                .handle_completion(
                    "p-removed".to_string(),
                    Ok(PromptTurnOk {
                        stop_reason: acp::StopReason::Cancelled,
                        total_tokens: 0,
                        turn_snapshot: None,
                        completion_kind: PromptCompletionKind::RemovedFromQueue,
                        structured_output: None,
                        usage: None,
                        tool_overrides: None,
                    }),
                )
                .await;

            let msgs = drain_persistence(&mut persistence_rx);
            assert!(
                turn_completed_fields(&msgs).is_none(),
                "a RemovedFromQueue completion never ran a turn and must emit no terminal"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_prompt_completion_emits_no_turn_completed() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            // Reproduce the cancel race: the Cancel path already finalized and
            // dequeued the turn (current_prompt_id cleared, queue empty), so a
            // stale `(prompt, EndTurn)` completion now lands on the unknown-prompt
            // branch of handle_completion. It must NOT emit a second terminal —
            // the Cancel path already emitted TurnCompleted{cancelled} for it.
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = None;

            actor
                .handle_completion(
                    "already-finalized".to_string(),
                    Ok(PromptTurnOk {
                        stop_reason: acp::StopReason::EndTurn,
                        total_tokens: 0,
                        turn_snapshot: None,
                        completion_kind: PromptCompletionKind::Completed,
                        structured_output: None,
                        usage: None,
                        tool_overrides: None,
                    }),
                )
                .await;

            let msgs = drain_persistence(&mut persistence_rx);
            assert!(
                turn_completed_fields(&msgs).is_none(),
                "a stale completion for a prompt the Cancel path already finalized must NOT \
                 emit a second TurnCompleted (the double-emit bug)"
            );
        })
        .await;
}

/// #44 persist-then-replay evidence for the **subagent / export** path
/// (`stream_replay_updates_at` → `ReplayUpdate`), **not** main-session resume
/// (`forward_raw_replay_line`).
///
/// Requirements:
/// 1. File written by the real `SessionActor` emit path (not hand-built JSONL).
/// 2. Read back through `stream_replay_updates_at` (production subagent entry).
/// 3. Assert on the final derived agent-text (consumer mirrors subagent apply).
/// 4. Raw `updates.jsonl` contains "hello" twice; replayed result once.
/// 5. Path covered: subagent inherited replay / export only.
///
/// Removing production `AttemptDiscarded` forwarding in
/// `for_each_replay_update_in_file` (or the consumer discard) makes (4b) fail
/// with `hellohello`.
#[tokio::test(flavor = "current_thread")]
async fn attempt_discarded_persist_then_stream_replay_subagent_path() {
    use crate::session::info::Info;
    use crate::session::persistence::default_model_id;
    use crate::session::storage::{
        JsonlStorageAdapter, ReplayUpdate, StorageAdapter, stream_replay_updates_at,
    };
    use tempfile::TempDir;

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            // Real writer path: SessionActor emission (same as production drain).
            let agent = |text: &str| {
                acp::SessionNotification::new(
                    actor.session_info.id.clone(),
                    agent_msg_update(text),
                )
            };
            actor.emit_notification_direct(agent("hello")).await;
            actor
                .emit_buffered(SessionNotification::Xai(Box::new(
                    crate::extensions::notification::SessionNotification {
                        session_id: actor.session_info.id.clone(),
                        update: XaiSessionUpdate::AttemptDiscarded,
                        meta: None,
                    },
                )))
                .await;
            actor.emit_notification_direct(agent("hello")).await;

            let msgs = drain_persistence(&mut persistence_rx);
            let updates: Vec<_> = msgs
                .into_iter()
                .filter_map(|m| match m {
                    PersistenceMsg::Update(u) => Some(u),
                    _ => None,
                })
                .collect();
            assert!(
                updates.iter().any(|u| matches!(
                    u,
                    crate::session::storage::SessionUpdate::Xai(n)
                        if matches!(n.update, XaiSessionUpdate::AttemptDiscarded)
                )),
                "fixture must include a real AttemptDiscarded write; updates={updates:?}"
            );

            // Persist through the production JSONL adapter (not hand-rolled lines).
            let grok_home = TempDir::new().unwrap();
            let info = Info {
                id: actor.session_info.id.clone(),
                cwd: actor.session_info.cwd.clone(),
            };
            let adapter = JsonlStorageAdapter::with_root(grok_home.path().to_path_buf());
            adapter
                .init_session(&info, default_model_id())
                .await
                .unwrap();
            for u in &updates {
                adapter.append_update(&info, u).await.unwrap();
            }

            let session_dir = grok_home
                .path()
                .join("sessions")
                .join(crate::util::grok_home::encode_cwd_dirname(&info.cwd))
                .join(info.id.to_string());
            let updates_path = session_dir.join("updates.jsonl");
            let raw = std::fs::read_to_string(&updates_path).unwrap();
            // (a) both attempts really on disk (fixture is load-bearing).
            let naive_hello = raw.matches("hello").count();
            assert!(
                naive_hello >= 2,
                "raw updates.jsonl must contain abandoned+retry text twice; got {naive_hello} in:\n{raw}"
            );
            assert!(
                raw.contains("attempt_discarded"),
                "raw updates.jsonl must contain the retraction; raw:\n{raw}"
            );

            // (b) subagent path: stream_replay_updates_at + discard consumer
            // mirrors `replay_inherited_updates` (Acp → handle_update,
            // AttemptDiscarded → discard_streamed_attempt).
            let mut attempt = String::new();
            let emission = stream_replay_updates_at(info.id.0.as_ref(), grok_home.path(), |u| {
                match u {
                    ReplayUpdate::Acp(acp::SessionUpdate::AgentMessageChunk(chunk)) => {
                        if let acp::ContentBlock::Text(t) = &chunk.content {
                            attempt.push_str(&t.text);
                        }
                    }
                    ReplayUpdate::AttemptDiscarded => {
                        attempt.clear();
                    }
                    ReplayUpdate::Acp(_) => {}
                }
            })
            .unwrap();
            assert_eq!(
                emission,
                crate::session::storage::ReplayEmission::Emitted
            );
            assert_eq!(
                attempt, "hello",
                "subagent-path replay must show retry text once, not both attempts"
            );
            assert_ne!(
                attempt, "hellohello",
                "regression: type barrier dropped AttemptDiscarded and both attempts rendered"
            );

            // Same disk file: rebuild_chat_history (model context) also once.
            let chat_path = session_dir.join("chat_history.jsonl");
            let _ = std::fs::remove_file(&chat_path);
            let loaded = adapter.load_session(&info).await.unwrap();
            let assistant: Vec<String> = loaded
                .chat_history
                .iter()
                .filter_map(|item| match item {
                    crate::sampling::ConversationItem::Assistant(a) => {
                        Some(a.content.to_string())
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(
                assistant,
                vec!["hello".to_string()],
                "rebuild_chat_history must also retract; got {assistant:?}"
            );
        })
        .await;
}

/// #44 F1: `emit_buffered(AttemptDiscarded)` must persist the retraction
/// (with eventId) so resume/rebuild can drop abandoned attempt text.
/// Mirrors the run_loop path after the ReplayBuffer drop-without-flush.
#[tokio::test(flavor = "current_thread")]
async fn attempt_discarded_emit_buffered_persists_with_event_id() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            let notification = SessionNotification::Xai(Box::new(
                crate::extensions::notification::SessionNotification {
                    session_id: actor.session_info.id.clone(),
                    update: XaiSessionUpdate::AttemptDiscarded,
                    meta: None,
                },
            ));
            actor.emit_buffered(notification).await;

            let msgs = drain_persistence(&mut persistence_rx);
            let discard = msgs.iter().find_map(|m| match m {
                PersistenceMsg::Update(crate::session::storage::SessionUpdate::Xai(n))
                    if matches!(n.update, XaiSessionUpdate::AttemptDiscarded) =>
                {
                    Some(n)
                }
                _ => None,
            });
            let n = discard.expect("AttemptDiscarded must be persisted via emit_buffered");
            let event_id = n
                .meta
                .as_ref()
                .and_then(|m| m.get("eventId"))
                .and_then(|v| v.as_str());
            assert!(
                event_id.is_some_and(|id| !id.is_empty()),
                "AttemptDiscarded must carry eventId for resume dedup; meta={:?}",
                n.meta
            );
        })
        .await;
}

/// #44 run_loop contract: on AttemptDiscarded, drop the ReplayBuffer pending
/// slot WITHOUT flushing its text to clients/persistence, then emit the
/// retraction (which does persist). Without the drop, consume_chunk would
/// force-flush abandoned agent text before the marker.
#[tokio::test(flavor = "current_thread")]
async fn attempt_discarded_run_loop_drops_buffered_chunks_then_persists_marker() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let (mut actor, mut event_rx) =
                create_test_actor_ex(0, 256_000, 85, gateway_tx, persistence_tx).await;
            actor.buffering_settings = Some(BufferingSettings {
                max_items: 100,
                max_bytes: 1_000_000,
                max_duration_ms: 3_600_000,
            });

            // Stream abandoned attempt text into the buffered path.
            actor.send_update(agent_msg_update("hello"), Some(1)).await;
            actor
                .send_buffered_xai_update(XaiSessionUpdate::AttemptDiscarded)
                .await;

            // Drain the event pipeline the same way run_session's special-case does.
            let mut replay_buffer = ReplayBuffer::new(actor.buffering_settings.clone());
            while let Ok(event) = event_rx.try_recv() {
                if let SessionEvent::Notification(notification) = event {
                    if matches!(
                        &notification,
                        SessionNotification::Xai(n)
                            if matches!(n.update, XaiSessionUpdate::AttemptDiscarded)
                    ) {
                        let dropped = replay_buffer.flush();
                        assert!(
                            dropped.is_some(),
                            "fixture must have buffered abandoned text to drop"
                        );
                        // Do NOT emit dropped chunks — that is the bug this
                        // special-case exists to prevent.
                        actor.emit_buffered(notification).await;
                        continue;
                    }
                    if let Some((first, second)) = replay_buffer.consume_chunk(notification) {
                        actor.emit_buffered(first).await;
                        if let Some(second) = second {
                            actor.emit_buffered(second).await;
                        }
                    }
                }
            }

            let msgs = drain_persistence(&mut persistence_rx);
            let has_agent = msgs.iter().any(is_agent_message_delta);
            assert!(
                !has_agent,
                "run_loop drop path must not persist abandoned agent text; msgs={msgs:?}"
            );
            let has_discard = msgs.iter().any(|m| {
                matches!(
                    m,
                    PersistenceMsg::Update(crate::session::storage::SessionUpdate::Xai(n))
                        if matches!(n.update, XaiSessionUpdate::AttemptDiscarded)
                )
            });
            assert!(
                has_discard,
                "retraction must still be persisted; msgs={msgs:?}"
            );
        })
        .await;
}
