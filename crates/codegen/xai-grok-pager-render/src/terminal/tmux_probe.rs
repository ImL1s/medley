//! Shared tmux command protocol and result parsing.

use std::process::{Command, Stdio};
use std::time::Duration;

const TMUX_QUERY_TIMEOUT: Duration = Duration::from_secs(2);
/// After the leader exits, allow this much additional time for process-group
/// teardown and concurrent pipe drains so a near-deadline success is not turned
/// into a drain timeout. The main process wait still uses only
/// [`TMUX_QUERY_TIMEOUT`].
const POST_EXIT_CLEANUP_GRACE: Duration = Duration::from_millis(300);
/// How long a signalled process group may take to empty before it is killed.
const GROUP_EXIT_GRACE: Duration = Duration::from_millis(100);
const GROUP_EXIT_POLL: Duration = Duration::from_millis(1);

#[cfg(test)]
thread_local! {
    static TERMINATE_OWNED_GROUP_SLEEPS: std::cell::Cell<u32> =
        const { std::cell::Cell::new(0) };
    static LAST_PIPE_DRAIN_DEADLINE: std::cell::Cell<Option<std::time::Instant>> =
        const { std::cell::Cell::new(None) };
    static LAST_PIPE_DRAIN_GRACE: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
    static TEST_TMUX_PROGRAM: std::cell::RefCell<Option<std::path::PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TmuxCommand<'a> {
    Version,
    OptionValue(&'a str),
    OptionSupport(&'a str),
    ControlMode,
    ClientFeatures,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TmuxCommandOutput {
    status_success: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

trait TmuxCommandRunner {
    fn run(&self, command: TmuxCommand<'_>) -> Result<TmuxCommandOutput, String>;
}

struct LiveTmuxCommandRunner;

impl TmuxCommandRunner for LiveTmuxCommandRunner {
    fn run(&self, command: TmuxCommand<'_>) -> Result<TmuxCommandOutput, String> {
        run_tmux_bounded(command, TMUX_QUERY_TIMEOUT)
    }
}

fn run_tmux_bounded(
    command: TmuxCommand<'_>,
    timeout: Duration,
) -> Result<TmuxCommandOutput, String> {
    let mut command = build_tmux_command(command);
    #[allow(clippy::disallowed_methods)] // bounded probe, waited on with a timeout
    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to run tmux: {error}"))?;
    let group = xai_tty_utils::ProcessGroup::new()
        .and_then(|mut group| {
            group.attach_std(&child)?;
            Ok(group)
        })
        .map_err(|error| {
            let _ = child.kill();
            let _ = child.wait();
            format!("failed to own tmux process tree: {error}")
        })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "tmux stdout pipe was not captured".to_owned())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "tmux stderr pipe was not captured".to_owned())?;
    let stdout = spawn_pipe_drain(stdout, "stdout");
    let stderr = spawn_pipe_drain(stderr, "stderr");
    let deadline = std::time::Instant::now() + timeout;

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(15));
            }
            Ok(None) => {
                terminate_tmux_tree(&group, &mut child);
                return Err(format!("tmux query timed out after {timeout:?}"));
            }
            Err(error) => {
                terminate_tmux_tree(&group, &mut child);
                return Err(format!("failed to wait for tmux: {error}"));
            }
        }
    };

    // The leader may be reaped while descendants still exist or hold pipes.
    // `post_exit_cleanup_and_drain` grants a fresh post-exit bound so a
    // near-deadline success still drains; the main process deadline above is
    // not extended for hung leaders.
    let (stdout, stderr) =
        post_exit_cleanup_and_drain(&group, stdout, stderr, POST_EXIT_CLEANUP_GRACE)?;
    Ok(TmuxCommandOutput {
        status_success: status.success(),
        stdout,
        stderr,
    })
}

/// SIGTERMs the group, then drains both pipes against a window that starts
/// *now* — not against whatever remained of an earlier deadline. `grace` is a
/// relative [`Duration`], not an absolute [`std::time::Instant`]: there is no
/// outer deadline variable in this function's scope to accidentally reuse, so
/// the #490 regression this guards against (a drain silently inheriting a
/// near-exhausted main-process deadline) would need a caller to explicitly
/// compute and pass a borrowed remainder — a deliberate, reviewable act,
/// rather than a one-line variable-reuse mistake made invisible by scope.
fn post_exit_cleanup_and_drain(
    group: &xai_tty_utils::ProcessGroup,
    stdout: std::sync::mpsc::Receiver<Result<Vec<u8>, String>>,
    stderr: std::sync::mpsc::Receiver<Result<Vec<u8>, String>>,
    grace: Duration,
) -> Result<(Vec<u8>, Vec<u8>), String> {
    let cleanup_deadline = std::time::Instant::now() + grace;
    #[cfg(test)]
    LAST_PIPE_DRAIN_DEADLINE.with(|slot| slot.set(Some(cleanup_deadline)));
    #[cfg(test)]
    LAST_PIPE_DRAIN_GRACE.with(|slot| slot.set(Some(grace)));
    terminate_owned_group(group);
    let stdout = recv_pipe_drain(stdout, cleanup_deadline, "stdout")?;
    let stderr = recv_pipe_drain(stderr, cleanup_deadline, "stderr")?;
    Ok((stdout, stderr))
}

fn spawn_pipe_drain(
    mut pipe: impl std::io::Read + Send + 'static,
    label: &'static str,
) -> std::sync::mpsc::Receiver<Result<Vec<u8>, String>> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut output = Vec::new();
        let result = pipe
            .read_to_end(&mut output)
            .map(|_| output)
            .map_err(|error| format!("failed to read tmux {label}: {error}"));
        let _ = sender.send(result);
    });
    receiver
}

fn recv_pipe_drain(
    receiver: std::sync::mpsc::Receiver<Result<Vec<u8>, String>>,
    deadline: std::time::Instant,
    label: &'static str,
) -> Result<Vec<u8>, String> {
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    let timed_out = || format!("tmux {label} did not close before the query deadline");
    if remaining.is_zero() {
        return match receiver.try_recv() {
            Ok(value) => value,
            Err(_) => Err(timed_out()),
        };
    }
    receiver.recv_timeout(remaining).map_err(|_| timed_out())?
}

fn terminate_tmux_tree(group: &xai_tty_utils::ProcessGroup, child: &mut std::process::Child) {
    terminate_owned_group(group);
    let _ = child.wait();
}

/// SIGTERM the group, then escalate to SIGKILL only if it outlives the grace.
///
/// Callers reach this with the leader already reaped, so the group is usually
/// empty on the first check.
fn terminate_owned_group(group: &xai_tty_utils::ProcessGroup) {
    let _ = group.terminate();
    let deadline = std::time::Instant::now() + GROUP_EXIT_GRACE;
    loop {
        if group.has_live_members() == Some(false) {
            // `return`, not `break`: the reaped leader's pid may already
            // belong to an unrelated group, so an empty group gets no SIGKILL.
            return;
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        #[cfg(test)]
        TERMINATE_OWNED_GROUP_SLEEPS.with(|c| c.set(c.get() + 1));
        std::thread::sleep(GROUP_EXIT_POLL);
    }
    let _ = group.kill();
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TmuxQueryResult<T> {
    Available(T),
    Unsupported,
    Unavailable,
    Error(String),
}

impl<T> TmuxQueryResult<T> {
    pub fn into_option(self) -> Option<T> {
        match self {
            Self::Available(value) => Some(value),
            Self::Unsupported | Self::Unavailable | Self::Error(_) => None,
        }
    }
}

pub fn query_version() -> TmuxQueryResult<String> {
    query_version_with(&LiveTmuxCommandRunner)
}

fn query_version_with(runner: &dyn TmuxCommandRunner) -> TmuxQueryResult<String> {
    parse_value(runner.run(TmuxCommand::Version))
}

pub fn query_option(option: &str) -> TmuxQueryResult<String> {
    query_option_with(&LiveTmuxCommandRunner, option)
}

fn query_option_with(runner: &dyn TmuxCommandRunner, option: &str) -> TmuxQueryResult<String> {
    parse_value(runner.run(TmuxCommand::OptionValue(option)))
}

pub fn query_option_support(option: &str) -> TmuxQueryResult<()> {
    query_option_support_with(&LiveTmuxCommandRunner, option)
}

fn query_option_support_with(runner: &dyn TmuxCommandRunner, option: &str) -> TmuxQueryResult<()> {
    match runner.run(TmuxCommand::OptionSupport(option)) {
        Ok(output) if output.status_success => TmuxQueryResult::Available(()),
        Ok(output) if stderr_identifies_unknown_option(&output.stderr, option) => {
            TmuxQueryResult::Unsupported
        }
        Ok(_) => TmuxQueryResult::Unavailable,
        Err(error) => TmuxQueryResult::Error(error),
    }
}

/// The attached client's resolved terminal features, as a comma-separated list
/// (`RGB`, `clipboard`, `focus`, …).
///
/// tmux resolves this once per client at attach time from the outer terminal's
/// terminfo plus `terminal-features` / `terminal-overrides`, and it decides
/// whether 24-bit SGR survives the multiplexer. `COLORTERM` inside the pane
/// describes only what the pane's program emits, so it cannot answer that.
///
/// Empty output means the answer is unknown rather than negative: tmux before
/// 3.2 has no `terminal-features` and renders the unknown format as an empty
/// string, and a server with no attached client has nothing to report.
pub fn query_client_features() -> TmuxQueryResult<String> {
    query_client_features_with(&LiveTmuxCommandRunner)
}

fn query_client_features_with(runner: &dyn TmuxCommandRunner) -> TmuxQueryResult<String> {
    parse_value(runner.run(TmuxCommand::ClientFeatures))
}

pub fn query_control_mode() -> TmuxQueryResult<bool> {
    query_control_mode_with(&LiveTmuxCommandRunner)
}

fn query_control_mode_with(runner: &dyn TmuxCommandRunner) -> TmuxQueryResult<bool> {
    match runner.run(TmuxCommand::ControlMode) {
        Ok(output) if output.status_success => TmuxQueryResult::Available(
            String::from_utf8_lossy(&output.stdout).contains("control-mode"),
        ),
        Ok(_) => TmuxQueryResult::Unavailable,
        Err(error) => TmuxQueryResult::Error(error),
    }
}

fn tmux_program() -> std::ffi::OsString {
    #[cfg(test)]
    if let Some(path) = TEST_TMUX_PROGRAM.with(|slot| slot.borrow().clone()) {
        return path.into();
    }
    "tmux".into()
}

fn build_tmux_command(command: TmuxCommand<'_>) -> Command {
    let mut cmd = Command::new(tmux_program());
    match command {
        TmuxCommand::Version => {
            cmd.arg("-V").stdout(Stdio::piped()).stderr(Stdio::piped());
        }
        TmuxCommand::OptionValue(option) => {
            cmd.args(["show-option", "-gqv", option])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
        }
        TmuxCommand::OptionSupport(option) => {
            cmd.args(["show-option", "-gv", option])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
        }
        TmuxCommand::ControlMode => {
            cmd.args(["display-message", "-p", "#{client_flags}"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
        }
        TmuxCommand::ClientFeatures => {
            cmd.args(["display-message", "-p", "#{client_termfeatures}"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
        }
    }
    cmd.stdin(Stdio::null()).envs(xai_tty_utils::pager_env());
    xai_tty_utils::detach_std_command(&mut cmd);
    cmd
}

fn parse_value(output: Result<TmuxCommandOutput, String>) -> TmuxQueryResult<String> {
    match output {
        Ok(output) if output.status_success => {
            let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if value.is_empty() {
                TmuxQueryResult::Unavailable
            } else {
                TmuxQueryResult::Available(value)
            }
        }
        Ok(_) => TmuxQueryResult::Unavailable,
        Err(error) => TmuxQueryResult::Error(error),
    }
}

fn stderr_identifies_unknown_option(stderr: &[u8], option: &str) -> bool {
    let invalid = format!("invalid option: {option}");
    let unknown = format!("unknown option: {option}");
    String::from_utf8_lossy(stderr)
        .lines()
        .any(|line| matches!(line.trim(), value if value == invalid || value == unknown))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::ffi::{OsStr, OsString};

    use super::*;

    struct FakeRunner {
        output: Result<TmuxCommandOutput, String>,
        calls: RefCell<Vec<String>>,
    }

    impl FakeRunner {
        fn output(status_success: bool, stdout: &[u8], stderr: &[u8]) -> Self {
            Self {
                output: Ok(TmuxCommandOutput {
                    status_success,
                    stdout: stdout.to_vec(),
                    stderr: stderr.to_vec(),
                }),
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl TmuxCommandRunner for FakeRunner {
        fn run(&self, command: TmuxCommand<'_>) -> Result<TmuxCommandOutput, String> {
            self.calls.borrow_mut().push(match command {
                TmuxCommand::Version => "version".to_owned(),
                TmuxCommand::OptionValue(option) => format!("value:{option}"),
                TmuxCommand::OptionSupport(option) => format!("support:{option}"),
                TmuxCommand::ControlMode => "control-mode".to_owned(),
                TmuxCommand::ClientFeatures => "client-features".to_owned(),
            });
            self.output.clone()
        }
    }

    struct TmuxProgramGuard {
        previous: Option<std::path::PathBuf>,
    }

    impl TmuxProgramGuard {
        fn pin(path: impl Into<std::path::PathBuf>) -> Self {
            let previous = TEST_TMUX_PROGRAM.with(|slot| slot.replace(Some(path.into())));
            Self { previous }
        }
    }

    impl Drop for TmuxProgramGuard {
        fn drop(&mut self) {
            TEST_TMUX_PROGRAM.with(|slot| {
                *slot.borrow_mut() = self.previous.take();
            });
        }
    }

    #[test]
    fn command_protocol_uses_exact_argv_and_pager_env() {
        let cases = [
            (TmuxCommand::Version, vec!["-V"]),
            (
                TmuxCommand::OptionValue("set-clipboard"),
                vec!["show-option", "-gqv", "set-clipboard"],
            ),
            (
                TmuxCommand::OptionSupport("allow-passthrough"),
                vec!["show-option", "-gv", "allow-passthrough"],
            ),
            (
                TmuxCommand::ControlMode,
                vec!["display-message", "-p", "#{client_flags}"],
            ),
            (
                TmuxCommand::ClientFeatures,
                vec!["display-message", "-p", "#{client_termfeatures}"],
            ),
        ];
        for (request, args) in cases {
            let cmd = build_tmux_command(request);
            assert_eq!(cmd.get_program(), OsStr::new("tmux"));
            assert_eq!(cmd.get_args().collect::<Vec<_>>(), args);
            let actual: HashMap<OsString, Option<OsString>> = cmd
                .get_envs()
                .map(|(key, value)| (key.to_owned(), value.map(OsStr::to_owned)))
                .collect();
            let expected: HashMap<OsString, Option<OsString>> = xai_tty_utils::pager_env()
                .into_iter()
                .map(|(key, value)| (key.into(), Some(value.into())))
                .collect();
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn value_and_control_queries_use_the_injected_runner() {
        let runner = FakeRunner::output(true, b" on\n", b"");
        assert_eq!(
            query_option_with(&runner, "set-clipboard"),
            TmuxQueryResult::Available("on".to_owned())
        );
        assert_eq!(runner.calls.into_inner(), ["value:set-clipboard"]);

        let runner = FakeRunner::output(true, b"control-mode,utf8\n", b"");
        assert_eq!(
            query_control_mode_with(&runner),
            TmuxQueryResult::Available(true)
        );
        assert_eq!(runner.calls.into_inner(), ["control-mode"]);
    }

    #[test]
    fn nonzero_and_execution_failure_remain_fail_open_facts() {
        let runner = FakeRunner::output(false, b"on", b"server unavailable");
        assert_eq!(
            query_option_with(&runner, "set-clipboard"),
            TmuxQueryResult::Unavailable
        );
        let runner = FakeRunner {
            output: Err("spawn failed".to_owned()),
            calls: RefCell::new(Vec::new()),
        };
        assert_eq!(
            query_version_with(&runner),
            TmuxQueryResult::Error("spawn failed".to_owned())
        );
    }

    #[test]
    fn support_query_accepts_both_known_spellings_only() {
        for stderr in [
            b"invalid option: allow-passthrough\n".as_slice(),
            b"unknown option: allow-passthrough\n".as_slice(),
        ] {
            let runner = FakeRunner::output(false, b"", stderr);
            assert_eq!(
                query_option_support_with(&runner, "allow-passthrough"),
                TmuxQueryResult::Unsupported
            );
        }
        let runner = FakeRunner::output(false, b"", b"no server running\n");
        assert_eq!(
            query_option_support_with(&runner, "allow-passthrough"),
            TmuxQueryResult::Unavailable
        );
    }

    /// Records the *usable* drain deadline (`now + grace`), not the input
    /// `grace` argument. Pre-filled pipes drain immediately, so asserting the
    /// argument still passes if the helper computes `Instant::now()` and
    /// throws the grace away — the zero-grace regression the sibling timeout
    /// test explicitly delegates here (#501 review).
    #[test]
    fn post_exit_grace_is_fresh_not_borrowed_from_an_earlier_deadline() {
        let group = xai_tty_utils::ProcessGroup::new().expect("group");
        LAST_PIPE_DRAIN_DEADLINE.with(|c| c.set(None));

        let (stdout_tx, stdout_rx) = std::sync::mpsc::sync_channel::<Result<Vec<u8>, String>>(1);
        stdout_tx
            .send(Ok(b"ok\n".to_vec()))
            .expect("pre-fill stdout");
        drop(stdout_tx);
        let (stderr_tx, stderr_rx) = std::sync::mpsc::sync_channel::<Result<Vec<u8>, String>>(1);
        stderr_tx.send(Ok(Vec::new())).expect("pre-fill stderr");
        drop(stderr_tx);

        let before = std::time::Instant::now();
        let (stdout, stderr) =
            post_exit_cleanup_and_drain(&group, stdout_rx, stderr_rx, POST_EXIT_CLEANUP_GRACE)
                .expect("pre-filled pipes must drain under a fresh grace");
        let after = std::time::Instant::now();
        assert_eq!(String::from_utf8_lossy(&stdout).trim(), "ok");
        assert!(stderr.is_empty());
        let recorded = LAST_PIPE_DRAIN_DEADLINE
            .with(|c| c.get())
            .expect("drain must record the usable deadline it waited against");
        assert!(
            recorded >= before + POST_EXIT_CLEANUP_GRACE,
            "usable deadline must include the full supplied grace, not Instant::now()"
        );
        assert!(
            recorded <= after + POST_EXIT_CLEANUP_GRACE,
            "usable deadline must be now+grace, not a leftover outer timeout"
        );
    }

    /// The other half of the property above, made deterministic instead of
    /// a second race against signal/escalation timing — which a PR whose
    /// entire subject is removing scheduling races should not introduce.
    ///
    /// Two earlier drafts of this test both raced real time. The first
    /// reused the SIGTERM-ignoring fixture above with a too-small `grace`,
    /// depending on trap-install timing *and* on SIGKILL teardown and
    /// drain-thread scheduling landing in a specific order relative to a
    /// 5ms deadline. The second replaced that with an external `sh -c
    /// 'sleep 1000'` holder deliberately never attached to `group`, and
    /// proved the timeout by asserting `elapsed < 200ms` — an upper bound
    /// on the *test's own* wall-clock time, which Codex's review of #501
    /// correctly flagged: a descheduled test thread can exceed any fixed
    /// ceiling even when the implementation under test is entirely
    /// correct, reintroducing exactly the class of flakiness this PR
    /// removes everywhere else.
    ///
    /// This version never races a delayed send against `recv_timeout`.
    /// The pipe is left empty and the deadline is already in the past, so
    /// `recv_pipe_drain` takes the zero-remaining `try_recv` path: either
    /// the value is already queued (pipe closed in time) or the wait is a
    /// timeout. A descheduled test thread cannot turn a later send into
    /// `Ok` (#501 review).
    ///
    /// One thing this test still does *not* catch: a `grace` silently
    /// dropped (collapsed to ~0). That path is `Err` too, indistinguishable
    /// from correct behaviour here — the sibling positive test catches it by
    /// asserting the recorded usable deadline is `now + grace`, not `now`
    /// (`post_exit_grace_is_fresh_not_borrowed_from_an_earlier_deadline`).
    #[test]
    fn post_exit_grace_bounds_the_drain_when_the_pipe_closes_only_after_the_grace() {
        let (stdout_tx, stdout_rx) = std::sync::mpsc::sync_channel::<Result<Vec<u8>, String>>(1);
        let past = std::time::Instant::now() - Duration::from_secs(1);
        let result = recv_pipe_drain(stdout_rx, past, "stdout");
        drop(stdout_tx);

        assert!(
            result.is_err(),
            "an already-elapsed deadline must bound the drain wait even when the pipe is still open: {result:?}"
        );
        assert!(
            result
                .unwrap_err()
                .contains("did not close before the query deadline"),
            "must fail as a drain timeout, not some other error"
        );
    }

    /// End-to-end sanity through the real spawn/wait/cleanup pipeline: a
    /// leader that forks a descendant which keeps the pipes open still
    /// produces captured output once the leader exits, because
    /// `post_exit_cleanup_and_drain` (tested for its specific "fresh grace"
    /// property above, in isolation from any external-process timing) tears
    /// the descendant down and finishes the drain.
    ///
    /// #490: no longer a race. The predecessor of this test made the leader
    /// sleep to just under the main deadline so a shared-deadline bug would
    /// fail the drain — sound in design, but it meant a real `perl`+shell
    /// spawn chain had to finish within 300ms of an unrelated 1.5s deadline,
    /// and that margin is exactly what ambient CPU contention eats: it
    /// failed **in isolation**, not just under load, with
    /// `tmux query timed out after 1.5s` — the *wait loop's* timeout, not a
    /// drain error, meaning the flake was in how long the external process
    /// chain took to run, not in anything `post_exit_cleanup_and_drain`
    /// does. The regression that race actually guarded against is now
    /// guarded by that function's own signature instead (see the test
    /// above), so this leader has no reason to cut it close: it exits
    /// immediately, and the only real-time dependency left is "SIGTERM
    /// reaps a `sleep` descendant," which needs low milliseconds, not
    /// hundreds.
    ///
    /// On its own this test cannot distinguish a caller passing a *fresh*
    /// grace from one passing a *borrowed* remainder of `timeout`: its
    /// descendant stays in the group and dies to the leading `SIGTERM`
    /// well inside either window, so a caller-side regression that
    /// resurrected the exact bug #490 removed would still leave this test
    /// green. That is a real, separately-caught gap, not a rhetorical
    /// one — a mutation at the `run_tmux_bounded` call site that swaps
    /// `POST_EXIT_CLEANUP_GRACE` for
    /// `deadline.saturating_duration_since(Instant::now())` reds only the
    /// sibling test below (`run_tmux_bounded_uses_a_fresh_grace_not_the_
    /// remaining_main_deadline`), never this one — confirmed by running
    /// that exact mutation against this file. That sibling test is what
    /// actually proves freshness at the caller level; this one only
    /// proves the drain survives a descendant outliving the leader at
    /// all.
    #[cfg(unix)]
    #[test]
    fn run_tmux_bounded_drains_a_descendant_still_holding_the_pipes() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let tmux = bin.join("tmux");
        let timeout = Duration::from_millis(1500);
        std::fs::write(
            &tmux,
            "#!/bin/sh\n\
             ( exec sleep 30 ) &\n\
             printf 'tmux 3.4\\n'\n\
             exit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o755)).unwrap();

        let _tmux_bin = TmuxProgramGuard::pin(&tmux);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_tmux_bounded(TmuxCommand::Version, timeout)
        }));
        let output = result
            .expect("probe must not panic")
            .expect("a pipe-holding descendant must not turn a clean exit into a drain error");
        assert!(output.status_success, "expected successful status");
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "tmux 3.4");
    }

    /// Sibling to the end-to-end test above: `run_tmux_bounded` must pass
    /// [`post_exit_cleanup_and_drain`] a *fresh* [`POST_EXIT_CLEANUP_GRACE`],
    /// not a remainder borrowed from its own much larger `timeout`.
    ///
    /// Earlier drafts inferred that from a wall-clock race (an escaped
    /// holder closing between 300ms and 5s). Under deschedule the holder
    /// can close before the drain thread observes its timeout, and a
    /// correct implementation then looks like `Ok` (#501 review). The
    /// drain now records the `Duration` it was given, so the assertion is
    /// on that value rather than on whether a real-time holder outlived
    /// it.
    #[cfg(unix)]
    #[test]
    fn run_tmux_bounded_uses_a_fresh_grace_not_the_remaining_main_deadline() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let tmux = bin.join("tmux");
        let timeout = Duration::from_millis(5000);
        std::fs::write(
            &tmux,
            "#!/bin/sh\n\
             printf 'tmux 3.4\\n'\n\
             exit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o755)).unwrap();

        let _tmux_bin = TmuxProgramGuard::pin(&tmux);
        LAST_PIPE_DRAIN_GRACE.with(|c| c.set(None));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_tmux_bounded(TmuxCommand::Version, timeout)
        }));
        result
            .expect("probe must not panic")
            .expect("an immediate-exit tmux must drain successfully");
        assert_eq!(
            LAST_PIPE_DRAIN_GRACE.with(|c| c.get()),
            Some(POST_EXIT_CLEANUP_GRACE),
            "run_tmux_bounded must pass a fresh POST_EXIT_CLEANUP_GRACE, not a remainder of timeout"
        );
    }

    /// Observe the poll/sleep path directly instead of inferring it from
    /// wall time: an empty group returns before `sleep`, so a descheduled
    /// test thread cannot false-fail a correct short-circuit (#501 review).
    #[cfg(unix)]
    #[test]
    fn empty_group_teardown_does_not_wait_out_the_grace() {
        let group = xai_tty_utils::ProcessGroup::new().expect("group");
        TERMINATE_OWNED_GROUP_SLEEPS.with(|c| c.set(0));
        terminate_owned_group(&group);
        assert_eq!(
            TERMINATE_OWNED_GROUP_SLEEPS.with(|c| c.get()),
            0,
            "an empty group must return before the poll/sleep loop"
        );
    }

    #[cfg(unix)]
    #[test]
    fn group_that_ignores_sigterm_waits_the_grace_and_is_killed() {
        let mut group = xai_tty_utils::ProcessGroup::new().expect("group");
        let ready_path = std::env::temp_dir().join(format!(
            "medley-tmux-probe-escalation-ready-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&ready_path);
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("trap '' TERM; : > \"$READY_FILE\"; sleep 1000")
            .env("READY_FILE", &ready_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        xai_tty_utils::detach_std_command(&mut cmd);
        #[allow(clippy::disallowed_methods)] // test fixture; the test kills it
        let mut child = cmd.spawn().expect("spawn sigterm-ignoring child");
        group.attach_std(&child).expect("attach");
        // Handshake, not a head start: the child creates READY_FILE only
        // after the SIGTERM trap is installed. Waiting for that file is
        // what keeps SIGTERM from racing the default disposition under a
        // loaded Ubuntu runner (#501 review).
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !ready_path.exists() {
            if std::time::Instant::now() >= deadline {
                let _ = group.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(&ready_path);
                panic!("fixture never signalled that its SIGTERM trap was installed");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = std::fs::remove_file(&ready_path);

        let started = std::time::Instant::now();
        terminate_owned_group(&group);
        let elapsed = started.elapsed();

        assert!(
            elapsed >= GROUP_EXIT_GRACE,
            "an occupied group must still get the full grace, took {elapsed:?}"
        );

        // Bounded: without SIGKILL this fails in seconds rather than blocking
        // the run on `sleep 1000`.
        let reap_deadline = std::time::Instant::now() + Duration::from_secs(5);
        let status = loop {
            match child.try_wait().expect("poll child") {
                Some(status) => break status,
                None if std::time::Instant::now() < reap_deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                None => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("the child survived teardown, so SIGKILL never escalated");
                }
            }
        };
        assert!(
            !status.success(),
            "the child must have been killed, got {status:?}"
        );
    }
}
