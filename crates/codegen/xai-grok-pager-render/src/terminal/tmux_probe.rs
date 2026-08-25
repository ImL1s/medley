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
    receiver
        .recv_timeout(remaining)
        .map_err(|_| format!("tmux {label} did not close before the query deadline"))?
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

fn build_tmux_command(command: TmuxCommand<'_>) -> Command {
    let mut cmd = Command::new("tmux");
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

    /// #490: `post_exit_cleanup_and_drain`'s "fresh, not borrowed" grace
    /// window is now guarded by its own signature (see that function's doc
    /// comment) — a caller has no `Instant` deadline in scope to reuse by
    /// accident. This test drives that directly, with no outer deadline
    /// object anywhere in scope at all.
    ///
    /// The process is built to ignore `SIGTERM` (same construction as
    /// `group_that_ignores_sigterm_waits_the_grace_and_is_killed`, which
    /// this shares timing with deliberately) so it needs the *full*
    /// SIGTERM → `GROUP_EXIT_GRACE` → SIGKILL escalation to die, and its
    /// pipe cannot close — so the drain thread cannot have anything
    /// buffered — until that escalation actually completes. That makes this
    /// test genuinely sensitive to `grace`'s value rather than trivially
    /// true regardless of it: the sibling test below (a different fixture,
    /// see its own doc comment for why) asserts the other direction, that
    /// too small a `grace` legitimately fails, so together they bound the
    /// function's behaviour instead of only checking the side that was
    /// always going to pass.
    ///
    /// One residual, named rather than hidden: this fixture has its own
    /// small timing dependency, installing the `SIGTERM` trap shortly after
    /// exec (see the 250ms head start below). If a signal ever raced that
    /// install under extreme load, the process would die to the *default*
    /// SIGTERM instead of forcing the escalation, and this test would pass
    /// without having exercised the property it names — a coverage gap
    /// under pathological load, not a source of flakiness, since a fast
    /// death still leaves `stdout`/`stderr` fully drained either way.
    #[cfg(unix)]
    #[test]
    fn post_exit_grace_is_fresh_not_borrowed_from_an_earlier_deadline() {
        let (group, mut child, stdout_rx, stderr_rx) = spawn_sigterm_ignoring_pipe_holder();

        let (stdout, stderr) =
            post_exit_cleanup_and_drain(&group, stdout_rx, stderr_rx, POST_EXIT_CLEANUP_GRACE)
                .expect("a fresh nominal grace must survive the SIGTERM->SIGKILL escalation");
        assert_eq!(String::from_utf8_lossy(&stdout).trim(), "ok");
        assert!(stderr.is_empty());

        reap_after_escalation(&group, &mut child);
    }

    /// The other half of the property above, made deterministic instead of
    /// a second race against signal/escalation timing — which a PR whose
    /// entire subject is removing scheduling races should not introduce.
    /// An earlier draft reused the SIGTERM-ignoring fixture above with a
    /// too-small `grace`; that still depends on trap-install timing *and*
    /// on the SIGKILL teardown and drain thread being scheduled in a
    /// specific order relative to a 5ms deadline, both real races at low
    /// probability.
    ///
    /// This holder is instead spawned in its own process group via
    /// `detach_std_command` and deliberately **never attached** to `group`.
    /// `terminate_owned_group` only ever signals the group it owns
    /// (`ProcessGroup::terminate`/`kill` operate on `self`'s leader alone,
    /// see `killpg_unix`), so an unattached holder cannot be reached by it
    /// at all — its pipe provably never closes on its own, with no signal
    /// delivery, no trap timing, and no escalation to race. That also
    /// states the actual invariant more precisely: `grace` bounds the
    /// drain wait even when nothing ever closes the pipe, not merely when
    /// closing it happens to take too long.
    #[cfg(unix)]
    #[test]
    fn post_exit_grace_bounds_the_drain_when_nothing_closes_the_pipe() {
        let group = xai_tty_utils::ProcessGroup::new().expect("group");
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("sleep 1000")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        xai_tty_utils::detach_std_command(&mut cmd);
        // Deliberately not attached to `group` -- see the doc comment above.
        #[allow(clippy::disallowed_methods)] // test fixture; killed directly below, not via `group`
        let mut holder = cmd.spawn().expect("spawn holder");
        let stdout = holder.stdout.take().expect("stdout piped");
        let stderr = holder.stderr.take().expect("stderr piped");
        let stdout_rx = spawn_pipe_drain(stdout, "stdout");
        let stderr_rx = spawn_pipe_drain(stderr, "stderr");

        let too_small = Duration::from_millis(5);
        let started = std::time::Instant::now();
        let result = post_exit_cleanup_and_drain(&group, stdout_rx, stderr_rx, too_small);
        let elapsed = started.elapsed();

        let _ = holder.kill();
        let _ = holder.wait();

        assert!(
            result.is_err(),
            "grace must bound the drain wait even when nothing ever closes the pipe: {result:?}"
        );
        // Bounds *how quickly* it fails, not just that it eventually does --
        // a pipe that never closes would still (eventually) produce `Err`
        // even from an implementation that silently ignored `too_small` and
        // fell back to some larger hardcoded bound instead. This is the half
        // of the pair that would actually catch that: the sibling test
        // above cannot, because a larger-than-needed grace still comfortably
        // clears its escalation and stays green.
        assert!(
            elapsed < Duration::from_millis(200),
            "must fail in the requested grace's own ballpark, not after some \
             larger hardcoded wait the parameter was ignored in favour of: \
             took {elapsed:?} for a 5ms grace"
        );
        assert!(
            result
                .unwrap_err()
                .contains("did not close before the query deadline"),
            "must fail as a drain timeout, not some other error"
        );
    }

    /// Fixture for the positive test above: a process that prints, installs
    /// a `SIGTERM` trap, and then blocks forever holding its own piped
    /// stdout/stderr open.
    #[cfg(unix)]
    #[allow(clippy::type_complexity)] // test fixture; a named type would be single-use
    fn spawn_sigterm_ignoring_pipe_holder() -> (
        xai_tty_utils::ProcessGroup,
        std::process::Child,
        std::sync::mpsc::Receiver<Result<Vec<u8>, String>>,
        std::sync::mpsc::Receiver<Result<Vec<u8>, String>>,
    ) {
        let mut group = xai_tty_utils::ProcessGroup::new().expect("group");
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("printf 'ok\\n'; trap '' TERM; sleep 1000")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        xai_tty_utils::detach_std_command(&mut cmd);
        #[allow(clippy::disallowed_methods)] // test fixture; killed via the escalation under test
        let mut child = cmd.spawn().expect("spawn");
        group.attach_std(&child).expect("attach");
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        let stdout_rx = spawn_pipe_drain(stdout, "stdout");
        let stderr_rx = spawn_pipe_drain(stderr, "stderr");
        // The shell installs its trap shortly after exec; signal before that
        // and it dies to the default SIGTERM instead of exercising the
        // escalation these tests need (same reasoning and margin as
        // `group_that_ignores_sigterm_waits_the_grace_and_is_killed`).
        std::thread::sleep(Duration::from_millis(250));
        (group, child, stdout_rx, stderr_rx)
    }

    /// `post_exit_cleanup_and_drain` sends SIGTERM but does not itself wait
    /// on the child, so a SIGKILL'd-but-unreaped process would otherwise
    /// leak as a zombie in the test binary. `terminate_owned_group` may
    /// already have reaped it via the group; either order is fine for
    /// `try_wait`.
    #[cfg(unix)]
    fn reap_after_escalation(group: &xai_tty_utils::ProcessGroup, child: &mut std::process::Child) {
        let _ = group.kill();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait().expect("poll child") {
                Some(_) => return,
                None if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                None => panic!("fixture process survived its own test's escalation"),
            }
        }
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
    #[cfg(unix)]
    #[test]
    #[serial_test::serial(tmux_probe_path)]
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

        let previous_path = std::env::var_os("PATH");
        let mut path = OsString::from(bin.as_os_str());
        path.push(":");
        if let Some(existing) = &previous_path {
            path.push(existing);
        }
        // SAFETY: serialized on `tmux_probe_path`; restored before return.
        unsafe {
            std::env::set_var("PATH", &path);
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_tmux_bounded(TmuxCommand::Version, timeout)
        }));
        match previous_path {
            Some(value) => unsafe {
                std::env::set_var("PATH", value);
            },
            None => unsafe {
                std::env::remove_var("PATH");
            },
        }
        let output = result
            .expect("probe must not panic")
            .expect("a pipe-holding descendant must not turn a clean exit into a drain error");
        assert!(output.status_success, "expected successful status");
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "tmux 3.4");
    }

    #[cfg(unix)]
    #[test]
    fn empty_group_teardown_does_not_wait_out_the_grace() {
        let group = xai_tty_utils::ProcessGroup::new().expect("group");
        let started = std::time::Instant::now();
        terminate_owned_group(&group);
        let elapsed = started.elapsed();
        assert!(
            elapsed < GROUP_EXIT_GRACE / 2,
            "an empty group must not wait out the grace, took {elapsed:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn group_that_ignores_sigterm_waits_the_grace_and_is_killed() {
        let mut group = xai_tty_utils::ProcessGroup::new().expect("group");
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("trap '' TERM; sleep 1000")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        xai_tty_utils::detach_std_command(&mut cmd);
        #[allow(clippy::disallowed_methods)] // test fixture; the test kills it
        let mut child = cmd.spawn().expect("spawn sigterm-ignoring child");
        group.attach_std(&child).expect("attach");
        // The shell installs its trap ~0.3ms after exec. Signal before that
        // and it dies to the default SIGTERM, leaving a zombie that still
        // reports live — the test then passes without exercising SIGKILL.
        std::thread::sleep(Duration::from_millis(250));

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
