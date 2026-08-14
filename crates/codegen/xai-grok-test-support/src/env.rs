//! Binary resolution, serial env guards, and git sandbox creation.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::sandbox::TestSandbox;

/// Parse env var `key` into `T`, falling back to `default` when it is unset or
/// present-but-unparseable (warning in the latter case).
pub fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    let Ok(raw) = std::env::var(key) else {
        return default;
    };
    match raw.parse() {
        Ok(value) => value,
        Err(_) => {
            eprintln!("[test-support] ignoring unparseable {key}={raw:?}; using default");
            default
        }
    }
}

/// RAII guard for a single environment variable in `#[serial]` tests: snapshots
/// the prior value on construction, applies the change, then restores the prior
/// value (or unsets it) on drop — even if an assertion panics. Restoring rather
/// than always unsetting avoids clobbering vars a parent process/harness set
/// (e.g. `RUST_LOG`).
///
/// Callers MUST be `#[serial_test::serial]`: the `unsafe` `set_var`/`remove_var`
/// are sound only when no other thread accesses the environment concurrently.
/// Each live guard also holds a process-wide nestable lock so two `EnvGuard`
/// users cannot interleave mutations even if a test forgets the attribute
/// (#318). That lock does not compose with raw `std::env::{set_var,remove_var}`.
pub struct EnvGuard {
    key: &'static str,
    prior: Option<OsString>,
    _lease: EnvLockLease,
}

static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

thread_local! {
    static ENV_LOCK_NEST: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static ENV_LOCK_HELD: std::cell::RefCell<Option<std::sync::MutexGuard<'static, ()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Process-wide lease: the first `EnvGuard` on a thread takes [`ENV_MUTEX`];
/// nested guards on the same thread increment a count and share that hold.
struct EnvLockLease;

impl EnvLockLease {
    fn acquire() -> Self {
        ENV_LOCK_NEST.with(|nest| {
            if nest.get() == 0 {
                let guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
                ENV_LOCK_HELD.with(|held| *held.borrow_mut() = Some(guard));
            }
            nest.set(nest.get() + 1);
        });
        Self
    }
}

impl Drop for EnvLockLease {
    fn drop(&mut self) {
        ENV_LOCK_NEST.with(|nest| {
            let left = nest.get().saturating_sub(1);
            nest.set(left);
            if left == 0 {
                ENV_LOCK_HELD.with(|held| {
                    drop(held.borrow_mut().take());
                });
            }
        });
    }
}

impl EnvGuard {
    /// Set `key` to `value` for the guard's lifetime. Accepts `&str`, `&Path`,
    /// `String`, etc. via `AsRef<OsStr>`.
    pub fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let _lease = EnvLockLease::acquire();
        let prior = std::env::var_os(key);
        // SAFETY: this thread holds [`ENV_MUTEX`] (and callers are `#[serial]`),
        // so no other thread touches the env while the lease is live.
        unsafe { std::env::set_var(key, value) };
        Self {
            key,
            prior,
            _lease,
        }
    }

    /// Unset `key` for the guard's lifetime.
    pub fn unset(key: &'static str) -> Self {
        let _lease = EnvLockLease::acquire();
        let prior = std::env::var_os(key);
        // SAFETY: see [`EnvGuard::set`].
        unsafe { std::env::remove_var(key) };
        Self {
            key,
            prior,
            _lease,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: see [`EnvGuard::set`]. The lease drops after this body and
        // releases the process lock only when the last nested guard is gone.
        match self.prior.take() {
            Some(v) => unsafe { std::env::set_var(self.key, v) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::EnvGuard;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    const RACE_KEY: &str = "XAI_GROK_TEST_SUPPORT_ENVGUARD_RACE";

    #[test]
    fn env_guard_serializes_concurrent_mutations() {
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
        let spawn = |label: &'static str| {
            let order = Arc::clone(&order);
            thread::spawn(move || {
                let _g = EnvGuard::set(RACE_KEY, label);
                order.lock().unwrap().push(format!("{label}-start"));
                thread::sleep(Duration::from_millis(20));
                assert_eq!(std::env::var(RACE_KEY).ok().as_deref(), Some(label));
                order.lock().unwrap().push(format!("{label}-end"));
            })
        };

        let a = spawn("alpha-isolation");
        let b = spawn("beta-isolation");
        a.join().expect("alpha EnvGuard thread");
        b.join().expect("beta EnvGuard thread");
        let order = order.lock().unwrap().clone();
        let alpha_first = [
            "alpha-isolation-start",
            "alpha-isolation-end",
            "beta-isolation-start",
            "beta-isolation-end",
        ];
        let beta_first = [
            "beta-isolation-start",
            "beta-isolation-end",
            "alpha-isolation-start",
            "alpha-isolation-end",
        ];
        assert!(
            order == alpha_first || order == beta_first,
            "EnvGuard users must run one-at-a-time, got {order:?}"
        );
        assert!(
            std::env::var_os(RACE_KEY).is_none(),
            "both guards must restore the prior (unset) value"
        );
    }
}

fn workspace_root() -> PathBuf {
    // nth(3): crate is nested three levels below the cargo workspace root.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("workspace root")
        .to_path_buf()
}

fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().join("target"))
}

fn local_grok_binary_path() -> PathBuf {
    target_dir()
        .join("debug")
        .join(format!("xai-grok-pager{}", std::env::consts::EXE_SUFFIX))
}

fn ensure_local_grok_binary(binary: &Path) {
    if binary.exists() {
        return;
    }

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let mut cmd = Command::new(&cargo);
    cmd.current_dir(workspace_root())
        .args([
            "build",
            "-p",
            "xai-grok-pager-bin",
            "--bin",
            "xai-grok-pager",
        ])
        .stdin(std::process::Stdio::null())
        .envs(xai_tty_utils::pager_env());
    xai_tty_utils::detach_std_command(&mut cmd);
    let output = cmd
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {cargo} to build xai-grok-pager: {e}"));

    assert!(
        output.status.success(),
        "failed to build xai-grok-pager for lifecycle tests (exit {:?})\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        binary.exists(),
        "xai-grok-pager build completed but binary missing at {}",
        binary.display()
    );
}

/// Resolve grok binary: `GROK_BINARY` env (CI) or a locally built `xai-grok-pager` binary.
pub fn grok_binary() -> PathBuf {
    if let Ok(path) = std::env::var("GROK_BINARY") {
        let p = PathBuf::from(path);
        assert!(p.exists(), "GROK_BINARY does not exist: {}", p.display());
        // Bazel's GROK_BINARY is runfiles-relative; the harness spawns the child
        // with a different cwd, so absolutize against the (runfiles-root) cwd now.
        return std::path::absolute(&p).unwrap_or(p);
    }

    if let Ok(path) = std::env::var("CARGO_BIN_EXE_xai-grok-pager") {
        let p = PathBuf::from(path);
        if p.exists() {
            return p;
        }
    }

    let binary = local_grok_binary_path();
    ensure_local_grok_binary(&binary);
    binary
}

/// Create an owned, git-initialized [`TestSandbox`].
pub fn git_workdir() -> TestSandbox {
    TestSandbox::builder().git().build()
}
