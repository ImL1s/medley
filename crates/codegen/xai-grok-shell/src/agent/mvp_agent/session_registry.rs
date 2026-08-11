//! Per-session resources and the registry that owns them. Distinct from
//! `agent::session_registry_client`, which talks to the remote registry.
use super::*;
/// The map stays private so every caller goes through a named operation.
#[derive(Clone, Default)]
pub(super) struct SessionRegistry {
    sessions: Rc<RefCell<HashMap<acp::SessionId, SessionResources>>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UnavailableRecoverySnapshot {
    pub(crate) unavailable_model: acp::ModelId,
    pub(crate) resident_model_id: acp::ModelId,
    pub(crate) catalog_identity: Option<xai_chat_state::CatalogIdentity>,
    pub(crate) agent_name: Option<String>,
    pub(crate) revision: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnavailableModelCommitPolicy {
    ClearIfRevision(u64),
    Preserve,
}

pub(crate) enum ModelSwitchCommitOutcome {
    Committed {
        cleared_unavailable_model: Option<acp::ModelId>,
        unavailable_model_revision_matched: bool,
    },
    Superseded,
}
/// Turn activity of a resident actor. Split from [`SessionLiveState`] so a
/// reader cannot observe `Working` without a resident actor: that combination
/// was representable when liveness was a parallel field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Activity {
    Idle,
    Working,
}
/// Where a session is in its life. Each variant carries the evidence that
/// makes it true, so a reader cannot observe a combination the type forbids.
///
/// A hosted actor is a handle on [`Self::Resident`] or mid-attach on
/// [`Self::Attaching`]. [`Self::Attaching`] owns the waiter racing requests
/// wait on and parks the previous presence in `displaced` so a failed attach
/// can restore it. Independent `set_live` writes stay inside
/// [`Self::Attaching`] until the attach settles. Terminal variants still carry
/// an optional thread so `set_live` / `set_thread` cannot drop a thread the
/// sweep still owns. `handle` is `Option` so `take_session` can drop residency
/// without also rewriting live state (unload and close do that next).
pub(super) enum SessionPresence {
    /// Actor running and registered. The handle is the evidence of residency.
    Resident {
        handle: Option<SessionHandle>,
        thread: Option<SessionThread>,
        activity: Activity,
    },
    /// A load/resume is building the actor. `active` is the newest load allowed
    /// to use the during-load bypass; `waiters` keeps every in-flight load
    /// visible to external waiters until each guard drops. `displaced` is what
    /// the attach replaced and what a failed attach restores once *all*
    /// waiters settle. A handle, thread, or `settled_activity` may land here
    /// before the attach finishes; only the last [`SessionRegistry::settle_attach`]
    /// retires this variant.
    Attaching {
        /// Marker currently allowed to use the during-load bypass.
        active: Option<tokio::sync::watch::Receiver<bool>>,
        /// All in-flight load markers for waiter-side gating.
        waiters: Vec<tokio::sync::watch::Receiver<bool>>,
        displaced: Option<Box<SessionPresence>>,
        handle: Option<SessionHandle>,
        thread: Option<SessionThread>,
        /// Activity a mid-attach `set_live` asked to apply once the attach
        /// settles. Independent liveness writes must not retire this variant.
        settled_activity: Option<Activity>,
    },
    /// Client gone (`release` kept a flushing actor) with no live bit.
    Evicted {
        thread: Option<SessionThread>,
    },
    Closed {
        thread: Option<SessionThread>,
    },
    Dead {
        thread: Option<SessionThread>,
    },
    /// Idle-unloaded. Distinct from [`Self::Evicted`] because unload records
    /// `Dormant` while `release` records no live bit at all.
    Dormant {
        thread: Option<SessionThread>,
    },
}
impl SessionPresence {
    /// Roster/telemetry projection. `None` on [`Self::Evicted`]: `release`
    /// deliberately drops the live bit so a flushing thread is not reported
    /// as Dormant or Closed.
    pub(super) fn live_state(&self) -> Option<SessionLiveState> {
        match self {
            Self::Resident {
                activity: Activity::Working,
                ..
            } => Some(SessionLiveState::Working),
            Self::Resident {
                activity: Activity::Idle,
                ..
            } => Some(SessionLiveState::IdleResident),
            Self::Attaching { .. } => Some(SessionLiveState::Attaching),
            Self::Evicted { .. } => None,
            Self::Closed { .. } => Some(SessionLiveState::Completed),
            Self::Dead { .. } => Some(SessionLiveState::DeadFailed),
            Self::Dormant { .. } => Some(SessionLiveState::Dormant),
        }
    }
    fn from_live(
        state: SessionLiveState,
        thread: Option<SessionThread>,
        handle: Option<SessionHandle>,
    ) -> Self {
        match state {
            SessionLiveState::Working => Self::Resident {
                handle,
                thread,
                activity: Activity::Working,
            },
            SessionLiveState::IdleResident => Self::Resident {
                handle,
                thread,
                activity: Activity::Idle,
            },
            SessionLiveState::Attaching => {
                let (_tx, waiter) = tokio::sync::watch::channel(false);
                Self::Attaching {
                    active: Some(waiter.clone()),
                    waiters: vec![waiter],
                    displaced: None,
                    handle,
                    thread,
                    settled_activity: None,
                }
            }
            SessionLiveState::Dormant => Self::Dormant { thread },
            SessionLiveState::Completed => Self::Closed { thread },
            SessionLiveState::DeadFailed => Self::Dead { thread },
        }
    }
    fn thread(&self) -> Option<&SessionThread> {
        if let Some(thread) = self.thread_slot().as_ref() {
            return Some(thread);
        }
        match self {
            Self::Attaching {
                displaced: Some(displaced),
                ..
            } => displaced.thread(),
            _ => None,
        }
    }
    fn has_thread(&self) -> bool {
        self.thread().is_some()
    }
    fn take_thread(&mut self) -> Option<SessionThread> {
        if let Some(thread) = self.thread_slot_mut().take() {
            return Some(thread);
        }
        match self {
            Self::Attaching {
                displaced: Some(displaced),
                ..
            } => displaced.take_thread(),
            _ => None,
        }
    }
    fn replace_thread(&mut self, thread: SessionThread) -> Option<SessionThread> {
        self.thread_slot_mut().replace(thread)
    }
    fn thread_slot(&self) -> &Option<SessionThread> {
        match self {
            Self::Resident { thread, .. }
            | Self::Attaching { thread, .. }
            | Self::Evicted { thread }
            | Self::Closed { thread }
            | Self::Dead { thread }
            | Self::Dormant { thread } => thread,
        }
    }
    fn thread_slot_mut(&mut self) -> &mut Option<SessionThread> {
        match self {
            Self::Resident { thread, .. }
            | Self::Attaching { thread, .. }
            | Self::Evicted { thread }
            | Self::Closed { thread }
            | Self::Dead { thread }
            | Self::Dormant { thread } => thread,
        }
    }
    fn hosted_handle(&self) -> Option<&SessionHandle> {
        match self {
            Self::Resident { handle, .. } | Self::Attaching { handle, .. } => handle.as_ref(),
            Self::Evicted { .. }
            | Self::Closed { .. }
            | Self::Dead { .. }
            | Self::Dormant { .. } => None,
        }
    }
    fn hosted_handle_mut(&mut self) -> Option<&mut SessionHandle> {
        match self {
            Self::Resident { handle, .. } | Self::Attaching { handle, .. } => handle.as_mut(),
            Self::Evicted { .. }
            | Self::Closed { .. }
            | Self::Dead { .. }
            | Self::Dormant { .. } => None,
        }
    }
    fn take_hosted_handle(&mut self) -> Option<SessionHandle> {
        match self {
            Self::Resident { handle, .. } | Self::Attaching { handle, .. } => handle.take(),
            Self::Evicted { .. }
            | Self::Closed { .. }
            | Self::Dead { .. }
            | Self::Dormant { .. } => None,
        }
    }
    /// True when this presence holds nothing `drop_if_empty` should keep: an
    /// `Evicted` with no thread is today's `live = None, thread = None`.
    fn is_resource_empty(&self) -> bool {
        matches!(self, Self::Evicted { thread: None })
    }
}
/// The per-session state this registry owns: retained, resident resources,
/// presence (thread + liveness), unavailable model, and bridge. Load guards,
/// rewind snapshots, local workspaces, and the handle map are owned elsewhere,
/// so a new field belongs here only if `release` should drop it with the rest.
#[derive(Default)]
struct SessionResources {
    retained: Option<RetainedResources>,
    /// Cleared at idle-unload; survives a reload rebuild.
    resident: Option<ResidentResources>,
    presence: Option<SessionPresence>,
    unavailable_model: Option<acp::ModelId>,
    unavailable_catalog_identity: Option<xai_chat_state::CatalogIdentity>,
    unavailable_agent_name: Option<String>,
    unavailable_model_revision: u64,
}
#[derive(Default)]
pub(super) struct SessionCounts {
    /// Tracked ids. The per-field counts below cannot see an entry that leaks
    /// with only a field they do not name.
    pub(super) entries: usize,
    pub(super) retained_resources: usize,
    pub(super) resident_resources: usize,
    pub(super) session_threads: usize,
    pub(super) session_live_state: usize,
    pub(super) model_unavailable_sessions: usize,
    pub(super) dispatch_locks: usize,
    pub(super) session_turn_numbers: usize,
    pub(super) permission_event_receivers: usize,
    pub(super) session_index_claims: usize,
    pub(super) require_gateway_sessions: usize,
}
impl SessionRegistry {
    /// Releases everything a closing session leaves behind, in one drop.
    ///
    /// A running actor thread stays: dropping its handle would detach it, and
    /// nothing would track the memory it holds. The sweep reclaims it later.
    pub(super) fn release(&self, id: &acp::SessionId) {
        let mut entries = self.sessions.borrow_mut();
        let Some(mut released) = entries.remove(id) else {
            return;
        };
        let running = released
            .presence
            .as_mut()
            .and_then(SessionPresence::take_thread)
            .filter(|t| !t.is_finished());
        if running.is_some() {
            entries.insert(
                id.clone(),
                SessionResources {
                    presence: Some(SessionPresence::Evicted { thread: running }),
                    retained: None,
                    resident: None,
                    unavailable_model: None,
                    unavailable_catalog_identity: None,
                    unavailable_agent_name: None,
                    unavailable_model_revision: 0,
                },
            );
        }
        drop(entries);
        drop(released);
    }
    pub(super) fn set_thread(&self, id: &acp::SessionId, thread: SessionThread) {
        let displaced = self.edit(id, |e| match &mut e.presence {
            Some(presence) => presence.replace_thread(thread),
            None => {
                e.presence = Some(SessionPresence::Evicted {
                    thread: Some(thread),
                });
                None
            }
        });
        if displaced.is_some_and(|t| !t.is_finished()) {
            tracing::warn!(session_id = %id.0, "session thread displaced while still running");
        }
    }
    /// Drops the tracked thread. Returns nothing on purpose: handing a
    /// `SessionThread` to a caller lets the last handle die in a local, which
    /// detaches the thread with no record left for the sweep.
    pub(super) fn clear_thread(&self, id: &acp::SessionId) {
        self.clear(id, |e| {
            if let Some(presence) = &mut e.presence {
                let _ = presence.take_thread();
                if presence.is_resource_empty() {
                    e.presence = None;
                }
            }
        });
    }
    /// `None` when no thread is tracked for the session.
    #[cfg(test)]
    pub(super) fn has_thread(&self, id: &acp::SessionId) -> bool {
        self.with(id, |e| {
            e.presence.as_ref().is_some_and(SessionPresence::has_thread)
        })
        .unwrap_or(false)
    }
    pub(super) fn thread_is_finished(&self, id: &acp::SessionId) -> Option<bool> {
        self.with(id, |e| {
            e.presence
                .as_ref()
                .and_then(SessionPresence::thread)
                .map(SessionThread::is_finished)
        })
        .flatten()
    }
    pub(super) fn finished_threads(&self) -> Vec<acp::SessionId> {
        self.sessions
            .borrow()
            .iter()
            .filter(|(_, e)| {
                e.presence
                    .as_ref()
                    .and_then(SessionPresence::thread)
                    .is_some_and(SessionThread::is_finished)
            })
            .map(|(id, _)| id.clone())
            .collect()
    }
    pub(super) fn clear_exited_thread(&self, id: &acp::SessionId) {
        self.clear(id, |e| {
            e.presence = None;
        });
    }
    pub(super) fn set_live(&self, id: &acp::SessionId, state: SessionLiveState) {
        if state == SessionLiveState::Attaching {
            tracing::warn!(session_id = %id.0, "ignoring set_live(Attaching) outside begin_attach");
            return;
        }
        self.edit(id, |e| {
            if let Some(SessionPresence::Attaching {
                settled_activity, ..
            }) = &mut e.presence
            {
                match state {
                    SessionLiveState::Working => {
                        *settled_activity = Some(Activity::Working);
                    }
                    SessionLiveState::IdleResident => {
                        *settled_activity = Some(Activity::Idle);
                    }
                    SessionLiveState::Attaching => {}
                    SessionLiveState::Dormant
                    | SessionLiveState::Completed
                    | SessionLiveState::DeadFailed => {
                        tracing::warn!(
                            session_id = %id.0,
                            ?state,
                            "set_live ignored a terminal write against an in-flight attach"
                        );
                    }
                }
                return;
            }
            let thread = e.presence.as_mut().and_then(SessionPresence::take_thread);
            let handle = e
                .presence
                .as_mut()
                .and_then(SessionPresence::take_hosted_handle);
            e.presence = Some(SessionPresence::from_live(state, thread, handle));
        });
    }
    /// Hosted actor handle, if any. Mid-attach registration counts: the handle
    /// lands before the attach finishes, and callers already treat that as
    /// resident for lookup and sweep.
    pub(super) fn resident_handle(&self, id: &acp::SessionId) -> Option<SessionHandle> {
        self.with(id, |e| {
            e.presence
                .as_ref()
                .and_then(SessionPresence::hosted_handle)
                .cloned()
        })
        .flatten()
    }
    pub(super) fn is_resident(&self, id: &acp::SessionId) -> bool {
        self.with(id, |e| {
            e.presence
                .as_ref()
                .and_then(SessionPresence::hosted_handle)
                .is_some()
        })
        .unwrap_or(false)
    }
    pub(super) fn resident_count(&self) -> usize {
        self.sessions
            .borrow()
            .values()
            .filter(|e| {
                e.presence
                    .as_ref()
                    .and_then(SessionPresence::hosted_handle)
                    .is_some()
            })
            .count()
    }
    pub(super) fn resident_ids(&self) -> Vec<acp::SessionId> {
        self.sessions
            .borrow()
            .iter()
            .filter(|(_, e)| {
                e.presence
                    .as_ref()
                    .and_then(SessionPresence::hosted_handle)
                    .is_some()
            })
            .map(|(id, _)| id.clone())
            .collect()
    }
    pub(super) fn for_each_resident(&self, mut f: impl FnMut(&acp::SessionId, &SessionHandle)) {
        for (id, entry) in self.sessions.borrow().iter() {
            if let Some(handle) = entry
                .presence
                .as_ref()
                .and_then(SessionPresence::hosted_handle)
            {
                f(id, handle);
            }
        }
    }
    pub(super) fn for_each_resident_mut(
        &self,
        mut f: impl FnMut(&acp::SessionId, &mut SessionHandle),
    ) {
        for (id, entry) in self.sessions.borrow_mut().iter_mut() {
            if let Some(handle) = entry
                .presence
                .as_mut()
                .and_then(SessionPresence::hosted_handle_mut)
            {
                f(id, handle);
            }
        }
    }
    pub(super) fn with_resident_mut<R>(
        &self,
        id: &acp::SessionId,
        f: impl FnOnce(&mut SessionHandle) -> R,
    ) -> Option<R> {
        let mut entries = self.sessions.borrow_mut();
        let handle = entries
            .get_mut(id)?
            .presence
            .as_mut()?
            .hosted_handle_mut()?;
        Some(f(handle))
    }

    pub(super) fn commit_model_switch(
        &self,
        id: &acp::SessionId,
        expected_cmd_tx: &tokio::sync::mpsc::UnboundedSender<SessionCommand>,
        allow_attaching: bool,
        unavailable_model_policy: UnavailableModelCommitPolicy,
        update: impl FnOnce(&mut SessionHandle),
    ) -> ModelSwitchCommitOutcome {
        let mut entries = self.sessions.borrow_mut();
        let Some(entry) = entries.get_mut(id) else {
            return ModelSwitchCommitOutcome::Superseded;
        };
        let unavailable_model_revision_matched = match unavailable_model_policy {
            UnavailableModelCommitPolicy::ClearIfRevision(expected) => {
                entry.unavailable_model_revision == expected
            }
            UnavailableModelCommitPolicy::Preserve => true,
        };
        {
            let handle = match entry.presence.as_mut() {
                Some(SessionPresence::Resident {
                    handle: Some(handle),
                    ..
                }) => handle,
                Some(SessionPresence::Attaching {
                    handle: Some(handle),
                    ..
                }) if allow_attaching => handle,
                _ => return ModelSwitchCommitOutcome::Superseded,
            };
            if !handle.cmd_tx.same_channel(expected_cmd_tx) {
                return ModelSwitchCommitOutcome::Superseded;
            }
            update(handle)
        }
        let clear_unavailable_model = matches!(
            unavailable_model_policy,
            UnavailableModelCommitPolicy::ClearIfRevision(_)
        ) && unavailable_model_revision_matched;
        let unavailable_model = if clear_unavailable_model {
            entry.unavailable_model_revision = entry.unavailable_model_revision.saturating_add(1);
            entry.unavailable_catalog_identity = None;
            entry.unavailable_agent_name = None;
            entry.unavailable_model.take()
        } else {
            None
        };
        ModelSwitchCommitOutcome::Committed {
            cleared_unavailable_model: unavailable_model,
            unavailable_model_revision_matched,
        }
    }
    /// Place a handle on the current presence without changing live kind.
    /// An entry with no presence becomes [`SessionPresence::Resident`] idle.
    /// Returns the displaced handle, if any, so the caller can reap it.
    pub(super) fn put_resident(
        &self,
        id: &acp::SessionId,
        handle: SessionHandle,
    ) -> Option<SessionHandle> {
        self.edit(id, |e| match &mut e.presence {
            Some(SessionPresence::Resident {
                handle: existing, ..
            })
            | Some(SessionPresence::Attaching {
                handle: existing, ..
            }) => existing.replace(handle),
            Some(presence) => {
                let thread = presence.take_thread();
                e.presence = Some(SessionPresence::Resident {
                    handle: Some(handle),
                    thread,
                    activity: Activity::Idle,
                });
                None
            }
            None => {
                e.presence = Some(SessionPresence::Resident {
                    handle: Some(handle),
                    thread: None,
                    activity: Activity::Idle,
                });
                None
            }
        })
    }
    /// Start an attach. Current presence becomes `displaced` so a failed
    /// attach can restore it. An attach over a missing entry creates one,
    /// which the last `settle_attach` removes. A concurrent attach over an
    /// already-Attaching entry keeps every prior waiter visible and makes the
    /// new marker the sole owner of the during-load bypass.
    pub(super) fn begin_attach(
        &self,
        id: &acp::SessionId,
    ) -> (
        tokio::sync::watch::Sender<bool>,
        tokio::sync::watch::Receiver<bool>,
    ) {
        let (tx, rx) = tokio::sync::watch::channel(false);
        self.edit(id, |e| {
            let previous = e.presence.take();
            e.presence = match previous {
                Some(SessionPresence::Attaching {
                    mut waiters,
                    handle,
                    thread,
                    displaced,
                    settled_activity,
                    ..
                }) => {
                    waiters.push(rx.clone());
                    Some(SessionPresence::Attaching {
                        active: Some(rx.clone()),
                        waiters,
                        displaced,
                        handle,
                        thread,
                        settled_activity,
                    })
                }
                other => {
                    let handle = other
                        .as_ref()
                        .and_then(SessionPresence::hosted_handle)
                        .cloned();
                    Some(SessionPresence::Attaching {
                        active: Some(rx.clone()),
                        waiters: vec![rx.clone()],
                        displaced: other.map(Box::new),
                        handle,
                        thread: None,
                        settled_activity: None,
                    })
                }
            };
        });
        (tx, rx)
    }
    /// Clone of the newest in-flight attach waiter, if any. External waiters
    /// re-check via this; when concurrent loads exist the gate stays shut until
    /// every waiter has settled (see [`Self::settle_attach`]).
    pub(super) fn attach_waiter(
        &self,
        id: &acp::SessionId,
    ) -> Option<tokio::sync::watch::Receiver<bool>> {
        self.with(id, |e| match &e.presence {
            Some(SessionPresence::Attaching { waiters, .. }) => waiters.last().cloned(),
            _ => None,
        })
        .flatten()
    }
    /// Clone of the load marker currently allowed to use the during-load
    /// bypass. A superseded older load does not regain ownership when a newer
    /// marker drops — `active` is cleared, not handed back.
    pub(super) fn attach_active(
        &self,
        id: &acp::SessionId,
    ) -> Option<tokio::sync::watch::Receiver<bool>> {
        self.with(id, |e| match &e.presence {
            Some(SessionPresence::Attaching { active, .. }) => active.clone(),
            _ => None,
        })
        .flatten()
    }
    pub(super) fn is_attaching(&self, id: &acp::SessionId) -> bool {
        self.with(id, |e| {
            matches!(e.presence, Some(SessionPresence::Attaching { .. }))
        })
        .unwrap_or(false)
    }
    pub(super) fn attaching_count(&self) -> usize {
        self.sessions
            .borrow()
            .values()
            .filter(|e| matches!(e.presence, Some(SessionPresence::Attaching { .. })))
            .count()
    }
    /// Retire one in-flight attach marker. Every guard drop lands here.
    /// `waiter` identifies the dropping guard; an unknown channel is a no-op.
    /// When other waiters remain, the entry stays `Attaching` (and if this was
    /// `active`, bypass stays closed for everyone). Only the last waiter
    /// settles presence from the hosted handle or restores `displaced`.
    pub(super) fn settle_attach(
        &self,
        id: &acp::SessionId,
        waiter: &tokio::sync::watch::Receiver<bool>,
    ) {
        let mut entries = self.sessions.borrow_mut();
        let Some(entry) = entries.get_mut(id) else {
            return;
        };
        let still_waiting = match &mut entry.presence {
            Some(SessionPresence::Attaching {
                active, waiters, ..
            }) => {
                let before = waiters.len();
                waiters.retain(|rx| !rx.same_channel(waiter));
                if waiters.len() == before {
                    // Unknown / already-settled channel — no-op.
                    return;
                }
                if active.as_ref().is_some_and(|rx| rx.same_channel(waiter)) {
                    // A superseded older load must not regain bypass ownership
                    // when the newer marker drops; owner-bound bypass remains
                    // closed until a fresh begin_attach installs a new active.
                    *active = None;
                }
                !waiters.is_empty()
            }
            _ => return,
        };
        if still_waiting {
            return;
        }
        let Some(SessionPresence::Attaching {
            mut displaced,
            handle,
            thread,
            settled_activity,
            ..
        }) = entry.presence.take()
        else {
            return;
        };
        let running = handle.as_ref().map(|h| {
            h.current_prompt_id
                .lock()
                .map(|p| p.is_some())
                .unwrap_or(true)
        });
        entry.presence = if let Some(running) = running {
            let thread = match thread {
                Some(own) => {
                    if displaced
                        .as_mut()
                        .and_then(|d| d.take_thread())
                        .is_some_and(|t| !t.is_finished())
                    {
                        tracing::warn!(
                            session_id = %id.0,
                            "session thread displaced while still running"
                        );
                    }
                    Some(own)
                }
                None => displaced.as_mut().and_then(|d| d.take_thread()),
            };
            let activity = match settled_activity {
                Some(activity) => activity,
                None if running => Activity::Working,
                None => Activity::Idle,
            };
            Some(SessionPresence::Resident {
                handle,
                thread,
                activity,
            })
        } else if let Some(displaced) = displaced {
            let mut restored = match *displaced {
                SessionPresence::Attaching { thread, .. } => SessionPresence::Dormant { thread },
                SessionPresence::Resident { handle, thread, .. }
                    if handle.as_ref().is_none_or(|h| h.cmd_tx.is_closed()) =>
                {
                    SessionPresence::Dormant { thread }
                }
                other => other,
            };
            if let Some(own) = thread {
                if restored.thread_slot().is_none() {
                    let _ = restored.replace_thread(own);
                } else if !own.is_finished() {
                    tracing::warn!(
                        session_id = %id.0,
                        "session thread displaced while still running"
                    );
                }
            }
            Some(restored)
        } else {
            entry.presence = Some(SessionPresence::Evicted { thread });
            drop(entries);
            self.release(id);
            return;
        };
        drop(entries);
        self.drop_if_empty(id);
    }
    /// Drop residency only. Live state and thread stay for the caller that
    /// owns the next transition (`set_live(Dormant)`, `release`, respawn).
    pub(super) fn take_resident(&self, id: &acp::SessionId) -> Option<SessionHandle> {
        let handle = self
            .sessions
            .borrow_mut()
            .get_mut(id)
            .and_then(|e| e.presence.as_mut())
            .and_then(SessionPresence::take_hosted_handle);
        self.drop_if_empty(id);
        handle
    }
    pub(super) fn live(&self, id: &acp::SessionId) -> Option<SessionLiveState> {
        self.with(id, |e| {
            e.presence.as_ref().and_then(SessionPresence::live_state)
        })
        .flatten()
    }
    pub(super) fn clear_resident(&self, id: &acp::SessionId) {
        self.clear(id, |e| e.resident = None);
    }
    pub(super) fn set_unavailable_model(&self, id: &acp::SessionId, model: acp::ModelId) {
        self.edit(id, |e| {
            e.unavailable_model_revision = e.unavailable_model_revision.saturating_add(1);
            e.unavailable_model = Some(model);
            e.unavailable_catalog_identity = None;
            e.unavailable_agent_name = None;
        });
    }
    pub(super) fn set_unavailable_model_with_identity(
        &self,
        id: &acp::SessionId,
        model: acp::ModelId,
        catalog_identity: Option<xai_chat_state::CatalogIdentity>,
        agent_name: Option<String>,
    ) {
        self.edit(id, |e| {
            e.unavailable_model_revision = e.unavailable_model_revision.saturating_add(1);
            e.unavailable_model = Some(model);
            e.unavailable_catalog_identity = catalog_identity;
            e.unavailable_agent_name = agent_name;
        });
    }
    pub(super) fn unavailable_model(&self, id: &acp::SessionId) -> Option<acp::ModelId> {
        self.with(id, |e| e.unavailable_model.clone()).flatten()
    }
    pub(super) fn unavailable_model_revision(&self, id: &acp::SessionId) -> Option<u64> {
        self.with(id, |e| e.unavailable_model_revision)
    }
    pub(super) fn unavailable_catalog_identity(
        &self,
        id: &acp::SessionId,
    ) -> Option<xai_chat_state::CatalogIdentity> {
        self.with(id, |e| e.unavailable_catalog_identity.clone())
            .flatten()
    }
    pub(super) fn unavailable_agent_name(&self, id: &acp::SessionId) -> Option<String> {
        self.with(id, |e| e.unavailable_agent_name.clone()).flatten()
    }

    pub(super) fn unavailable_recovery_snapshot(
        &self,
        id: &acp::SessionId,
    ) -> Option<UnavailableRecoverySnapshot> {
        let entries = self.sessions.borrow();
        let entry = entries.get(id)?;
        Some(UnavailableRecoverySnapshot {
            unavailable_model: entry.unavailable_model.clone()?,
            resident_model_id: entry
                .presence
                .as_ref()?
                .hosted_handle()?
                .model_id
                .clone(),
            catalog_identity: entry.unavailable_catalog_identity.clone(),
            agent_name: entry.unavailable_agent_name.clone(),
            revision: entry.unavailable_model_revision,
        })
    }

    pub(super) fn unavailable_recovery_is_current(
        &self,
        id: &acp::SessionId,
        expected: &UnavailableRecoverySnapshot,
    ) -> bool {
        self.unavailable_recovery_snapshot(id).as_ref() == Some(expected)
    }
    pub(super) fn take_unavailable_model(&self, id: &acp::SessionId) -> Option<acp::ModelId> {
        let model = self
            .sessions
            .borrow_mut()
            .get_mut(id)
            .and_then(|e| {
                e.unavailable_model_revision = e.unavailable_model_revision.saturating_add(1);
                e.unavailable_catalog_identity = None;
                e.unavailable_agent_name = None;
                e.unavailable_model.take()
            });
        self.drop_if_empty(id);
        model
    }
    pub(super) fn turn_number(&self, id: &acp::SessionId) -> Option<u64> {
        self.with(id, |e| e.retained.as_ref()?.turn_number)
            .flatten()
    }
    pub(super) fn set_turn_number(&self, id: &acp::SessionId, next: u64) {
        self.edit(id, |e| {
            e.retained.get_or_insert_default().turn_number = Some(next);
        });
    }
    pub(super) fn dispatch_lock(&self, id: &acp::SessionId) -> Rc<tokio::sync::Mutex<()>> {
        self.edit(id, |e| {
            e.retained
                .get_or_insert_default()
                .dispatch_lock
                .get_or_insert_with(Default::default)
                .clone()
        })
    }
    pub(super) fn set_permission_receiver(
        &self,
        id: &acp::SessionId,
        rx: tokio::sync::mpsc::UnboundedReceiver<PermissionEvent>,
    ) {
        self.edit(id, |e| {
            e.retained.get_or_insert_default().permission_event_receiver = Some(rx);
        });
    }
    pub(super) fn drain_permission_events(&self, id: &acp::SessionId) -> Vec<PermissionEvent> {
        let mut events = Vec::new();
        let mut entries = self.sessions.borrow_mut();
        if let Some(rx) = entries
            .get_mut(id)
            .and_then(|e| e.retained.as_mut())
            .and_then(|r| r.permission_event_receiver.as_mut())
        {
            while let Ok(event) = rx.try_recv() {
                events.push(event);
            }
        }
        events
    }
    pub(super) fn set_codebase_index(
        &self,
        id: &acp::SessionId,
        index: std::sync::Arc<xai_codebase_graph::IndexManagerHandle>,
    ) {
        self.edit(id, |e| {
            e.resident.get_or_insert_default().codebase_index = Some(index);
        });
    }
    #[cfg(test)]
    pub(super) fn mark_require_gateway(&self, id: &acp::SessionId) {
        self.edit(id, |e| {
            e.resident.get_or_insert_default().require_gateway = true;
        });
    }
    /// Destructured so a new field has to be counted, or go unmeasured.
    pub(super) fn counts(&self) -> SessionCounts {
        let mut counts = SessionCounts::default();
        for entry in self.sessions.borrow().values() {
            counts.entries += 1;
            let SessionResources {
                retained,
                resident,
                presence,
                unavailable_model,
                unavailable_catalog_identity: _,
                unavailable_agent_name: _,
                unavailable_model_revision: _,
            } = entry;
            counts.retained_resources += usize::from(retained.is_some());
            counts.resident_resources += usize::from(resident.is_some());
            counts.session_threads +=
                usize::from(presence.as_ref().is_some_and(SessionPresence::has_thread));
            counts.session_live_state += usize::from(
                presence
                    .as_ref()
                    .and_then(SessionPresence::live_state)
                    .is_some(),
            );
            counts.model_unavailable_sessions += usize::from(unavailable_model.is_some());
            if let Some(retained) = retained {
                counts.dispatch_locks += usize::from(retained.dispatch_lock.is_some());
                counts.session_turn_numbers += usize::from(retained.turn_number.is_some());
                counts.permission_event_receivers +=
                    usize::from(retained.permission_event_receiver.is_some());
            }
            if let Some(resident) = resident {
                counts.session_index_claims += usize::from(resident.codebase_index.is_some());
                counts.require_gateway_sessions += usize::from(resident.require_gateway);
            }
        }
        counts
    }
    fn with<R>(&self, id: &acp::SessionId, f: impl FnOnce(&SessionResources) -> R) -> Option<R> {
        self.sessions.borrow().get(id).map(f)
    }
    fn edit<R>(&self, id: &acp::SessionId, f: impl FnOnce(&mut SessionResources) -> R) -> R {
        f(self.sessions.borrow_mut().entry(id.clone()).or_default())
    }
    fn clear(&self, id: &acp::SessionId, f: impl FnOnce(&mut SessionResources)) {
        {
            let mut entries = self.sessions.borrow_mut();
            let Some(entry) = entries.get_mut(id) else {
                return;
            };
            f(entry);
        }
        self.drop_if_empty(id);
    }
    fn drop_if_empty(&self, id: &acp::SessionId) {
        let mut entries = self.sessions.borrow_mut();
        if entries.get(id).is_some_and(SessionResources::is_empty) {
            entries.remove(id);
        }
    }
}
impl SessionResources {
    fn is_empty(&self) -> bool {
        let Self {
            retained,
            resident,
            presence,
            unavailable_model,
            unavailable_catalog_identity: _,
            unavailable_agent_name: _,
            unavailable_model_revision: _,
        } = self;
        let chat_vacant = true;
        let presence_vacant = match presence {
            None => true,
            Some(presence) => presence.is_resource_empty(),
        };
        retained.is_none()
            && resident.is_none()
            && presence_vacant
            && unavailable_model.is_none()
            && chat_vacant
    }
}
