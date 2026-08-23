//! One-shot publication barrier for provisional session actors.
//!
//! A `/new` actor is expensive to build and therefore cannot hold the auth
//! selection seal across its async construction.  The actor may exist locally
//! before the final seal, but externally visible startup work must wait until
//! the sealed resident commit succeeds.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PublicationState {
    Pending,
    Published,
    Aborted,
}

#[derive(Clone, Debug)]
pub struct SessionPublicationGate {
    state: tokio::sync::watch::Sender<PublicationState>,
    session_threads: tokio::sync::watch::Sender<usize>,
}

/// Lifetime lease for an OS session thread that may still touch provisional
/// persistence. Deletion waits for every lease to drop, but request
/// cancellation never blocks an async executor on `JoinHandle::join`.
pub(crate) struct ProvisionalSessionThreadLease {
    session_threads: tokio::sync::watch::Sender<usize>,
}

impl Drop for ProvisionalSessionThreadLease {
    fn drop(&mut self) {
        self.session_threads.send_modify(|active| {
            debug_assert!(*active > 0, "session-thread lease count underflow");
            *active = active.saturating_sub(1);
        });
    }
}

impl SessionPublicationGate {
    pub(crate) fn pending() -> Self {
        let (state, _rx) = tokio::sync::watch::channel(PublicationState::Pending);
        let (session_threads, _rx) = tokio::sync::watch::channel(0);
        Self {
            state,
            session_threads,
        }
    }

    pub(crate) fn published() -> Self {
        let (state, _rx) = tokio::sync::watch::channel(PublicationState::Published);
        let (session_threads, _rx) = tokio::sync::watch::channel(0);
        Self {
            state,
            session_threads,
        }
    }

    pub(crate) fn is_published(&self) -> bool {
        *self.state.borrow() == PublicationState::Published
    }

    pub(crate) fn publish(&self) {
        self.state.send_if_modified(|state| {
            if *state != PublicationState::Pending {
                return false;
            }
            *state = PublicationState::Published;
            true
        });
    }

    pub(crate) fn abort(&self) {
        self.state.send_if_modified(|state| {
            if *state != PublicationState::Pending {
                return false;
            }
            *state = PublicationState::Aborted;
            true
        });
    }

    /// Register an OS session thread that can touch provisional storage until
    /// it exits. The returned lease must be moved into that thread before the
    /// spawn attempt; if spawning fails, dropping the closure releases it.
    pub(crate) fn register_session_thread(&self) -> ProvisionalSessionThreadLease {
        debug_assert!(
            !self.is_published(),
            "published sessions do not need provisional thread leases"
        );
        self.session_threads.send_modify(|active| {
            *active = active
                .checked_add(1)
                .expect("provisional session-thread lease count overflow");
        });
        ProvisionalSessionThreadLease {
            session_threads: self.session_threads.clone(),
        }
    }

    /// Wait until no OS session thread can still create or mutate files in the
    /// provisional directory. Persistence calls this after abort and before
    /// recursive deletion, keeping cleanup ordered without blocking the ACP
    /// executor during request-future cancellation.
    pub(crate) async fn wait_until_session_threads_exit(&self) {
        let mut session_threads = self.session_threads.subscribe();
        loop {
            if *session_threads.borrow_and_update() == 0 {
                return;
            }
            if session_threads.changed().await.is_err() {
                return;
            }
        }
    }

    /// Wait until the provisional actor may produce externally visible work.
    /// Returns `false` when publication was rejected.
    pub(crate) async fn wait_until_published(&self) -> bool {
        let mut state = self.state.subscribe();
        loop {
            match *state.borrow_and_update() {
                PublicationState::Published => return true,
                PublicationState::Aborted => return false,
                PublicationState::Pending => {}
            }
            if state.changed().await.is_err() {
                return false;
            }
        }
    }

    /// Wait until a provisional actor is rejected during initialization.
    /// Once published, the gate can never be aborted and this future remains
    /// pending so it is safe to race against actor construction.
    pub(crate) async fn wait_until_aborted(&self) {
        let mut state = self.state.subscribe();
        loop {
            match *state.borrow_and_update() {
                PublicationState::Aborted => return,
                PublicationState::Published => std::future::pending::<()>().await,
                PublicationState::Pending => {}
            }
            if state.changed().await.is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PublicationState, SessionPublicationGate};

    #[tokio::test]
    async fn pending_gate_releases_only_after_publish() {
        let gate = SessionPublicationGate::pending();
        let mut wait = Box::pin(gate.wait_until_published());
        assert!(matches!(
            futures::poll!(wait.as_mut()),
            std::task::Poll::Pending
        ));

        gate.publish();

        assert!(wait.await);
        assert!(gate.is_published());
    }

    #[tokio::test]
    async fn pending_gate_rejects_after_abort() {
        let gate = SessionPublicationGate::pending();
        let mut wait = Box::pin(gate.wait_until_published());
        assert!(matches!(
            futures::poll!(wait.as_mut()),
            std::task::Poll::Pending
        ));

        gate.abort();

        assert!(!wait.await);
        assert!(!gate.is_published());
    }

    #[tokio::test]
    async fn terminal_gate_states_cannot_be_reversed() {
        let aborted = SessionPublicationGate::pending();
        aborted.abort();
        aborted.publish();
        assert!(!aborted.wait_until_published().await);

        let published = SessionPublicationGate::pending();
        published.publish();
        published.abort();
        assert!(published.wait_until_published().await);
    }

    #[tokio::test]
    async fn provisional_cleanup_waits_for_session_thread_lease() {
        let gate = SessionPublicationGate::pending();
        let lease = gate.register_session_thread();
        let mut exited = Box::pin(gate.wait_until_session_threads_exit());
        assert!(matches!(
            futures::poll!(exited.as_mut()),
            std::task::Poll::Pending
        ));

        drop(lease);

        exited.await;
    }

    #[test]
    fn concurrent_terminal_transitions_have_one_irreversible_winner() {
        for _ in 0..64 {
            let gate = SessionPublicationGate::pending();
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
            let publish_gate = gate.clone();
            let publish_barrier = barrier.clone();
            let publisher = std::thread::spawn(move || {
                publish_barrier.wait();
                publish_gate.publish();
            });
            let abort_gate = gate.clone();
            let abort_barrier = barrier.clone();
            let aborter = std::thread::spawn(move || {
                abort_barrier.wait();
                abort_gate.abort();
            });
            barrier.wait();
            publisher.join().expect("publisher thread");
            aborter.join().expect("aborter thread");

            let winner = *gate.state.borrow();
            assert!(matches!(
                winner,
                PublicationState::Published | PublicationState::Aborted
            ));
            gate.publish();
            gate.abort();
            assert_eq!(*gate.state.borrow(), winner);
        }
    }
}
