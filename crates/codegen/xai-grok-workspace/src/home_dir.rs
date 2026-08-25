//! Single seam for resolving the user's home directory (#493).
//!
//! Every call site in this crate that needs "the user's home" should go
//! through [`resolved_home_dir`] instead of calling `dirs::home_dir()`
//! directly, so a test can redirect it via [`HomeDirGuard`] without
//! mutating process environment.
//!
//! ## Why this exists: `std::env::set_var` is unsound here, not just risky
//!
//! `dirs::home_dir()` reads `$HOME` on Unix. Tests that needed to isolate it
//! used to do `EnvGuard::set("HOME", tempdir)` — sound only when no other
//! thread reads the environment while the guard is live. `#[serial_test::serial]`
//! serialises *test bodies* against each other; it does nothing about threads
//! the code under test spawns itself (tokio runtime workers, `spawn_blocking`'s
//! blocking pool, `notify` watcher threads, `jsonl`'s background writer). Any
//! one of those calling `getenv` concurrently with `set_var`/`remove_var` is
//! undefined behaviour — glibc's `getenv`/`setenv` genuinely race and segfault
//! (#493, observed as `SIGSEGV` in `agent::folder_trust::` on CI's Linux
//! runner). This is why `set_var`/`remove_var` are `unsafe` in Rust 2024.
//!
//! A thread-local override removes the hazard by construction instead of
//! narrowing the window: nothing here ever calls `std::env::set_var`, so
//! there is no unsafe mutation for a concurrent reader to race.
//!
//! ## Why the override check below is NOT `#[cfg(test)]`
//!
//! `cfg(test)` is only ever true when *this crate* is itself compiled as a
//! test target. `xai-grok-shell`'s tests (`agent::folder_trust`,
//! `claude_import`) call into functions in *this* crate while it compiles as
//! an ordinary, non-test dependency of `xai-grok-shell`'s test binary — under
//! that build `cfg(test)` here is unconditionally `false`, so a `cfg(test)`-
//! gated branch would silently never fire for any cross-crate caller and
//! `HomeDirGuard::pin` would be a no-op that looks like it works (#482 hit
//! the identical trap moving `resolved_xai_auth_path` to `xai-grok-config`).
//! The thread-local check below runs unconditionally, in every build,
//! including production, at the cost of one thread-local read per call —
//! `HomeDirGuard` is simply never constructed outside test code, so the
//! override is always `None` there.

use std::cell::RefCell;
use std::path::PathBuf;

thread_local! {
    static HOME_DIR_OVERRIDE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// Resolve the user's home directory: a thread-local [`HomeDirGuard`] pin if
/// one is live on this thread, else `dirs::home_dir()` (which reads `$HOME`
/// on Unix). See the module doc for why this replaces `dirs::home_dir()`
/// directly at every call site a `HomeDirGuard`-using test can reach.
pub fn resolved_home_dir() -> Option<PathBuf> {
    if let Some(p) = HOME_DIR_OVERRIDE.with(|slot| slot.borrow().clone()) {
        return Some(p);
    }
    dirs::home_dir()
}

/// Thread-local pin of the resolved home directory, for tests that need
/// [`resolved_home_dir`] (and everything built on it) to see a fake home
/// without mutating process environment. `!Send`: `Drop` restores this
/// thread's override, so the guard must not cross threads (moving it would
/// restore against a different thread's slot and leak the pin on this one —
/// the same hazard `xai-grok-shell`'s `XaiAuthPathGuard` documents, #409).
///
/// Tests using this guard do not need `#[serial_test::serial]` for this
/// state specifically: the pin is thread-local, not process-global, so two
/// tests pinning different homes on different threads cannot observe each
/// other. (They may still need `#[serial]` for *other* process-global state
/// the same test touches, e.g. `GROK_HOME`.)
pub struct HomeDirGuard {
    previous: Option<PathBuf>,
    _not_send: std::marker::PhantomData<*const ()>,
}

impl HomeDirGuard {
    /// Pin `path` as the resolved home directory for the current thread
    /// until the returned guard drops.
    pub fn pin(path: impl Into<PathBuf>) -> Self {
        let previous = HOME_DIR_OVERRIDE.with(|slot| slot.borrow_mut().replace(path.into()));
        Self {
            previous,
            _not_send: std::marker::PhantomData,
        }
    }
}

impl Drop for HomeDirGuard {
    fn drop(&mut self) {
        HOME_DIR_OVERRIDE.with(|slot| {
            *slot.borrow_mut() = self.previous.take();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_overrides_resolved_home_dir_and_restores_on_drop() {
        let unpinned = resolved_home_dir();
        let fake = std::path::PathBuf::from("/tmp/fake-home-for-pin-test");
        {
            let _guard = HomeDirGuard::pin(fake.clone());
            assert_eq!(resolved_home_dir(), Some(fake));
        }
        assert_eq!(
            resolved_home_dir(),
            unpinned,
            "dropping the guard must restore whatever resolved before the pin"
        );
    }

    #[test]
    fn nested_pins_restore_the_outer_pin_not_the_original() {
        let outer = std::path::PathBuf::from("/tmp/outer-home-for-pin-test");
        let inner = std::path::PathBuf::from("/tmp/inner-home-for-pin-test");
        let _outer_guard = HomeDirGuard::pin(outer.clone());
        {
            let _inner_guard = HomeDirGuard::pin(inner.clone());
            assert_eq!(resolved_home_dir(), Some(inner));
        }
        assert_eq!(
            resolved_home_dir(),
            Some(outer),
            "the inner guard's drop must restore the outer pin, not fall through to it"
        );
    }
}
