//! Agent-scoped interactive PTY manager. PTYs are keyed by `terminalId`,
//! outlive sessions, and multiplex I/O over the existing ACP WebSocket.
//! Shells enroll in the process-global scope as terminal owners, so teardown
//! hangs a live shell up and it forwards that to its jobs. A shell that exits
//! on its own leaves them running, as a terminal does: their process groups are
//! their own and nothing here holds a handle to them.

// A panic here loses a shell: teardown paths run inside `Drop`, where an
// unwind during another unwind aborts the process. Tests panic freely.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::collections::{HashMap, VecDeque};
#[cfg(not(unix))]
use std::io::{Read, Write};
use std::sync::{Arc, LazyLock};

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use xai_acp_lib::AcpAgentGatewaySender as GatewaySender;

use crate::extensions::routing::{TargetClientId, send_routed_notification};
use crate::terminal::{TerminalExtError, TerminalInfo, TerminalStatus};

const NOTIFICATION_METHOD: &str = "x.ai/terminal/pty/notification";
const OUTPUT_RING_BUFFER_SIZE: usize = 256 * 1024;
const OUTPUT_BATCH_INTERVAL_MS: u64 = 16;
const BUSY_POLL_INTERVAL_MS: u64 = 500;
const INPUT_CHANNEL_CAPACITY: usize = 256;
const EXIT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);
/// Collecting a killed shell, not waiting on a live one.
const REAP_GRACE: std::time::Duration = std::time::Duration::from_millis(100);

pub(crate) struct PtySession {
    master: Option<Box<dyn MasterPty + Send>>,
    /// Taken at teardown; the writer loop also watches `cancel`, so sender
    /// clones cannot keep it parked in `recv`.
    input_tx: Option<mpsc::Sender<Vec<u8>>>,
    output_offset: u64,
    output_ring: VecDeque<u8>,
    shell: Shell,
    cwd: Option<String>,
    name: Option<String>,
    created_at: u64,
    rows: u16,
    cols: u16,
    target_client_id: TargetClientId,
    busy: bool,
    gateway: GatewaySender,
    /// Signaled by teardown so the output reader leaves its wait points.
    cancel: CancellationToken,
    /// Signaled by the reader thread once it exits (reader fd released).
    reader_finished: CancellationToken,
    /// Signaled by the writer thread once it exits (writer fd released).
    writer_finished: CancellationToken,
}

/// A shell and the group that can signal it.
///
/// Reaping releases the pid, and a released pid can be recycled, so a signal
/// after the reap may reach a stranger. `Reaped` carries no group, which makes
/// that mistake unrepresentable rather than a rule to remember.
enum Shell {
    Running {
        child: Box<dyn portable_pty::Child + Send + Sync>,
        group: Option<Arc<xai_tty_utils::ProcessGroup>>,
    },
    Reaped(Option<portable_pty::ExitStatus>),
}

impl Shell {
    fn pid(&self) -> Option<u32> {
        match self {
            Shell::Running { child, .. } => child.process_id(),
            Shell::Reaped(_) => None,
        }
    }

    /// Reports whether a hangup was sent.
    fn hangup(&self) -> bool {
        match self {
            Shell::Running {
                group: Some(group), ..
            } if group.wants_hangup() => {
                let _ = group.hangup();
                true
            }
            _ => false,
        }
    }

    fn attach_group(&mut self, enrolled: Arc<xai_tty_utils::ProcessGroup>) {
        if let Shell::Running { group, .. } = self {
            *group = Some(enrolled);
        }
    }

    /// Reap a shell that never reached the registry, where teardown would
    /// otherwise never find it. Same order as [`reap`], and it has to wait:
    /// `killpg` returns before the kernel has zombified the leader, so dropping
    /// straight after would leak it and retire the group with it.
    fn reap_now(&mut self) {
        if self.hangup() && self.wait_exit(xai_tty_utils::HANGUP_GRACE) {
            return;
        }
        self.kill();
        self.wait_exit(REAP_GRACE);
    }

    /// Poll to a deadline. [`wait_for_exit`] is the same wait for a session that
    /// reached the registry, where each turn has to retake the lock.
    fn wait_exit(&mut self, budget: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + budget;
        loop {
            if self.poll_exit() {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(EXIT_POLL_INTERVAL);
        }
    }

    /// The shell leads its group, so one non-blocking `killpg` reaches the tree.
    fn kill(&mut self) {
        match self {
            Shell::Running {
                group: Some(group), ..
            } => {
                let _ = group.kill();
            }
            // No pid to enroll, so the child handle is all there is.
            Shell::Running { child, .. } => {
                let _ = child.kill();
            }
            Shell::Reaped(_) => {}
        }
    }

    /// Whether the shell is gone. An error counts as gone: the pid is not ours
    /// to signal either way, and leaving the group attached would let teardown
    /// signal a recycled one.
    fn poll_exit(&mut self) -> bool {
        if let Shell::Running { child, .. } = self {
            match child.try_wait() {
                Ok(None) => return false,
                Ok(Some(status)) => *self = Shell::Reaped(Some(status)),
                Err(_) => *self = Shell::Reaped(None),
            }
        }
        true
    }

    fn exit_code(&self) -> Option<i32> {
        match self {
            Shell::Reaped(Some(status)) => Some(status.exit_code() as i32),
            _ => None,
        }
    }
}

/// A shell not yet in the registry, where teardown would never find it.
/// Dropping reaps it; [`Self::into_registered`] hands it to the registry
/// instead.
///
/// Disarming leaves [`Shell::Reaped`] behind rather than an empty slot, so the
/// guard has no state in which its own field is missing.
struct UnregisteredShell(Shell);

impl UnregisteredShell {
    fn new(child: Box<dyn portable_pty::Child + Send + Sync>) -> Self {
        Self(Shell::Running { child, group: None })
    }

    fn pid(&self) -> Option<u32> {
        self.0.pid()
    }

    fn attach_group(&mut self, enrolled: Arc<xai_tty_utils::ProcessGroup>) {
        self.0.attach_group(enrolled);
    }

    /// Disarms the guard. The caller must reach the registry without awaiting,
    /// or it reopens the window this type closes.
    fn into_registered(mut self) -> Shell {
        self.disarm()
    }

    fn disarm(&mut self) -> Shell {
        std::mem::replace(&mut self.0, Shell::Reaped(None))
    }
}

impl Drop for UnregisteredShell {
    fn drop(&mut self) {
        let mut shell = self.disarm();
        // `reap_now` blocks through its grace waits, so keep it off an async
        // thread. Not the runtime's blocking pool though: a task still queued
        // there at shutdown is dropped unrun, taking the group with it and
        // leaving the scope holding a dead `Weak`.
        if tokio::runtime::Handle::try_current().is_ok() {
            std::thread::spawn(move || shell.reap_now());
        } else {
            shell.reap_now();
        }
    }
}

type PtyMap = HashMap<String, Arc<Mutex<PtySession>>>;

static PTY_REGISTRY: LazyLock<Mutex<PtyMap>> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) async fn get_pty(pty_id: &str) -> Option<Arc<Mutex<PtySession>>> {
    PTY_REGISTRY.lock().await.get(pty_id).cloned()
}

pub(crate) async fn require_pty(
    terminal_id: &str,
) -> Result<Arc<Mutex<PtySession>>, TerminalExtError> {
    get_pty(terminal_id)
        .await
        .ok_or_else(|| TerminalExtError::NotInteractive {
            terminal_id: terminal_id.into(),
        })
}

pub(crate) async fn create_pty(
    shell: Option<&str>,
    cwd: Option<&str>,
    env: HashMap<String, String>,
    rows: u16,
    cols: u16,
    name: Option<&str>,
    gateway: GatewaySender,
    target_client_id: TargetClientId,
) -> Result<String, TerminalExtError> {
    let pty_id = uuid::Uuid::now_v7().to_string();

    let pty_system = native_pty_system();
    let size = PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    };

    let pair = pty_system
        .openpty(size)
        .map_err(|e| TerminalExtError::Internal(format!("failed to open pty: {e}")))?;

    let (shell_path, shell_args) = resolve_pty_shell(shell);

    let mut cmd = CommandBuilder::new(&shell_path);
    for arg in &shell_args {
        cmd.arg(arg);
    }

    if let Some(dir) = cwd {
        cmd.cwd(dir);
    } else if let Ok(dir) = std::env::current_dir() {
        cmd.cwd(dir);
    }

    for (k, v) in &env {
        cmd.env(k, v);
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("LANG", "en_US.UTF-8");
    cmd.env("LC_ALL", "en_US.UTF-8");

    // Enrolled below, and reaped by the guard until it reaches the registry.
    #[allow(clippy::disallowed_methods)]
    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| TerminalExtError::Internal(format!("failed to spawn shell: {e}")))?;

    let mut shell = UnregisteredShell::new(child);
    if let Some(pid) = shell.pid() {
        // `enroll_terminal_pid` reaps with a grace wait if it loses the close
        // race, so run it off-task.
        let enrolled = tokio::task::spawn_blocking(move || {
            xai_tty_utils::global_process_scope().enroll_terminal_pid(pid)
        })
        .await
        .map_err(|e| TerminalExtError::Internal(format!("enroll task failed: {e}")))?
        .map_err(|e| TerminalExtError::Internal(format!("failed to enroll shell: {e}")))?;
        shell.attach_group(enrolled);
    }

    // #132: Unix owns a master dup for the cancellable poll-reader. Do NOT set
    // O_NONBLOCK — that flag is per open-file-description and would make the
    // writer nonblocking too (shared OFD via dup), which previously left
    // `write_all` retrying WouldBlock forever on a full PTY buffer. Closing or
    // dup2'ing the reader's fd from teardown also fails on Darwin: both block
    // while another thread sits in read(2). Non-unix keeps the portable-pty
    // reader trait object.
    #[cfg(unix)]
    let (reader_fd, writer_fd) = {
        let raw = pair
            .master
            .as_raw_fd()
            .ok_or_else(|| TerminalExtError::Internal("pty master has no raw fd".to_string()))?;
        let reader_fd = dup_fd(raw).map_err(|e| {
            TerminalExtError::Internal(format!("failed to prepare pty reader: {e}"))
        })?;
        let writer_fd = dup_fd(raw).map_err(|e| {
            TerminalExtError::Internal(format!("failed to prepare pty writer: {e}"))
        })?;
        (reader_fd, writer_fd)
    };
    #[cfg(not(unix))]
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| TerminalExtError::Internal(format!("failed to clone pty reader: {e}")))?;

    #[cfg(not(unix))]
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| TerminalExtError::Internal(format!("failed to take pty writer: {e}")))?;

    let cancel = CancellationToken::new();
    let reader_finished = CancellationToken::new();
    let writer_finished = CancellationToken::new();

    let (input_tx, input_rx) = mpsc::channel(INPUT_CHANNEL_CAPACITY);
    #[cfg(unix)]
    spawn_pty_input_loop_unix(writer_fd, input_rx, cancel.clone(), writer_finished.clone());
    #[cfg(not(unix))]
    spawn_pty_input_loop_blocking(writer, input_rx, cancel.clone(), writer_finished.clone());

    let created_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let resolved_cwd = cwd.map(|s| s.to_string()).or_else(|| {
        std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().to_string())
    });

    let resolved_name = name.map(|s| s.to_string()).or_else(|| {
        // Default to cwd basename, fall back to shell basename.
        resolved_cwd
            .as_deref()
            .and_then(|p| {
                std::path::Path::new(p)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
            })
            .or_else(|| {
                std::path::Path::new(&shell_path)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
            })
    });

    // Taking the lock first means nothing can await between disarming the guard
    // and the insert that gives teardown another way to reach the shell.
    let mut registry = PTY_REGISTRY.lock().await;

    // The scope can close during the setup above, and teardown has already run
    // by then: publishing here would advertise a shell it just killed.
    if xai_tty_utils::global_process_scope().is_closed() {
        return Err(TerminalExtError::Internal(
            "process scope closed while the shell was starting".to_string(),
        ));
    }

    let entry = Arc::new(Mutex::new(PtySession {
        master: Some(pair.master),
        input_tx: Some(input_tx),
        output_offset: 0,
        output_ring: VecDeque::with_capacity(OUTPUT_RING_BUFFER_SIZE),
        shell: shell.into_registered(),
        cwd: resolved_cwd,
        name: resolved_name,
        created_at,
        rows,
        cols,
        target_client_id,
        busy: false,
        gateway: gateway.clone(),
        cancel: cancel.clone(),
        reader_finished: reader_finished.clone(),
        writer_finished: writer_finished.clone(),
    }));
    registry.insert(pty_id.clone(), entry.clone());
    drop(registry);

    let pty_id_clone = pty_id.clone();
    #[cfg(unix)]
    tokio::task::spawn_local(run_pty_output_loop_unix(
        reader_fd,
        entry,
        pty_id_clone,
        gateway,
        cancel,
        reader_finished,
    ));
    #[cfg(not(unix))]
    tokio::task::spawn_local(run_pty_output_loop_blocking(
        reader,
        entry,
        pty_id_clone,
        gateway,
        cancel,
        reader_finished,
    ));

    Ok(pty_id)
}

/// Wait for one input chunk, but also wake when `cancel` fires.
///
/// Outside a runtime this falls back to polling `try_recv` with a short sleep.
fn recv_input_chunk(
    runtime: Option<&tokio::runtime::Handle>,
    input_rx: &mut mpsc::Receiver<Vec<u8>>,
    cancel: &CancellationToken,
) -> Option<Vec<u8>> {
    if let Some(runtime) = runtime {
        return runtime.block_on(async {
            tokio::select! {
                _ = cancel.cancelled() => None,
                chunk = input_rx.recv() => chunk,
            }
        });
    }

    loop {
        if cancel.is_cancelled() {
            return None;
        }
        match input_rx.try_recv() {
            Ok(chunk) => return Some(chunk),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                if input_rx.is_closed() {
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return None,
        }
    }
}

/// Cancels its token on drop, used for reader/writer lifecycle signals.
struct SignalFinished(CancellationToken);

impl Drop for SignalFinished {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// #132 unix writer: poll for writable + cancel checks around each bounded
/// write syscall so shutdown is not gated on sender-drop or `write_all`.
#[cfg(unix)]
fn spawn_pty_input_loop_unix(
    writer_fd: std::os::fd::OwnedFd,
    input_rx: mpsc::Receiver<Vec<u8>>,
    cancel: CancellationToken,
    writer_finished: CancellationToken,
) {
    use std::os::fd::AsRawFd;

    let runtime = tokio::runtime::Handle::try_current().ok();
    std::thread::spawn(move || {
        let _signal_writer_finished = SignalFinished(writer_finished);
        let runtime = runtime.as_ref();
        let mut input_rx = input_rx;
        while let Some(mut chunk) = recv_input_chunk(runtime, &mut input_rx, &cancel) {
            while let Ok(more) = input_rx.try_recv() {
                chunk.extend_from_slice(&more);
            }
            if write_pty_fd_cancelable(writer_fd.as_raw_fd(), &chunk, &cancel).is_err() {
                break;
            }
        }
    });
}

/// #132 non-unix writer: still uses the portable-pty writer trait object.
#[cfg(not(unix))]
fn spawn_pty_input_loop_blocking(
    mut writer: Box<dyn Write + Send>,
    input_rx: mpsc::Receiver<Vec<u8>>,
    cancel: CancellationToken,
    writer_finished: CancellationToken,
) {
    let runtime = tokio::runtime::Handle::try_current().ok();
    std::thread::spawn(move || {
        let _signal_writer_finished = SignalFinished(writer_finished);
        let runtime = runtime.as_ref();
        let mut input_rx = input_rx;
        while let Some(mut chunk) = recv_input_chunk(runtime, &mut input_rx, &cancel) {
            while let Ok(more) = input_rx.try_recv() {
                chunk.extend_from_slice(&more);
            }
            if writer.write_all(&chunk).is_err() {
                break;
            }
            if writer.flush().is_err() {
                break;
            }
        }
    });
}

#[cfg(unix)]
fn dup_fd(raw: std::os::fd::RawFd) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{FromRawFd, OwnedFd, RawFd};

    let duped: RawFd = unsafe { libc::fcntl(raw, libc::F_DUPFD_CLOEXEC, 0) };
    if duped < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `duped` is an open fd we uniquely own.
    Ok(unsafe { OwnedFd::from_raw_fd(duped) })
}

/// Read from a PTY master, mapping slave-closed `EIO` to EOF like portable-pty.
#[cfg(unix)]
fn read_pty_fd(fd: std::os::fd::RawFd, buf: &mut [u8]) -> std::io::Result<usize> {
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EIO) {
            return Ok(0);
        }
        return Err(err);
    }
    Ok(n as usize)
}

#[cfg(unix)]
fn write_pty_fd(fd: std::os::fd::RawFd, buf: &[u8]) -> std::io::Result<usize> {
    let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(n as usize)
}

#[cfg(unix)]
fn write_pty_fd_cancelable(
    fd: std::os::fd::RawFd,
    mut buf: &[u8],
    cancel: &CancellationToken,
) -> std::io::Result<()> {
    use std::io::ErrorKind;

    while !buf.is_empty() {
        if cancel.is_cancelled() {
            return Err(std::io::Error::new(
                ErrorKind::Interrupted,
                "pty input cancelled",
            ));
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        let pret = unsafe { libc::poll(&mut pfd, 1, IO_POLL_TIMEOUT_MS) };
        if pret < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if pret == 0 {
            continue;
        }
        let to_write = buf.len().min(4096);
        match write_pty_fd(fd, &buf[..to_write]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    ErrorKind::WriteZero,
                    "pty writer produced a zero-byte write",
                ));
            }
            Ok(n) => buf = &buf[n..],
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

async fn ingest_output(pty: &Arc<Mutex<PtySession>>, pending: &mut Vec<u8>, data: &[u8]) {
    {
        let mut session = pty.lock().await;
        session.output_ring.extend(data.iter().copied());
        if session.output_ring.len() > OUTPUT_RING_BUFFER_SIZE {
            let excess = session.output_ring.len() - OUTPUT_RING_BUFFER_SIZE;
            session.output_ring.drain(..excess);
        }
        session.output_offset += data.len() as u64;
    }
    pending.extend_from_slice(data);
}

/// #132: reader/writer poll cadence for checking cancellation while keeping
/// idle wakeups bounded.
const IO_POLL_TIMEOUT_MS: i32 = 50;

/// #132: reads PTY output via a cancellable `poll(2)` loop on a blocking
/// thread, batches on a 16ms tick, and sends notifications.
///
/// Cancellation is an explicit lifecycle signal — not EOF — so teardown does
/// not depend on every master dup having closed. The fd stays **blocking**:
/// we never set `O_NONBLOCK` (shared with the writer OFD). `poll` with a
/// short timeout is what makes the reader cancellable without that flag.
///
/// Also samples the foreground process group on a slower tick and pushes
/// `process_started`/`process_ended` on idle↔busy transitions — time-based
/// rather than output-driven because a busy process can be silent.
#[cfg(unix)]
async fn run_pty_output_loop_unix(
    reader_fd: std::os::fd::OwnedFd,
    pty: Arc<Mutex<PtySession>>,
    pty_id: String,
    gateway: GatewaySender,
    cancel: CancellationToken,
    reader_finished: CancellationToken,
) {
    use std::io::ErrorKind;
    use std::os::fd::AsRawFd;
    use tokio::sync::mpsc;
    use tokio::time::{Duration, interval};

    let (data_tx, mut data_rx) = mpsc::channel::<Vec<u8>>(64);

    let cancel_reader = cancel.clone();
    // #132: see spawn_pty_input_loop — avoid BlockingPool::shutdown wedging
    // on an abandoned poll loop when close_pty never ran.
    std::thread::spawn(move || {
        let _signal_reader_finished = SignalFinished(reader_finished);
        let reader_fd = reader_fd;
        let mut buf = [0u8; 4096];
        while !cancel_reader.is_cancelled() {
            let mut pfd = libc::pollfd {
                fd: reader_fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let pret = unsafe { libc::poll(&mut pfd, 1, IO_POLL_TIMEOUT_MS) };
            if cancel_reader.is_cancelled() {
                break;
            }
            if pret < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            if pret == 0 {
                continue;
            }
            // Readable, hangup, or error — try to drain; 0/EIO means EOF.
            match read_pty_fd(reader_fd.as_raw_fd(), &mut buf) {
                Ok(0) => {
                    break;
                }
                Ok(n) => {
                    let mut chunk = buf[..n].to_vec();
                    loop {
                        if cancel_reader.is_cancelled() {
                            return;
                        }
                        match data_tx.try_send(chunk) {
                            Ok(()) => break,
                            Err(mpsc::error::TrySendError::Full(returned)) => {
                                chunk = returned;
                                std::thread::sleep(std::time::Duration::from_millis(1));
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                return;
                            }
                        }
                    }
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                // Blocking fd after poll said ready: treat other errors as stop.
                Err(_) => break,
            }
        }
    });

    let mut pending = Vec::new();
    let mut tick = interval(Duration::from_millis(OUTPUT_BATCH_INTERVAL_MS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut busy_tick = interval(Duration::from_millis(BUSY_POLL_INTERVAL_MS));
    busy_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,

            chunk = data_rx.recv() => {
                match chunk {
                    Some(data) => {
                        ingest_output(&pty, &mut pending, &data).await;
                    }
                    None => break,
                }
            }

            _ = tick.tick() => {
                if !pending.is_empty() {
                    flush_output(&pty, &pty_id, &mut pending, &gateway).await;
                }
            }

            _ = busy_tick.tick() => {
                sample_busy_transition(&pty, &pty_id).await;
            }
        }
    }

    if !pending.is_empty() {
        flush_output(&pty, &pty_id, &mut pending, &gateway).await;
    }

    // Dropping the receiver makes try_send fail closed. `reader_finished` is
    // signaled by the reader thread itself once that fd has actually closed.
    drop(data_rx);

    wait_and_notify_exit(&pty, &pty_id, &gateway).await;
}

/// Non-unix fallback: blocking reader thread. Cancellation cannot interrupt
/// `read(2)` portably here; prefer the unix poll path.
#[cfg(not(unix))]
async fn run_pty_output_loop_blocking(
    reader: Box<dyn Read + Send>,
    pty: Arc<Mutex<PtySession>>,
    pty_id: String,
    gateway: GatewaySender,
    cancel: CancellationToken,
    reader_finished: CancellationToken,
) {
    use tokio::sync::mpsc;
    use tokio::time::{Duration, interval};

    let (data_tx, mut data_rx) = mpsc::channel::<Vec<u8>>(64);

    let cancel_reader = cancel.clone();
    tokio::task::spawn_blocking(move || {
        let _signal_reader_finished = SignalFinished(reader_finished);
        let mut reader = reader;
        let mut buf = [0u8; 4096];
        loop {
            if cancel_reader.is_cancelled() {
                break;
            }
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    // Prefer try_send so a full channel cannot park this thread
                    // forever after the receiver stops draining on cancel.
                    let mut chunk = buf[..n].to_vec();
                    loop {
                        if cancel_reader.is_cancelled() {
                            return;
                        }
                        match data_tx.try_send(chunk) {
                            Ok(()) => break,
                            Err(mpsc::error::TrySendError::Full(returned)) => {
                                chunk = returned;
                                std::thread::sleep(std::time::Duration::from_millis(1));
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => return,
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut pending = Vec::new();
    let mut tick = interval(Duration::from_millis(OUTPUT_BATCH_INTERVAL_MS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut busy_tick = interval(Duration::from_millis(BUSY_POLL_INTERVAL_MS));
    busy_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,

            chunk = data_rx.recv() => {
                match chunk {
                    Some(data) => {
                        ingest_output(&pty, &mut pending, &data).await;
                    }
                    None => break,
                }
            }

            _ = tick.tick() => {
                if !pending.is_empty() {
                    flush_output(&pty, &pty_id, &mut pending, &gateway).await;
                }
            }

            _ = busy_tick.tick() => {
                sample_busy_transition(&pty, &pty_id).await;
            }
        }
    }

    if !pending.is_empty() {
        flush_output(&pty, &pty_id, &mut pending, &gateway).await;
    }

    // Dropping the receiver makes try_send fail closed.
    drop(data_rx);

    wait_and_notify_exit(&pty, &pty_id, &gateway).await;
}

/// The reader ending does not prove the shell did, so poll rather than
/// blocking on `wait`: teardown needs this lock to hang the shell up.
async fn wait_and_notify_exit(pty: &Arc<Mutex<PtySession>>, pty_id: &str, gateway: &GatewaySender) {
    let (exit_code, signal, target_client_id, was_busy) = loop {
        {
            let mut session = pty.lock().await;
            let target_client_id = session.target_client_id.clone();
            let was_busy = session.busy;
            if session.shell.poll_exit() {
                break (
                    session.shell.exit_code(),
                    None::<String>,
                    target_client_id,
                    was_busy,
                );
            }
        }
        tokio::time::sleep(EXIT_POLL_INTERVAL).await;
    };

    if was_busy {
        send_busy_notification(pty_id, false, &target_client_id, gateway);
    }

    send_routed_notification(
        gateway,
        NOTIFICATION_METHOD,
        serde_json::json!({
            "terminalId": pty_id,
            "type": "exit",
            "exitCode": exit_code,
            "signal": signal,
        }),
        &target_client_id,
    );
}

/// Whether a command is running, rather than the shell sitting at its prompt.
///
/// A shell that `exec`s a program in place keeps its pid and pgid, so that
/// program reads as idle. Separating it from a real prompt needs per-OS
/// process inspection.
#[cfg(unix)]
fn session_has_foreground_process(session: &PtySession) -> bool {
    let Some(foreground_pgid) = session
        .master
        .as_ref()
        .and_then(|m| m.process_group_leader())
    else {
        return false;
    };
    match session.shell.pid() {
        Some(shell_pid) => i64::from(foreground_pgid) != i64::from(shell_pid),
        None => false,
    }
}

/// `tcgetpgrp` has no ConPTY equivalent (`process_group_leader` is
/// unix-only in portable-pty), so non-unix PTYs never report a foreground
/// process and clients close terminals without confirmation.
#[cfg(not(unix))]
fn session_has_foreground_process(_session: &PtySession) -> bool {
    false
}

/// Emits `process_started` / `process_ended` only on idle↔busy transitions so
/// a steady state never repeats notifications.
async fn sample_busy_transition(pty: &Arc<Mutex<PtySession>>, pty_id: &str) {
    let mut session = pty.lock().await;
    let now_busy = session_has_foreground_process(&session);
    if now_busy == session.busy {
        return;
    }
    session.busy = now_busy;
    send_busy_notification(
        pty_id,
        now_busy,
        &session.target_client_id,
        &session.gateway,
    );
}

fn send_busy_notification(
    pty_id: &str,
    busy: bool,
    target_client_id: &TargetClientId,
    gateway: &GatewaySender,
) {
    send_routed_notification(
        gateway,
        NOTIFICATION_METHOD,
        serde_json::json!({
            "terminalId": pty_id,
            "type": if busy { "process_started" } else { "process_ended" },
        }),
        target_client_id,
    );
}

async fn flush_output(
    pty: &Arc<Mutex<PtySession>>,
    pty_id: &str,
    pending: &mut Vec<u8>,
    gateway: &GatewaySender,
) {
    use base64::Engine as _;

    let (output_offset, target_client_id) = {
        let session = pty.lock().await;
        (session.output_offset, session.target_client_id.clone())
    };
    let b64 = base64::engine::general_purpose::STANDARD.encode(&*pending);
    pending.clear();

    send_routed_notification(
        gateway,
        NOTIFICATION_METHOD,
        serde_json::json!({
            "terminalId": pty_id,
            "type": "output",
            "data": b64,
            "outputOffset": output_offset,
        }),
        &target_client_id,
    );
}

pub(crate) async fn write_pty_input(pty_id: &str, data: &[u8]) -> Result<(), TerminalExtError> {
    let entry = require_pty(pty_id).await?;
    let input_tx = entry.lock().await.input_tx.clone();
    input_tx
        .ok_or_else(|| TerminalExtError::InputClosed {
            terminal_id: pty_id.into(),
        })?
        .send(data.to_vec())
        .await
        .map_err(|_| TerminalExtError::InputClosed {
            terminal_id: pty_id.into(),
        })?;
    sample_busy_transition(&entry, pty_id).await;
    Ok(())
}

pub(crate) async fn resize_pty(pty_id: &str, rows: u16, cols: u16) -> Result<(), TerminalExtError> {
    let entry = require_pty(pty_id).await?;
    let mut session = entry.lock().await;
    let Some(master) = session.master.as_ref() else {
        return Ok(());
    };
    master
        .resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| TerminalExtError::Internal(format!("failed to resize pty: {e}")))?;
    session.rows = rows;
    session.cols = cols;
    Ok(())
}

pub(crate) async fn is_exited(pty_id: &str) -> bool {
    match get_pty(pty_id).await {
        Some(entry) => entry.lock().await.shell.poll_exit(),
        None => true,
    }
}

pub(crate) async fn close_pty(pty_id: &str) -> Result<(), String> {
    let entry = { PTY_REGISTRY.lock().await.remove(pty_id) };
    if let Some(entry) = entry {
        shutdown_pty_session(entry).await?;
    }
    Ok(())
}

/// #132: teardown order (cancellation is explicit; EOF is not the protocol):
/// 1. session already removed from the registry by the caller
/// 2. hang up / kill the child **while** the reader still holds a master dup,
///    so a job-control shell can forward SIGHUP to background grandchildren
/// 3. signal reader cancellation; stop the writer; release the stored master
/// 4. await reader termination outside the session lock
///
/// Closing every master dup *before* hangup makes the kernel SIGHUP the shell
/// via PTY teardown alone, which does not give bash a chance to reap its
/// job-control children — that is why hangup is ordered first. Without the
/// cancel step the reader's poll loop stays parked until timeout/`cancel`, and
/// a bare `spawn_blocking` `read(2)` (upstream) wedged `#[tokio::test]` runtime
/// drop for as long as any descendant held the slave.
async fn shutdown_pty_session(entry: Arc<Mutex<PtySession>>) -> Result<(), String> {
    let (reader_finished, writer_finished) = {
        let session = entry.lock().await;
        (
            session.reader_finished.clone(),
            session.writer_finished.clone(),
        )
    };

    // Hang up while master + reader fd are still open. `killpg(SIGHUP)` alone
    // is not enough if the shell has already lost its controlling terminal.
    tokio::task::spawn_blocking({
        let entry = entry.clone();
        move || hangup_or_kill_shell(&entry)
    })
    .await
    .map_err(|e| format!("close task failed: {e}"))?;

    {
        let mut session = entry.lock().await;
        session.cancel.cancel();
        session.master.take();
        // Drops the session-owned sender; writer exit is keyed off cancel.
        session.input_tx.take();
    }

    // On unix the reader/writer signals are tied to thread exit (and fd drop).
    // Non-unix may still block in a read/write syscall until teardown effects.
    #[cfg(unix)]
    {
        let _ = tokio::join!(reader_finished.cancelled(), writer_finished.cancelled());
    }
    #[cfg(not(unix))]
    {
        let _ = (reader_finished, writer_finished);
    }

    Ok(())
}

/// Hang up a terminal-owning shell (grace for job-control forward), else kill.
/// Does not tear down master/reader fds — the async shutdown path does that
/// after this returns so grandchildren can still receive a forwarded SIGHUP.
fn hangup_or_kill_shell(entry: &Arc<Mutex<PtySession>>) {
    let hung_up = entry.blocking_lock().shell.hangup();
    if hung_up && wait_for_exit(entry, xai_tty_utils::HANGUP_GRACE) {
        return;
    }
    entry.blocking_lock().shell.kill();
    wait_for_exit(entry, REAP_GRACE);
}

/// Polls rather than blocking on `wait`, re-locking each turn: a shell that
/// ignores its hangup must not wedge teardown or stall the pty's own I/O.
fn wait_for_exit(entry: &Arc<Mutex<PtySession>>, budget: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + budget;
    loop {
        if entry.blocking_lock().shell.poll_exit() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(EXIT_POLL_INTERVAL);
    }
}

/// Called on agent disconnect to clean up all PTYs.
pub(crate) async fn close_all() {
    let entries: Vec<Arc<Mutex<PtySession>>> = {
        let mut reg = PTY_REGISTRY.lock().await;
        reg.drain().map(|(_, v)| v).collect()
    };
    for entry in entries {
        let _ = shutdown_pty_session(entry).await;
    }
}

/// Explicit `shell` param, then `$SHELL`, then the platform default. Windows
/// has no `$SHELL`, so it uses the `detect_windows_shell` cascade.
fn resolve_pty_shell(shell: Option<&str>) -> (String, Vec<String>) {
    if let Some(s) = shell {
        return (s.to_string(), vec![]);
    }

    #[cfg(unix)]
    {
        let path = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        (path, vec!["-l".to_string()])
    }

    #[cfg(not(unix))]
    {
        use xai_grok_config::shell::{WindowsShell, detect_windows_shell};
        match detect_windows_shell() {
            WindowsShell::GitBash(path) => (path.clone(), vec!["-l".to_string()]),
            WindowsShell::Pwsh => ("pwsh".to_string(), vec!["-NoLogo".to_string()]),
            WindowsShell::PowerShell => ("powershell.exe".to_string(), vec!["-NoLogo".to_string()]),
            WindowsShell::Cmd => ("cmd.exe".to_string(), vec![]),
        }
    }
}

pub(crate) async fn list_ptys() -> Vec<TerminalInfo> {
    let entries: Vec<(String, Arc<Mutex<PtySession>>)> = {
        let reg = PTY_REGISTRY.lock().await;
        reg.iter()
            .map(|(id, entry)| (id.clone(), entry.clone()))
            .collect()
    };

    let mut result = Vec::with_capacity(entries.len());
    for (id, entry) in entries {
        let mut session = entry.lock().await;
        let (status, exit_code) = if session.shell.poll_exit() {
            (TerminalStatus::Exited, session.shell.exit_code())
        } else {
            (TerminalStatus::Connected, None)
        };
        result.push(TerminalInfo {
            terminal_id: id,
            status,
            interactive: true,
            name: session.name.clone(),
            exit_code,
            cwd: session.cwd.clone(),
            output_offset: session.output_offset,
            created_at: session.created_at,
        });
    }
    result
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PtyLoadResult {
    pub terminal_id: String,
    pub rows: u16,
    pub cols: u16,
    pub exited: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

/// Reconnect to a PTY, replaying the ring buffer so the client can reset its
/// VTE emulator and feed all bytes from scratch. An exited PTY still loads, so
/// its final output stays readable.
pub(crate) async fn load(
    pty_id: &str,
    gateway: &GatewaySender,
    target_client_id: TargetClientId,
) -> Result<PtyLoadResult, TerminalExtError> {
    let entry = require_pty(pty_id).await?;
    let (replay, output_offset, exited, exit_code, rows, cols, busy) = {
        let mut session = entry.lock().await;
        session.target_client_id = target_client_id.clone();
        // Two facts, not one: a shell whose wait failed is gone with no code.
        let exited = session.shell.poll_exit();
        let exit_code = session.shell.exit_code();
        let busy = session_has_foreground_process(&session);
        session.busy = busy;
        (
            session.output_ring.iter().copied().collect::<Vec<u8>>(),
            session.output_offset,
            exited,
            exit_code,
            session.rows,
            session.cols,
            busy,
        )
    };

    if !replay.is_empty() {
        use base64::Engine as _;
        send_routed_notification(
            gateway,
            NOTIFICATION_METHOD,
            serde_json::json!({
                "terminalId": pty_id,
                "type": "output",
                "data": base64::engine::general_purpose::STANDARD.encode(&replay),
                "outputOffset": output_offset,
                "isReplay": true,
            }),
            &target_client_id,
        );
    }

    if exited {
        send_routed_notification(
            gateway,
            NOTIFICATION_METHOD,
            serde_json::json!({
                "terminalId": pty_id,
                "type": "exit",
                "exitCode": exit_code,
                "isReplay": true,
            }),
            &target_client_id,
        );
    } else {
        send_busy_notification(pty_id, busy, &target_client_id, gateway);
    }

    Ok(PtyLoadResult {
        terminal_id: pty_id.to_string(),
        rows,
        cols,
        exited,
        exit_code,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Duration;

    use agent_client_protocol as acp;
    use xai_acp_lib::acp_gateway;

    type RecordedNotifications = Rc<RefCell<Vec<(String, serde_json::Value)>>>;

    struct RecordingClient {
        notifications: RecordedNotifications,
    }

    #[async_trait::async_trait(?Send)]
    impl acp::Client for RecordingClient {
        async fn request_permission(
            &self,
            _: acp::RequestPermissionRequest,
        ) -> acp::Result<acp::RequestPermissionResponse> {
            unimplemented!()
        }

        async fn session_notification(&self, _: acp::SessionNotification) -> acp::Result<()> {
            Ok(())
        }

        async fn ext_notification(&self, args: acp::ExtNotification) -> acp::Result<()> {
            let params: serde_json::Value =
                serde_json::from_str(args.params.get()).unwrap_or_default();
            self.notifications
                .borrow_mut()
                .push((args.method.to_string(), params));
            Ok(())
        }
    }

    fn recording_gateway() -> (GatewaySender, RecordedNotifications) {
        let notifications: RecordedNotifications = Rc::new(RefCell::new(Vec::new()));
        let (sender, receiver) = acp_gateway::<acp::AgentSide, _>(RecordingClient {
            notifications: notifications.clone(),
        });
        tokio::task::spawn_local(receiver.run());
        (sender, notifications)
    }

    async fn create_test_pty(gateway: GatewaySender) -> String {
        // #132: give the child a clean HOME so interactive bash does not load
        // the developer's Fig/iTerm shell integration (OSC 697), which swallows
        // or delays command output and makes these upstream tests fail before
        // close_pty — masking the runtime-drop hang under an assertion failure.
        let home = std::env::temp_dir().join("medley-pty-test-home");
        let _ = std::fs::create_dir_all(&home);
        let env = HashMap::from([
            ("ENV".to_string(), String::new()),
            ("HOME".to_string(), home.to_string_lossy().into_owned()),
            ("TERM".to_string(), "xterm-256color".to_string()),
        ]);
        create_pty(
            Some("/bin/bash"),
            None,
            env,
            24,
            80,
            None,
            gateway,
            TargetClientId::None,
        )
        .await
        .expect("create test pty")
    }

    fn busy_event_types(notifications: &RecordedNotifications, pty_id: &str) -> Vec<String> {
        notifications
            .borrow()
            .iter()
            .filter(|(method, params)| {
                method == NOTIFICATION_METHOD && params["terminalId"] == pty_id
            })
            .filter_map(|(_, params)| match params["type"].as_str() {
                Some(t @ ("process_started" | "process_ended")) => Some(t.to_string()),
                _ => None,
            })
            .collect()
    }

    async fn wait_for_busy_events(
        notifications: &RecordedNotifications,
        pty_id: &str,
        expected: &[&str],
        deadline: Duration,
    ) -> Vec<String> {
        let started = tokio::time::Instant::now();
        loop {
            let events = busy_event_types(notifications, pty_id);
            if events == expected {
                return events;
            }
            if started.elapsed() > deadline {
                return events;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    // `dev_t` and `ino_t` are already `u64` on Linux and narrower on Darwin, so
    // exactly one platform sees these casts as redundant. Widening is correct on
    // both and there is no `From` impl that covers the signed Darwin `dev_t`, so
    // the lint is allowed here rather than the cast removed -- removing it breaks
    // the build on macOS, which is where this test was written.
    #[allow(clippy::unnecessary_cast)]
    fn stat_fd_identity(fd: std::os::fd::RawFd) -> std::io::Result<(u64, u64)> {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        let rc = unsafe { libc::fstat(fd, stat.as_mut_ptr()) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let stat = unsafe { stat.assume_init() };
        Ok((stat.st_dev as u64, stat.st_ino as u64))
    }

    #[test]
    fn dup_fd_is_not_inherited_by_exec_child() {
        use std::os::fd::AsRawFd;
        use std::process::Command;

        const PROBE_FD_ENV: &str = "XAI_GROK_SHELL_DUP_FD_PROBE";
        const PROBE_DEV_ENV: &str = "XAI_GROK_SHELL_DUP_FD_DEV";
        const PROBE_INO_ENV: &str = "XAI_GROK_SHELL_DUP_FD_INO";
        const SELF_TEST: &str =
            "terminal::pty_session::tests::dup_fd_is_not_inherited_by_exec_child";

        if let Ok(probe_fd) = std::env::var(PROBE_FD_ENV) {
            let fd: std::os::fd::RawFd = probe_fd.parse().expect("probe fd parse");
            let expected_dev: u64 = std::env::var(PROBE_DEV_ENV)
                .expect("missing probe dev")
                .parse()
                .expect("probe dev parse");
            let expected_ino: u64 = std::env::var(PROBE_INO_ENV)
                .expect("missing probe ino")
                .parse()
                .expect("probe ino parse");

            match stat_fd_identity(fd) {
                Ok((dev, ino)) => {
                    assert_ne!(
                        (dev, ino),
                        (expected_dev, expected_ino),
                        "fd {fd} was inherited across exec"
                    );
                }
                Err(err) => {
                    assert_eq!(
                        err.raw_os_error(),
                        Some(libc::EBADF),
                        "expected fd {fd} to be closed after exec, got {err}"
                    );
                }
            }
            return;
        }

        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let raw = pair.master.as_raw_fd().expect("raw pty master fd");
        let duped = dup_fd(raw).expect("dup pty master");
        let (dev, ino) = stat_fd_identity(duped.as_raw_fd()).expect("stat duped fd");

        let test_binary = std::env::current_exe().expect("test binary path");
        let status = Command::new(test_binary)
            .arg("--exact")
            .arg(SELF_TEST)
            .arg("--test-threads=1")
            .env(PROBE_FD_ENV, duped.as_raw_fd().to_string())
            .env(PROBE_DEV_ENV, dev.to_string())
            .env(PROBE_INO_ENV, ino.to_string())
            .status()
            .expect("spawn exec child probe");

        assert!(
            status.success(),
            "exec child observed inherited dup fd: {status}"
        );
    }

    #[tokio::test]
    async fn long_running_command_emits_balanced_started_then_ended_pair() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (gateway, notifications) = recording_gateway();
                let pty_id = create_test_pty(gateway).await;

                write_pty_input(&pty_id, b"sleep 2\n")
                    .await
                    .expect("write command");

                let after_start = wait_for_busy_events(
                    &notifications,
                    &pty_id,
                    &["process_started"],
                    Duration::from_secs(10),
                )
                .await;
                assert_eq!(after_start, vec!["process_started"]);

                let after_end = wait_for_busy_events(
                    &notifications,
                    &pty_id,
                    &["process_started", "process_ended"],
                    Duration::from_secs(10),
                )
                .await;
                assert_eq!(after_end, vec!["process_started", "process_ended"]);

                close_pty(&pty_id).await.expect("close pty");
            })
            .await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn close_pty_kills_a_background_grandchild() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (gateway, _) = recording_gateway();
                let pty_id = create_test_pty(gateway).await;

                write_pty_input(&pty_id, b"sleep 300 & echo pid=$!\n")
                    .await
                    .expect("write command");
                let grandchild = wait_for_reported_pid(&pty_id).await;

                // Without job control the job shares the shell's group and the
                // group kill alone would pass this test.
                let shell = require_pty(&pty_id)
                    .await
                    .expect("pty")
                    .lock()
                    .await
                    .shell
                    .pid()
                    .expect("shell pid") as i32;
                assert_ne!(
                    unsafe { libc::getpgid(grandchild) },
                    unsafe { libc::getpgid(shell) },
                    "background job did not get its own process group"
                );

                close_pty(&pty_id).await.expect("close pty");

                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while unsafe { libc::kill(grandchild, 0) } == 0 {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "grandchild {grandchild} survived the pty close"
                    );
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await;
    }

    /// A local scope, so the process-global one is not latched closed for the
    /// tests that follow.
    #[cfg(unix)]
    #[tokio::test]
    async fn scope_teardown_kills_a_background_grandchild() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (gateway, _) = recording_gateway();
                let pty_id = create_test_pty(gateway).await;

                write_pty_input(&pty_id, b"sleep 300 & echo pid=$!\n")
                    .await
                    .expect("write command");
                let grandchild = wait_for_reported_pid(&pty_id).await;

                let shell = require_pty(&pty_id)
                    .await
                    .expect("pty")
                    .lock()
                    .await
                    .shell
                    .pid()
                    .expect("shell pid");
                assert_ne!(
                    unsafe { libc::getpgid(grandchild) },
                    unsafe { libc::getpgid(shell as i32) },
                    "background job did not get its own process group"
                );

                let scope = xai_tty_utils::ProcessScope::new();
                let _group = scope.enroll_terminal_pid(shell).expect("enroll");
                scope.kill_all();

                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while unsafe { libc::kill(grandchild, 0) } == 0 {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "grandchild {grandchild} survived scope teardown"
                    );
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }

                close_pty(&pty_id).await.expect("close pty");
            })
            .await;
    }

    /// Covers the failure paths in [`create_pty`], which reap by returning.
    #[tokio::test]
    async fn dropping_an_unregistered_shell_reaps_it() {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.arg("-c");
        cmd.arg("sleep 300");
        #[allow(clippy::disallowed_methods)]
        let child = pair.slave.spawn_command(cmd).expect("spawn shell");
        let pid = child.process_id().expect("shell pid") as i32;

        drop(UnregisteredShell::new(child));

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while unsafe { libc::kill(pid, 0) } == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "shell {pid} survived the guard drop"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[cfg(unix)]
    async fn wait_for_reported_pid(pty_id: &str) -> i32 {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let entry = require_pty(pty_id).await.expect("pty");
            let out: Vec<u8> = entry.lock().await.output_ring.iter().copied().collect();
            let text = String::from_utf8_lossy(&out);
            // Every occurrence, since the shell echoes the command's own `pid=$!`.
            if let Some(pid) = text
                .split("pid=")
                .skip(1)
                .filter_map(|rest| rest.split_whitespace().next())
                .find_map(|digits| digits.trim().parse::<i32>().ok())
            {
                return pid;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "shell never reported a background pid: {text}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn idle_shell_emits_no_busy_notifications() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (gateway, notifications) = recording_gateway();
                let pty_id = create_test_pty(gateway).await;

                tokio::time::sleep(Duration::from_millis(BUSY_POLL_INTERVAL_MS * 3)).await;

                assert_eq!(
                    busy_event_types(&notifications, &pty_id),
                    Vec::<String>::new()
                );

                close_pty(&pty_id).await.expect("close pty");
            })
            .await;
    }
}
