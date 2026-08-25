//! Test-only pin for the process state directory resolved by
//! [`crate::grok_home`].
//!
//! Fork-owned (#420). [`crate::grok_home`] caches its answer in a process-wide
//! `OnceLock` so the whole process agrees on one directory. That is right for
//! production and wrong for a test binary: the first test to resolve the home
//! decides it for all 6000+ siblings, and a later test setting `MEDLEY_HOME` /
//! `GROK_HOME` through an env guard changes nothing. Such a test silently reads
//! and writes the *developer's* live state directory instead of its fixture,
//! which is how six `agent::mvp_agent` tests came to pass alone, pass on a
//! fresh CI container, and fail on any machine that had run them before.
//!
//! The pin is thread-local and the guard is `!Send`, mirroring
//! `CodexAuthPathGuard` (#343): a test's own thread gets its own answer without
//! perturbing tests running concurrently beside it.
//!
//! **The override check is deliberately not `#[cfg(test)]`.** `cfg(test)` is
//! true only while *this* crate compiles as a test target. A test in
//! `xai-grok-shell` calls into this crate while it is an ordinary dependency of
//! that crate's test binary, so a `cfg(test)`-gated branch would be
//! unconditionally dead for every cross-crate caller and [`StateHomeGuard::pin`]
//! would look like it worked while doing nothing (#482, #503).

use std::cell::RefCell;
use std::path::{Path, PathBuf};

thread_local! {
    static STATE_HOME_OVERRIDE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// The state directory pinned for the current thread, if a [`StateHomeGuard`]
/// is live on it. `None` in production, where nothing pins one.
pub fn pinned_state_home() -> Option<PathBuf> {
    STATE_HOME_OVERRIDE.with(|slot| slot.borrow().clone())
}

/// RAII pin of the state directory [`crate::grok_home`] reports on this thread.
///
/// Restores the previous pin on drop, including across a panicking assertion,
/// so a failing test cannot leak its fixture directory onto the next one.
///
/// `*const ()` keeps the guard `!Send`: dropping it on another thread would
/// restore a different thread's slot and strand this one's pin.
pub struct StateHomeGuard {
    previous: Option<PathBuf>,
    _not_send: std::marker::PhantomData<*const ()>,
}

impl StateHomeGuard {
    /// Pin `path` as this thread's state directory, creating it if needed so
    /// callers get the same "directory exists" guarantee [`crate::grok_home`]
    /// gives on its own first resolution.
    pub fn pin(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        let _ = std::fs::create_dir_all(&path);
        let previous = STATE_HOME_OVERRIDE.with(|slot| slot.replace(Some(path)));
        Self {
            previous,
            _not_send: std::marker::PhantomData,
        }
    }
}

impl Drop for StateHomeGuard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        STATE_HOME_OVERRIDE.with(|slot| {
            *slot.borrow_mut() = previous;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_redirects_grok_home_and_drop_restores_it() {
        let unpinned = crate::grok_home();
        let fixture = tempfile::tempdir().expect("fixture state home");
        {
            let _guard = StateHomeGuard::pin(fixture.path());
            assert_eq!(crate::grok_home(), fixture.path());
        }
        assert_eq!(crate::grok_home(), unpinned);
        assert_eq!(pinned_state_home(), None);
    }

    #[test]
    fn pin_creates_a_missing_directory() {
        let root = tempfile::tempdir().expect("fixture root");
        let fixture = root.path().join("not-created-yet");
        let _guard = StateHomeGuard::pin(&fixture);
        assert!(fixture.is_dir(), "pin must create the directory it pins");
        assert_eq!(crate::grok_home(), fixture);
    }

    #[test]
    fn nested_pins_restore_the_outer_directory() {
        let outer = tempfile::tempdir().expect("outer state home");
        let inner = tempfile::tempdir().expect("inner state home");
        let _outer_guard = StateHomeGuard::pin(outer.path());
        {
            let _inner_guard = StateHomeGuard::pin(inner.path());
            assert_eq!(crate::grok_home(), inner.path());
        }
        assert_eq!(crate::grok_home(), outer.path());
    }

    /// A thread that was never told about the pin does not get it. Threads that
    /// *must* share it — the session actor's own `std::thread` — are handed it
    /// explicitly at their spawn site, so this is the boundary working, not a
    /// gap in it.
    #[test]
    fn a_pin_does_not_leak_to_another_thread() {
        let fixture = tempfile::tempdir().expect("fixture state home");
        let _guard = StateHomeGuard::pin(fixture.path());
        let pinned = fixture.path().to_path_buf();
        let elsewhere = std::thread::spawn(pinned_state_home)
            .join()
            .expect("sibling thread");
        assert_eq!(
            elsewhere, None,
            "the pin must stay on the thread that took it"
        );
        assert_eq!(crate::grok_home(), pinned);
    }
}
