//! Bounded stdio MCP auto-restart.
//!
//! When [`crate::session::mcp_dispatcher::run_dispatcher`] processes a
//! window containing a [`xai_grok_mcp::servers::McpClientEventKind::TransportClosed`]
//! or [`xai_grok_mcp::servers::McpClientEventKind::HandshakeFailed`] key for a
//! **stdio** MCP server, the dispatcher hands the key off to
//! [`maybe_schedule_restart`]. That function applies the guard rails listed
//! below and, if all pass, spawns a one-shot [`auto_restart_stdio`] task that
//! sleeps + respawns up to three times before parking the server as
//! `unavailable`.
//!
//! ## Backoff
//!
//! ### Intra-cycle (attempt ladder)
//!
//! Three attempts at exactly:
//!
//! ```text
//! attempt 1 → +1s  (t=1s)
//! attempt 2 → +4s  (t=5s)
//! attempt 3 → +16s (t=21s)
//! ```
//!
//! Encoded as [`BACKOFF`]. The full window before exhaustion is 21 s.
//!
//! ### Inter-cycle (early-death budget, issue #45)
//!
//! A handshake that succeeds but whose **transport closes** within
//! [`STABILITY_WINDOW`] counts as a failed cycle. Up to
//! [`MAX_EARLY_DEATHS`] such cycles each schedule a restart after
//! [`CYCLE_BACKOFF`]; the next early exit parks the server for the
//! session with a single `RestartFailed` diagnostic. Surviving past
//! the stability window resets the consecutive counter.
//!
//! Only [`xai_grok_mcp::servers::McpClientEventKind::TransportClosed`]
//! feeds this budget — see [`RestartBudget::classify_death`] for why
//! `HandshakeFailed` must not.
//!
//! A park is **not** session-permanent: a handshake observed on the
//! dispatcher's event stream (`Ready`) clears it, via
//! [`RestartBudget::note_recovery_ready`]. See that method for why such
//! a handshake can only be user-initiated recovery.
//!
//! ## Guard rails (skip conditions)
//!
//! These are the guard rails for where auto-restart must NOT fire.
//! Same ground truth at both check sites, BUT the **check
//! order differs by design** between the two sites — see the comparison
//! table below.
//!
//! 1. **Non-restart event kind** — `maybe_schedule_restart` short-circuits
//!    for anything other than `TransportClosed` / `HandshakeFailed`. The
//!    auto-restart loop does not see other kinds (it's never invoked for
//!    them), so this gate appears only at schedule time.
//! 2. **HTTP / HttpAuth** — auto-restart is **stdio-only**. HTTP/OAuth
//!    transports go through `reset_transport` on the next tool call,
//!    which is the existing and correct recovery path. The single
//!    [`RestartActions::is_stdio_server_configured`] question returns
//!    `false` for any non-stdio configured entry, so the gate doubles as
//!    the HTTP filter (no separate `is_http` check is needed).
//! 3. **`kill_on_drop` from config diff** —
//!    [`xai_grok_mcp::servers::start_mcp_server`] sets
//!    `kill_on_drop(true)` on the spawned `tokio::process::Command`
//!    in the `acp::McpServer::Stdio` arm. When
//!    `McpState::update_configs_diff` drops the `Arc<McpClient>` the
//!    child is SIGKILLed and the liveness watcher eventually emits
//!    `TransportClosed`. The dispatcher's
//!    [`crate::session::mcp_dispatcher::ShutdownState`] (set on
//!    `ConfigRemoved` events) is the explicit "this teardown was
//!    intentional" channel. We consult it via
//!    [`RestartActions::is_in_shutting_down`] at both check sites.
//! 4. **Disabled / not currently configured** — `update_configs_diff` or
//!    `ToggleMcpServer enabled=false` removes the stdio entry. We consult
//!    [`RestartActions::is_stdio_server_configured`] (which already
//!    folds the disabled-list check); on `false` mid-loop we emit one
//!    final [`crate::session::mcp_dispatcher::McpServerStatusReason::Disabled`]
//!    push and stop.
//! 5. **Already-Empty** — see the [`xai_grok_mcp::servers::ClientStateKind::Empty`]
//!    doc: a previous handshake exhausted attempts. Recovery from
//!    `Empty` is via the explicit `Refresh` button, not auto-restart.
//!    Enforced upstream: the liveness watcher emits `TransportClosed`
//!    only from `Ready` / `Initializing`, never from `Empty`.
//! 6. **Early-death budget parked** — the server crash-looped past
//!    [`MAX_EARLY_DEATHS`] (see [`RestartBudget`]). Checked last in
//!    [`maybe_schedule_restart`], *after* the in-flight dedup claim,
//!    because it is the only guard that mutates state: a call the
//!    dedup guard is going to reject must not first burn a cycle.
//!
//! ### Check-order difference
//!
//! | Site                       | First check                        | Then                              |
//! |----------------------------|------------------------------------|-----------------------------------|
//! | [`maybe_schedule_restart`] | `is_in_shutting_down` (cheap, sync)| `is_stdio_server_configured` (async, may hit disk) |
//! | [`auto_restart_stdio`] loop| `is_stdio_server_configured`       | `is_in_shutting_down`             |
//!
//! At schedule time we shed the cheap sync check first so we never pay
//! the async + disk hit for an event we'll skip anyway. Inside the loop
//! the priority inverts: the "user removed it" path needs an explicit
//! wire push (`Reason::Disabled`) before we exit, so we check it first;
//! `shutting_down` exit needs no push (the upstream `ConfigRemoved`
//! flush already emitted one).
//!
//! ## Telemetry
//!
//! Emitted via `tracing::info!` with the metric name in the `target:`
//! field (`metrics.mcp.auto_restart.<counter>`), one target per metric.
//!
//! | Metric                              | Labels                                                      |
//! |-------------------------------------|-------------------------------------------------------------|
//! | `mcp.auto_restart.attempted`        | `server`, `attempt`                                         |
//! | `mcp.auto_restart.succeeded`        | `server`, `attempt ∈ {1,2,3}`                               |
//! | `mcp.auto_restart.exhausted`        | `server`                                                    |
//! | `mcp.auto_restart.skipped`          | `server`, `reason ∈ {shutting_down, not_configured, disabled}` |
//!
//! `attempted` is counted once per actual `respawn_stdio` call (after
//! the in-loop guards pass and the backoff sleep elapses), not at task
//! entry — so it stays honest if the configured-set flips mid-sleep.

use std::rc::Rc;
use std::time::Duration;

use agent_client_protocol as acp;
use async_trait::async_trait;
use xai_grok_mcp::servers::{McpClientEventKind, McpServerName};

use crate::session::mcp_dispatcher::{
    McpServerStatus, McpServerStatusPayload, McpServerStatusReason, SERVER_STATUS_METHOD,
    classify_source,
};

/// Exponential backoff for the three respawn attempts.
///
/// Wall-clock targets: `t=1s, t=5s, t=21s` (cumulative). Total worst-case
/// window before the task gives up and parks the server is 21 s.
pub(crate) const BACKOFF: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(4),
    Duration::from_secs(16),
];

/// How long a post-handshake transport must stay open before the restart
/// is treated as healthy (resets the early-death cycle budget).
///
/// 30s is long enough that a server doing brief post-init work (tool
/// registration, a reconnect hop) is not misclassified, and short enough
/// that a crash-loop parks within a few minutes once cycle backoff is
/// applied. Classification keys only off transport close — a server that
/// stays up while "becoming useful" is never mis-parked, regardless of
/// how long that takes.
pub(crate) const STABILITY_WINDOW: Duration = Duration::from_secs(30);

/// Consecutive handshake-then-early-exit cycles before auto-restart parks
/// the server (until a dispatcher-observed `Ready` recovers it — see
/// [`RestartBudget::note_recovery_ready`]).
///
/// Cap shape: **cycle count**, not a wall-clock window. Acceptance wants
/// "stays up past the window → budget resets"; a consecutive counter with
/// a stability reset is the dual of that rule and stays deterministic
/// under `tokio::time::pause`. A rolling wall-clock cap would still allow
/// infinite restarts spaced just over the window.
pub(crate) const MAX_EARLY_DEATHS: usize = 5;

/// Backoff *between* early-death restart cycles (before the intra-cycle
/// [`BACKOFF`] ladder). Indexed by `early_deaths - 1` for deaths 1..=5
/// (the 6th early death parks). The last step is a 60s cap.
///
/// Strictly non-decreasing so steady-state log/spawn volume stays bounded
/// without multi-hour waits.
pub(crate) const CYCLE_BACKOFF: [Duration; 5] = [
    Duration::from_secs(2),
    Duration::from_secs(8),
    Duration::from_secs(32),
    Duration::from_secs(60),
    Duration::from_secs(60),
];

/// Outcome of classifying a transport death against the early-death budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeathDisposition {
    /// Schedule a restart cycle; sleep `cycle_backoff` before the attempt ladder.
    Proceed { cycle_backoff: Duration },
    /// This death exhausted the budget — emit the one park diagnostic, do not restart.
    Exhausted,
    /// Parked by an earlier [`Self::Exhausted`] and not yet recovered
    /// (see [`RestartBudget::note_recovery_ready`]) — do not restart and
    /// do not emit a second diagnostic. The caller still unregisters the
    /// server's tools: the client was just evicted again.
    AlreadyParked,
}

/// Per-server early-death budget (issue #45). Lives in [`ShutdownState`].
#[derive(Debug, Default)]
pub(crate) struct RestartBudget {
    /// Consecutive early-death cycles observed since the last healthy stretch.
    early_deaths: usize,
    /// When this server was last observed `Ready`. Monotonic: written
    /// by [`Self::note_ready`] / [`Self::note_recovery_ready`], and
    /// **never cleared**.
    ///
    /// Clearing it is a tempting tidy-up and re-creates issue #45. A
    /// restart is scheduled on *every* early death, so clearing this
    /// "when a restart is scheduled" makes the next
    /// [`Self::classify_death`] find `None`, take the not-early branch,
    /// and run `early_deaths = 0` — zeroing the counter once per cycle,
    /// forever. Clearing it inside `classify_death` is worse still: the
    /// second death of a double-burn window would reset the counter
    /// instead of incrementing it, turning a mild over-count into a
    /// budget escape. The invariant is "when last seen `Ready`", not
    /// "since the last restart".
    last_ready_at: Option<tokio::time::Instant>,
    /// Park after more than [`MAX_EARLY_DEATHS`] early exits. Cleared by
    /// [`Self::note_recovery_ready`].
    parked: bool,
}

impl RestartBudget {
    /// Record a successful handshake performed by [`auto_restart_stdio`];
    /// starts/refreshes the stability window.
    ///
    /// Deliberately does NOT touch `early_deaths` or `parked` — a restart
    /// this module performed itself is the thing being budgeted, so
    /// letting it clear the counter would re-create the issue #45 bug
    /// (every successful handshake resetting the budget). Recovery
    /// driven from outside goes through [`Self::note_recovery_ready`].
    pub(crate) fn note_ready(&mut self, now: tokio::time::Instant) {
        self.last_ready_at = Some(now);
    }

    /// Record a handshake observed on the **dispatcher's** client-event
    /// stream (`McpClientEventKind::Ready`) and clear the park.
    ///
    /// Why this is safe to treat as user-initiated recovery: the
    /// auto-restart success path never produces a dispatcher `Ready`.
    /// `respawn_stdio` wires `set_event_tx` AFTER `ensure_initialized`
    /// (see `AcpSessionImpl::respawn_stdio`), so the handshake it
    /// performs is invisible to the dispatcher — it reports through
    /// [`Self::note_ready`] instead.
    ///
    /// **That ordering is necessary but not sufficient, and the
    /// sufficient fact lives in another crate.** Once `set_event_tx` has
    /// run, the restarted client *does* have a sender wired: if it could
    /// ever re-handshake in place it would emit a dispatcher `Ready` and
    /// lift its own park. What makes that impossible for stdio is
    /// `restorable_transport` mapping `PendingTransport::Stdio(_)` to
    /// `None` (`xai-grok-mcp/src/servers.rs`) — `reset_transport` is a
    /// no-op, so a dead stdio client cannot be re-initialised in place.
    /// It can only be replaced by a fresh `start_mcp_server`.
    ///
    /// **If stdio is ever made restorable** — the shape of that is a
    /// "reconnect without respawning the child" optimisation — **this
    /// breaks**, and an auto-restart would silently unpark the server it
    /// had just parked. The `if self.parked` gate below bounds the
    /// damage to unparking rather than refunding the counter, but the
    /// dependency is real and it is not visible from this file.
    ///
    /// So the only producers of a dispatcher `Ready` are a fresh
    /// `start_mcp_server`: session startup, `ConfigAdded` (re-add),
    /// `ToggleMcpServer` on, or an explicit Refresh. After a park, all
    /// of those are the user fixing the server.
    ///
    /// The counter reset is deliberately gated on `parked`, and that
    /// gate is load-bearing. Undoing a park is this method's whole job;
    /// resetting an *un-parked* server's counter would open a refund
    /// path that re-creates issue #45. `flush_window` runs before
    /// `maybe_schedule_restart` (see `run_dispatcher`), so a `Ready`
    /// and a `TransportClosed` coalescing into the same 50 ms window
    /// would zero the counter and then immediately re-increment it from
    /// zero — once per cycle, forever. Making that safe would require
    /// proving no path anywhere can emit a stray `Ready` for a live
    /// crash-looping client; gating on `parked` makes the proof
    /// unnecessary.
    ///
    /// Nothing is lost by the gate: "this server is healthy again" for
    /// an un-parked server is already handled by
    /// [`Self::classify_death`]'s not-early branch, which resets the
    /// counter after a stretch longer than [`STABILITY_WINDOW`]. And a
    /// park always yields a full budget again, because the reset does
    /// run in the case the gate admits.
    pub(crate) fn note_recovery_ready(&mut self, now: tokio::time::Instant) {
        self.last_ready_at = Some(now);
        // Not removable: see this method's doc. Deleting the gate is
        // caught by `dispatcher_ready_while_unparked_does_not_refund_the_budget`.
        if self.parked {
            self.parked = false;
            self.early_deaths = 0;
        }
    }

    /// Classify a death event and update counters.
    ///
    /// Up to [`MAX_EARLY_DEATHS`] early exits each schedule a restart (with
    /// increasing cycle backoff). The next early exit parks.
    ///
    /// **Only [`McpClientEventKind::TransportClosed`] feeds the
    /// early-death counter.** `HandshakeFailed` is emitted for *any*
    /// `ensure_initialized` error — including a `startup_timeout_sec`
    /// timeout on a server that is still alive and whose transport never
    /// closed. Letting it burn a cycle would park a healthy-but-slow
    /// server after a handful of startup hiccups, under a message
    /// ("early exit within 30s of handshake") describing something that
    /// did not happen. It also has no need for a budget of its own: a
    /// handshake that never succeeds never reaches the `Ok` arm of
    /// [`auto_restart_stdio`], so the intra-cycle [`BACKOFF`] ladder is
    /// never reset and exhaustion after 3 attempts is already reachable
    /// — which is the pre-#45 behaviour and is correct.
    ///
    /// The `parked` check runs *after* the kind check, so a park never
    /// suppresses a `HandshakeFailed`. For stdio a parked server's old
    /// client cannot re-handshake in place — `restorable_transport` maps
    /// `PendingTransport::Stdio(_)` to `None`
    /// (`xai-grok-mcp/src/servers.rs`) so `reset_transport` is a no-op,
    /// `Ready(dead)` returns `Ok` without handshaking, and `Empty`
    /// returns `Err` before the emit block. The only producer of a
    /// `HandshakeFailed` for a parked stdio server is therefore a fresh
    /// `start_mcp_server`: a client the user just asked for.
    ///
    /// Suppressing that would defeat the recovery path on its first
    /// stumble. The user fixes the command and toggles the server back
    /// on; the new client's first handshake overruns
    /// `startup_timeout_sec` — the very transient this kind exclusion
    /// exists to tolerate — and with the checks the other way round it
    /// would get zero restart attempts instead of the three-attempt
    /// ladder that would most likely recover it, with nothing telling
    /// the user why. Nothing leaks in return: a restart that succeeds
    /// reports through [`Self::note_ready`], not
    /// [`Self::note_recovery_ready`], so `parked` stays set and the
    /// transport-death park is untouched.
    pub(crate) fn classify_death(
        &mut self,
        now: tokio::time::Instant,
        kind: McpClientEventKind,
    ) -> DeathDisposition {
        // Kind first, and the order matters: a non-transport death is
        // outside this budget entirely, so it is not subject to the park
        // either. See the doc above for why suppressing a
        // `HandshakeFailed` here would defeat the recovery path.
        if !matches!(kind, McpClientEventKind::TransportClosed) {
            // Proceed on the legacy path without touching `early_deaths`
            // or `last_ready_at`. Resetting either here would let an
            // interleaved `HandshakeFailed` silently refund a
            // crash-looping server's budget.
            return DeathDisposition::Proceed {
                cycle_backoff: Duration::ZERO,
            };
        }
        if self.parked {
            return DeathDisposition::AlreadyParked;
        }
        let early = self
            .last_ready_at
            .is_some_and(|t| now.saturating_duration_since(t) < STABILITY_WINDOW);
        if early {
            self.early_deaths = self.early_deaths.saturating_add(1);
            if self.early_deaths > MAX_EARLY_DEATHS {
                self.parked = true;
                return DeathDisposition::Exhausted;
            }
            let idx = self.early_deaths.saturating_sub(1);
            let cycle_backoff = CYCLE_BACKOFF
                .get(idx)
                .copied()
                .unwrap_or(*CYCLE_BACKOFF.last().expect("CYCLE_BACKOFF non-empty"));
            DeathDisposition::Proceed { cycle_backoff }
        } else {
            // Survived past the window: correct by construction. A
            // `TransportClosed` can only come from a liveness watcher,
            // which `arm_liveness_watcher` refuses to spawn unless the
            // client is already `Ready` (`xai-grok-mcp/src/servers.rs`)
            // and which withdraws silently from every non-`Ready` state
            // (`xai-grok-mcp/src/liveness.rs`). So this client was
            // `Ready` at `last_ready_at` and stayed `Ready` until it
            // closed — a healthy stretch longer than STABILITY_WINDOW,
            // and the consecutive counter should reset.
            //
            // `last_ready_at == None` also lands here, as a defensive
            // default. It is not a reachable case for the only kind
            // that gets this far: reaching `Ready` is exactly what
            // records the timestamp, on both the startup path
            // (dispatcher `Ready` → `note_recovery_ready`) and the
            // restart path (`respawn_stdio` success path → `note_ready`).
            //
            // The boundary that keeps the restart path honest is
            // **watcher-arm → `note_ready`**, NOT respawn-return →
            // `note_ready`. `interval` ticks immediately, so the watcher
            // can emit a close the instant it is armed; any `.await`
            // between arming and recording lets the dispatcher classify
            // the NEW client's death against the PREVIOUS client's
            // timestamp, and once that is stale this branch refunds the
            // budget. `respawn_stdio` therefore arms AFTER its
            // `owned_clients.insert` and returns straight into
            // `note_ready` with nothing awaiting in between — see the
            // comment there before reordering it.
            self.early_deaths = 0;
            DeathDisposition::Proceed {
                cycle_backoff: Duration::ZERO,
            }
        }
    }
}

/// Backoff between HTTP recovery attempts (first attempt is immediate).
/// Longer than the stdio [`BACKOFF`] because an HTTP MCP server (e.g.
/// `http-mcp-server`) usually drops on a rolling redeploy that takes minutes to bring
/// a healthy replica back; retrying across ~2.5 min lets it self-heal
/// instead of parking until the next tool call. 8 attempts total.
pub(crate) const HTTP_RECOVERY_BACKOFF: [Duration; 7] = [
    Duration::from_secs(1),
    Duration::from_secs(4),
    Duration::from_secs(16),
    Duration::from_secs(30),
    Duration::from_secs(30),
    Duration::from_secs(30),
    Duration::from_secs(30),
];

/// Skip-reason label values surfaced on `mcp.auto_restart.skipped`.
///
/// `Disabled` vs `NotConfigured` both come from
/// [`RestartActions::is_stdio_server_configured`] returning `false`;
/// the split is temporal (schedule time vs inside the backoff loop) so
/// operators can tell "flipped off mid-restart" from "stale event".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SkipReason {
    /// Server is in the dispatcher's `shutting_down` set
    /// ([`crate::session::mcp_dispatcher::ShutdownState`]).
    ShuttingDown,
    /// `is_stdio_server_configured` returned `false` at schedule
    /// time.
    NotConfigured,
    /// `is_stdio_server_configured` returned `false` inside the
    /// backoff loop.
    Disabled,
    /// A restart task for this server is already in flight
    /// ([`RestartActions::begin_restart`] returned `false`). A second
    /// `TransportClosed` / `HandshakeFailed` for the same server while
    /// the first respawn is still sleeping or mid-handshake is
    /// short-circuited here so we never spawn a duplicate task.
    InProgress,
    /// Early-death budget exhausted earlier; further deaths stay
    /// parked until a dispatcher-observed handshake (`Ready` — from
    /// Refresh, a config re-add, or toggle-on) clears the park via
    /// [`RestartBudget::note_recovery_ready`].
    Parked,
}

impl SkipReason {
    fn as_label(self) -> &'static str {
        match self {
            Self::ShuttingDown => "shutting_down",
            Self::NotConfigured => "not_configured",
            Self::Disabled => "disabled",
            Self::InProgress => "in_progress",
            Self::Parked => "parked",
        }
    }
}

/// Side effects that the auto-restart task needs. Abstracted as a trait so
/// unit tests can plug in a mock — the production binding lives next to
/// the dispatcher wiring in `acp_session.rs::SessionRestartActions`.
///
/// ## Threading contract
///
/// `?Send` matches the session actor's LocalSet: the production impl
/// holds `Arc<SessionActor>` (!Send) and the dispatcher's
/// `AcpAgentGatewaySender` (!Send via `acp::AgentSideConnection`).
/// Both [`maybe_schedule_restart`] and [`auto_restart_stdio`] call
/// `tokio::task::spawn_local` directly, which **panics** at runtime
/// if invoked outside a `LocalSet`. Callers MUST drive these
/// functions from a future running inside a `LocalSet` (the
/// session-actor pattern); any future `RestartActions` impl that
/// claims `Send + Sync` does NOT relax this requirement.
#[async_trait(?Send)]
pub(crate) trait RestartActions {
    /// Returns `true` iff the server still has a stdio entry in
    /// `McpState::configs` AND is enabled (not on the disabled list).
    /// Used both at schedule time and at the top of each backoff loop.
    async fn is_stdio_server_configured(&self, server: &str) -> bool;

    /// Returns `true` iff the server name is in the dispatcher's
    /// `shutting_down` set. The set is populated by `flush_window`
    /// when it observes an `McpClientEventKind::ConfigRemoved` event
    /// (see `mcp_dispatcher.rs`).
    fn is_in_shutting_down(&self, server: &str) -> bool;

    /// Re-run `start_mcp_server` for `server` against its current
    /// `McpState::configs` entry, drive the handshake to completion, arm
    /// the liveness watcher, atomically swap the new `Arc<McpClient>` into
    /// `McpState::owned_clients`, and record `note_ready` before returning
    /// `Ok(())`.
    ///
    /// **Stdio-only.** Callers gate on
    /// [`Self::is_stdio_server_configured`]; HTTP / HttpAuth never
    /// reach this method. Failure modes (returned as a sanitized
    /// `Err`) are:
    /// 1. No matching stdio config entry — racy concurrent removal.
    /// 2. `start_mcp_server` failed — spawn / OAuth-discovery /
    ///    transport-build error.
    /// 3. `ensure_initialized` failed — handshake error.
    /// 4. Post-handshake re-check of the configured
    ///    set found the server disabled/removed during the (multi-
    ///    second) handshake window; the new `Arc<McpClient>` is
    ///    dropped on the floor, `kill_on_drop` SIGKILLs the spawned
    ///    child, and an explicit "raced with config change" error
    ///    bubbles up.
    async fn respawn_stdio(&self, server: &str) -> Result<(), String>;

    /// Push an already-built `x.ai/mcp/server_status` payload to the
    /// pager. The production impl wraps the dispatcher's gateway
    /// sender via [`forward_status`].
    fn push_status(&self, payload: &McpServerStatusPayload);

    /// Atomically claim the single in-flight restart slot for
    /// `server`. Returns `true` if the claim succeeded (no other
    /// restart task is running for this server) and `false` if a
    /// restart task is already in flight.
    ///
    /// Paired with [`Self::end_restart`] (released via an RAII guard on
    /// every exit path). Default impl is a no-op claim so mocks keep
    /// compiling; production backs it with a `HashSet` beside
    /// `ShutdownState`.
    fn begin_restart(&self, _server: &str) -> bool {
        true
    }

    /// Release the in-flight restart claim taken by
    /// [`Self::begin_restart`]. Default impl is a no-op (pairs with the
    /// default `begin_restart`).
    fn end_restart(&self, _server: &str) {}

    /// Returns `true` iff the server still has an **HTTP / SSE** entry in
    /// `McpState::configs` AND is enabled (not on the disabled list).
    ///
    /// HTTP analog of [`Self::is_stdio_server_configured`]; gates
    /// [`maybe_schedule_http_recovery`]. Default `false` for mocks.
    async fn is_http_server_configured(&self, _server: &str) -> bool {
        false
    }

    /// Recover a dead HTTP client in place: reset transport, re-handshake,
    /// re-arm liveness. The `Arc<McpClient>` stays in `owned_clients` (tools
    /// stay valid). Status is emitted by `ensure_initialized`, not here.
    /// Default `Err` for mocks.
    async fn reset_http_client(&self, _server: &str) -> Result<(), String> {
        Err("reset_http_client not implemented".to_string())
    }

    /// Drop `server`'s tools from the bridge after stdio restart exhaustion,
    /// so the model stops calling a `not found` server. Default no-op for mocks.
    fn unregister_server_tools(&self, _server: &str) {}

    /// Record a successful handshake for `server` (starts/refreshes the
    /// [`STABILITY_WINDOW`] used to classify the next transport death).
    /// Default no-op for mocks that do not track early-death budgets.
    ///
    /// This is the [`auto_restart_stdio`] success path only; the
    /// dispatcher's own `Ready` observation goes straight to
    /// [`RestartBudget::note_recovery_ready`] (it holds the
    /// `ShutdownState` lock already and must be able to *clear* a park,
    /// which this method deliberately cannot).
    fn note_ready(&self, _server: &str) {}

    /// Classify a death event against the early-death budget.
    /// Default: always proceed with no cycle backoff (legacy behaviour).
    fn classify_death(&self, _server: &str, _kind: McpClientEventKind) -> DeathDisposition {
        DeathDisposition::Proceed {
            cycle_backoff: Duration::ZERO,
        }
    }
}

/// Decide whether to schedule an [`auto_restart_stdio`] task for the
/// given event, applying the guard rails (see the module doc and the
/// inline `Guard N` comments below). Returns `true` iff a task was
/// spawned; `false` for any guard-rail rejection or non-restart kind.
///
/// Calls `tokio::task::spawn_local`, so it MUST run inside a `LocalSet`
/// — in production the dispatcher's `run_dispatcher` task is.
pub(crate) async fn maybe_schedule_restart(
    actions: Rc<dyn RestartActions>,
    session_id: String,
    server: McpServerName,
    kind: McpClientEventKind,
    cancel: tokio_util::sync::CancellationToken,
) -> bool {
    // Guard 1: only transport-dead events trigger a restart.
    if !matches!(
        kind,
        McpClientEventKind::TransportClosed | McpClientEventKind::HandshakeFailed
    ) {
        return false;
    }

    // Guard 2: kill_on_drop grace window from a config diff / toggle
    // (cheap sync check before the async configured-set probe).
    if actions.is_in_shutting_down(&server) {
        record_skipped(&server, SkipReason::ShuttingDown);
        return false;
    }

    // Guard 3: must be currently configured as stdio. HTTP/HttpAuth
    // are out of scope (their `is_stdio_server_configured` impl
    // returns false for non-stdio entries). A server removed from
    // `configs` between the event firing and us checking also lands
    // here.
    if !actions.is_stdio_server_configured(&server).await {
        record_skipped(&server, SkipReason::NotConfigured);
        return false;
    }

    // Guard 4: dedup against an already-in-flight restart. A second
    // event in a later coalesce window must NOT spawn a duplicate —
    // two tasks would each `start_mcp_server` and race on
    // `owned_clients.insert`, orphaning a stdio child. The claim is
    // atomic: no `.await` between here and the `spawn_local` below.
    // Released by the RAII guard on every exit path.
    //
    // This runs BEFORE the early-death classifier below because
    // `classify_death` MUTATES the budget: a single coalesce window can
    // carry both a `TransportClosed` (liveness watcher) and a
    // `HandshakeFailed` (a concurrent tool call's re-init) for the same
    // server, and `run_dispatcher` calls us once per `(server, kind)`
    // key. Classifying first would let the second call burn a budget
    // slot and then be dropped here, halving the effective budget for
    // any server whose crashes reliably produce both events.
    if !actions.begin_restart(&server) {
        record_skipped(&server, SkipReason::InProgress);
        return false;
    }
    // RAII from here on: every `return false` below drops this and
    // releases the claim taken above; the spawn path moves it into the
    // task instead, so the claim outlives this function exactly when a
    // restart is actually in flight.
    let in_flight = RestartInFlightGuard {
        actions: Rc::clone(&actions),
        server: server.clone(),
    };

    // Guard 5: early-death budget (issue #45). Handshake-OK then
    // transport-close within STABILITY_WINDOW counts as a failed
    // cycle; after MAX_EARLY_DEATHS such cycles we park the server
    // with a single RestartFailed diagnostic until a dispatcher
    // `Ready` (Refresh / re-add / toggle-on) clears the park.
    let cycle_backoff = match actions.classify_death(&server, kind) {
        DeathDisposition::AlreadyParked => {
            record_skipped(&server, SkipReason::Parked);
            // No second diagnostic (the park already emitted one), but
            // the tools MUST go. Only a `TransportClosed` can reach
            // here — `classify_death` checks `kind` before `parked` and
            // returns `Proceed` for everything else — and for that kind
            // `drop_dead_clients` has just evicted the client, so
            // leaving its tools registered lets the model call a server
            // that no longer exists. Idempotent.
            //
            // That reason is specific to this kind, not a general one:
            // `drop_dead_clients` never sees `HandshakeFailed`. If the
            // check order above is ever reversed, this comment stops
            // being true — which is the point of stating the dependency
            // rather than just the conclusion.
            actions.unregister_server_tools(&server);
            drop(in_flight);
            return false;
        }
        DeathDisposition::Exhausted => {
            record_exhausted(&server);
            push(
                &*actions,
                &session_id,
                &server,
                McpServerStatus::Unavailable,
                McpServerStatusReason::RestartFailed,
                Some(format!(
                    "stopped auto-restart: early exit within {}s of handshake {} times",
                    STABILITY_WINDOW.as_secs(),
                    MAX_EARLY_DEATHS,
                )),
            );
            actions.unregister_server_tools(&server);
            drop(in_flight);
            return false;
        }
        DeathDisposition::Proceed { cycle_backoff } => cycle_backoff,
    };

    let task_actions = Rc::clone(&actions);
    tokio::task::spawn_local(async move {
        // Release the in-flight claim when the task exits for any reason.
        let _in_flight = in_flight;
        auto_restart_stdio(task_actions, session_id, server, cancel, cycle_backoff).await;
    });
    true
}

/// RAII guard that releases the in-flight restart claim taken by
/// [`maybe_schedule_restart`] via [`RestartActions::begin_restart`].
/// Dropped when the spawned [`auto_restart_stdio`] task exits — on
/// success, exhaustion, a guard-rail skip, cancellation, or a panic —
/// so a future `TransportClosed` for the same server can schedule a
/// fresh restart.
struct RestartInFlightGuard {
    actions: Rc<dyn RestartActions>,
    server: McpServerName,
}

impl Drop for RestartInFlightGuard {
    fn drop(&mut self) {
        self.actions.end_restart(&self.server);
    }
}

/// One-shot task: optional inter-cycle backoff, then sleep / re-check
/// guard rails / respawn (≤3 attempts), emitting the
/// `mcp.auto_restart.*` metrics. Must run inside a `LocalSet` (the
/// production `RestartActions` holds `!Send` types).
///
/// `cycle_backoff` is the issue #45 delay *between* early-death
/// restart cycles; the intra-cycle [`BACKOFF`] ladder still applies
/// after it.
///
/// Each iteration re-checks the guards in the inverse order of
/// [`maybe_schedule_restart`] (see the module doc § "Check-order
/// difference"): `is_stdio_server_configured` first — a mid-backoff
/// removal emits a final `Reason::Disabled` push — then
/// `is_in_shutting_down` (no push; the `ConfigRemoved` flush already
/// emitted one).
///
/// On `Ok` it emits `Reason::RestartSucceeded`, records `note_ready`
/// (starts the stability window), and returns; this is the SOLE
/// success emitter, since `respawn_stdio` wires `set_event_tx` AFTER
/// `ensure_initialized` so the dispatcher's `Ready → Initialized`
/// mapping does not fire. On `Err` it emits `Reason::RestartFailed`
/// and continues; after three failures the server is parked (recovery
/// is via explicit Refresh).
pub(crate) async fn auto_restart_stdio(
    actions: Rc<dyn RestartActions>,
    session_id: String,
    server: McpServerName,
    cancel: tokio_util::sync::CancellationToken,
    cycle_backoff: Duration,
) {
    if !cycle_backoff.is_zero() {
        tokio::select! {
            _ = tokio::time::sleep(cycle_backoff) => {}
            _ = cancel.cancelled() => {
                tracing::debug!(
                    server = %server,
                    "auto-restart cancelled during cycle backoff (session shutdown)",
                );
                return;
            }
        }
        if cancel.is_cancelled() {
            return;
        }
    }

    for (idx, wait) in BACKOFF.iter().enumerate() {
        let attempt = idx + 1;

        // On graceful shutdown the dispatcher cancels this token;
        // select on it so the backoff sleep aborts promptly instead of
        // delaying shutdown or pushing through a tearing-down gateway.
        tokio::select! {
            _ = tokio::time::sleep(*wait) => {}
            _ = cancel.cancelled() => {
                tracing::debug!(
                    server = %server,
                    attempt,
                    "auto-restart cancelled during backoff (session shutdown)",
                );
                return;
            }
        }

        // Also short-circuit before the (multi-second) respawn call if
        // cancellation landed between the sleep completing and now.
        if cancel.is_cancelled() {
            tracing::debug!(
                server = %server,
                attempt,
                "auto-restart cancelled before respawn (session shutdown)",
            );
            return;
        }

        // HTTP/HttpAuth are filtered at schedule time, so the
        // `Reason::Disabled` push below only fires for user-driven
        // removal (toggle-off / config diff).
        if !actions.is_stdio_server_configured(&server).await {
            tracing::info!(
                server = %server,
                attempt,
                "auto-restart aborted: server no longer configured",
            );
            record_skipped(&server, SkipReason::Disabled);
            push(
                &*actions,
                &session_id,
                &server,
                McpServerStatus::Unavailable,
                McpServerStatusReason::Disabled,
                None,
            );
            return;
        }
        if actions.is_in_shutting_down(&server) {
            tracing::info!(
                server = %server,
                attempt,
                "auto-restart aborted: server in shutting_down set",
            );
            record_skipped(&server, SkipReason::ShuttingDown);
            return;
        }

        record_attempted(&server, attempt);

        match actions.respawn_stdio(&server).await {
            Ok(()) => {
                tracing::info!(
                    server = %server,
                    attempt,
                    "auto-restart succeeded",
                );
                record_succeeded(&server, attempt);
                push(
                    &*actions,
                    &session_id,
                    &server,
                    McpServerStatus::Ready,
                    McpServerStatusReason::RestartSucceeded,
                    None,
                );
                return;
            }
            Err(reason) => {
                tracing::warn!(
                    server = %server,
                    attempt,
                    %reason,
                    "auto-restart attempt failed",
                );
                push(
                    &*actions,
                    &session_id,
                    &server,
                    McpServerStatus::Unavailable,
                    McpServerStatusReason::RestartFailed,
                    Some(format!(
                        "attempt {} of {}: {}",
                        attempt,
                        BACKOFF.len(),
                        reason
                    )),
                );
            }
        }
    }

    // All three attempts failed — park the server.
    record_exhausted(&server);
    push(
        &*actions,
        &session_id,
        &server,
        McpServerStatus::Unavailable,
        McpServerStatusReason::RestartFailed,
        Some(format!("exhausted after {} attempts", BACKOFF.len())),
    );
    // The evicted client was never replaced; its tools are still registered.
    // Drop them so the model stops calling a `not found` server.
    actions.unregister_server_tools(&server);
}

/// HTTP counterpart to [`maybe_schedule_restart`]: retries
/// `reset_http_client` on the [`HTTP_RECOVERY_BACKOFF`] ladder so a dropped
/// HTTP client self-heals. Pushes no status (`ensure_initialized` owns it).
/// Same guard rails as [`maybe_schedule_restart`] (shutting-down /
/// configured / in-flight dedup). Returns `true` iff a task was spawned;
/// must run inside a `LocalSet`.
pub(crate) async fn maybe_schedule_http_recovery(
    actions: Rc<dyn RestartActions>,
    server: McpServerName,
    cancel: tokio_util::sync::CancellationToken,
) -> bool {
    // Guard: intentional teardown (config diff / toggle-off).
    if actions.is_in_shutting_down(&server) {
        record_http_recovery_skipped(&server, SkipReason::ShuttingDown);
        return false;
    }

    // Guard: must still be an enabled HTTP/SSE entry.
    if !actions.is_http_server_configured(&server).await {
        record_http_recovery_skipped(&server, SkipReason::NotConfigured);
        return false;
    }

    // Guard: dedup. Shares the `in_flight_restart` slot with stdio respawn.
    // Atomic: no `.await` between the claim and `spawn_local`.
    if !actions.begin_restart(&server) {
        record_http_recovery_skipped(&server, SkipReason::InProgress);
        return false;
    }

    let task_actions = Rc::clone(&actions);
    tokio::task::spawn_local(async move {
        // RAII: release the in-flight claim on every exit path.
        let _in_flight = RestartInFlightGuard {
            actions: Rc::clone(&task_actions),
            server: server.clone(),
        };
        http_recovery_loop(task_actions, server, cancel).await;
    });
    true
}

/// Retry loop backing [`maybe_schedule_http_recovery`]: immediate attempt,
/// then back off on [`HTTP_RECOVERY_BACKOFF`], re-checking the guards each
/// time. Returns on success, a tripped guard, or cancellation; parks the
/// server (metric only) once exhausted. Emits no status pushes —
/// `ensure_initialized` owns the server's status. Must run in a `LocalSet`.
async fn http_recovery_loop(
    actions: Rc<dyn RestartActions>,
    server: McpServerName,
    cancel: tokio_util::sync::CancellationToken,
) {
    // `wait_before`: delay before each attempt — `None` for the immediate
    // first, then each `HTTP_RECOVERY_BACKOFF` step.
    let waits = std::iter::once(None).chain(HTTP_RECOVERY_BACKOFF.iter().map(Some));
    let total = HTTP_RECOVERY_BACKOFF.len() + 1;

    for (idx, wait_before) in waits.enumerate() {
        let attempt = idx + 1;

        if let Some(wait) = wait_before {
            // Abort the sleep promptly on shutdown instead of holding the claim.
            tokio::select! {
                _ = tokio::time::sleep(*wait) => {}
                _ = cancel.cancelled() => return,
            }
        }
        if cancel.is_cancelled() {
            return;
        }

        // Re-check guards each attempt: a config toggle-off / shutdown can
        // land between attempts (same LocalSet).
        if actions.is_in_shutting_down(&server) {
            record_http_recovery_skipped(&server, SkipReason::ShuttingDown);
            return;
        }
        if !actions.is_http_server_configured(&server).await {
            record_http_recovery_skipped(&server, SkipReason::Disabled);
            return;
        }

        record_http_recovery_attempted(&server);
        match actions.reset_http_client(&server).await {
            Ok(()) => {
                tracing::info!(
                    server = %server,
                    attempt,
                    "in-place HTTP transport recovery succeeded",
                );
                record_http_recovery_succeeded(&server);
                return;
            }
            Err(reason) => {
                // Keep retrying; the `Pending` client keeps lazy recovery alive.
                tracing::warn!(
                    server = %server,
                    attempt,
                    %reason,
                    "in-place HTTP transport recovery attempt failed",
                );
            }
        }
    }

    // Ladder exhausted — park the server; a later tool call still triggers
    // lazy recovery via `ensure_initialized`.
    record_http_recovery_exhausted(&server);
    tracing::warn!(
        server = %server,
        attempts = total,
        "in-place HTTP transport recovery exhausted; server parked until next tool call",
    );
}

/// Build a wire payload and hand it to the actions' `push_status` hook.
fn push(
    actions: &dyn RestartActions,
    session_id: &str,
    server: &str,
    status: McpServerStatus,
    reason: McpServerStatusReason,
    detail: Option<String>,
) {
    let payload = McpServerStatusPayload {
        session_id: session_id.to_string(),
        name: server.to_string(),
        source: classify_source(server),
        status,
        reason,
        detail,
        tools: None,
    };
    actions.push_status(&payload);
}

/// Serialize a [`McpServerStatusPayload`] and send it to the gateway as an
/// ACP `x.ai/mcp/server_status` notification. Failures are logged and
/// dropped — restart-task pushes must not block the session actor.
///
/// Public so production impls and tests can wrap a gateway sender
/// without reaching into private dispatcher internals. Uses
/// [`crate::session::mcp_dispatcher::SERVER_STATUS_METHOD`] so pushes
/// share the dispatcher's wire method name.
pub(crate) fn forward_status(
    gateway: &xai_acp_lib::AcpAgentGatewaySender,
    payload: &McpServerStatusPayload,
) {
    let raw = match serde_json::value::to_raw_value(payload) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                server = %payload.name,
                error = %e,
                "auto-restart: failed to serialize mcp/server_status payload",
            );
            return;
        }
    };
    gateway.forward_fire_and_forget(acp::ExtNotification::new(SERVER_STATUS_METHOD, raw.into()));
}

// ── telemetry helpers (tracing-as-metrics; see module doc § Telemetry) ──

fn record_attempted(server: &str, attempt: usize) {
    tracing::info!(
        target: "metrics.mcp.auto_restart.attempted",
        server = %server,
        attempt,
    );
}
fn record_succeeded(server: &str, attempt: usize) {
    tracing::info!(target: "metrics.mcp.auto_restart.succeeded", server = %server, attempt);
}
fn record_exhausted(server: &str) {
    tracing::info!(target: "metrics.mcp.auto_restart.exhausted", server = %server);
}
fn record_skipped(server: &str, reason: SkipReason) {
    tracing::info!(
        target: "metrics.mcp.auto_restart.skipped",
        server = %server,
        reason = reason.as_label(),
    );
}

// ── in-place HTTP recovery metrics (kept separate from auto_restart.* so
//    operators can distinguish stdio respawn from HTTP transport reset) ──

fn record_http_recovery_attempted(server: &str) {
    tracing::info!(target: "metrics.mcp.http_recovery.attempted", server = %server);
}
fn record_http_recovery_succeeded(server: &str) {
    tracing::info!(target: "metrics.mcp.http_recovery.succeeded", server = %server);
}
fn record_http_recovery_exhausted(server: &str) {
    tracing::info!(target: "metrics.mcp.http_recovery.exhausted", server = %server);
}
fn record_http_recovery_skipped(server: &str, reason: SkipReason) {
    tracing::info!(
        target: "metrics.mcp.http_recovery.skipped",
        server = %server,
        reason = reason.as_label(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashSet;
    use std::time::Duration as StdDuration;

    /// Records `RestartActions` calls for assertion. All fields are
    /// `RefCell`-wrapped because the production trait takes `&self`
    /// and the auto-restart task threads a single `Rc<dyn ...>`
    /// through the loop. The production trait is `Rc<dyn RestartActions>`,
    /// so tests share the same `Rc` directly.
    #[derive(Default)]
    struct MockActions {
        configured: RefCell<HashSet<String>>,
        shutting_down: RefCell<HashSet<String>>,
        /// Scripted respawn outcomes. `pop_front` per attempt; if the
        /// deque empties before the loop completes, attempts past the
        /// scripted ones return `Err("not scripted")` (which surfaces a
        /// test bug rather than silently passing).
        respawn_outcomes: RefCell<std::collections::VecDeque<Result<(), String>>>,
        respawn_calls: RefCell<Vec<String>>,
        pushes: RefCell<Vec<McpServerStatusPayload>>,
        /// Servers with an in-flight restart claim (mirrors the
        /// production `ShutdownState::in_flight_restart` set) so the
        /// dedup guard in `maybe_schedule_restart` can be exercised.
        in_flight: RefCell<HashSet<String>>,
        /// Servers configured as HTTP/SSE (for `is_http_server_configured`).
        http_configured: RefCell<HashSet<String>>,
        /// Scripted `reset_http_client` outcomes, per server.
        reset_outcomes: RefCell<
            std::collections::HashMap<String, std::collections::VecDeque<Result<(), String>>>,
        >,
        /// Recorded `reset_http_client` calls.
        reset_calls: RefCell<Vec<String>>,
        /// Recorded `unregister_server_tools` calls.
        unregister_calls: RefCell<Vec<String>>,
        /// Per-server early-death budgets (issue #45).
        budgets: RefCell<std::collections::HashMap<String, RestartBudget>>,
    }

    impl MockActions {
        fn new() -> Self {
            Self::default()
        }
        fn configure(&self, name: &str) {
            self.configured.borrow_mut().insert(name.to_string());
        }
        fn unconfigure(&self, name: &str) {
            self.configured.borrow_mut().remove(name);
        }
        fn mark_shutting_down(&self, name: &str) {
            self.shutting_down.borrow_mut().insert(name.to_string());
        }
        fn script_outcome(&self, outcome: Result<(), String>) {
            self.respawn_outcomes.borrow_mut().push_back(outcome);
        }
        fn respawn_call_count(&self) -> usize {
            self.respawn_calls.borrow().len()
        }
        fn pushes(&self) -> Vec<McpServerStatusPayload> {
            self.pushes.borrow().clone()
        }
        fn configure_http(&self, name: &str) {
            self.http_configured.borrow_mut().insert(name.to_string());
        }
        fn script_reset(&self, name: &str, outcome: Result<(), String>) {
            self.reset_outcomes
                .borrow_mut()
                .entry(name.to_string())
                .or_default()
                .push_back(outcome);
        }
        fn reset_calls(&self) -> Vec<String> {
            self.reset_calls.borrow().clone()
        }
        fn unregister_calls(&self) -> Vec<String> {
            self.unregister_calls.borrow().clone()
        }
        /// Mirror of the dispatcher's `flush_window` `Ready` arm (which
        /// calls `ShutdownState::note_recovery_ready`, not a
        /// `RestartActions` method — the dispatcher already holds the
        /// lock). Lets the unit tests drive the unpark path.
        fn note_recovery_ready(&self, name: &str) {
            self.budgets
                .borrow_mut()
                .entry(name.to_string())
                .or_default()
                .note_recovery_ready(tokio::time::Instant::now());
        }
        /// Consecutive early-death count for `name` (0 if never seen).
        /// The dedup test asserts on this directly: a rejected
        /// `maybe_schedule_restart` must leave the counter untouched,
        /// which no push / respawn / spawn assertion can observe.
        fn early_deaths(&self, name: &str) -> usize {
            self.budgets
                .borrow()
                .get(name)
                .map(|b| b.early_deaths)
                .unwrap_or(0)
        }
    }

    #[async_trait(?Send)]
    impl RestartActions for MockActions {
        async fn is_stdio_server_configured(&self, server: &str) -> bool {
            self.configured.borrow().contains(server)
        }
        fn is_in_shutting_down(&self, server: &str) -> bool {
            self.shutting_down.borrow().contains(server)
        }
        async fn respawn_stdio(&self, server: &str) -> Result<(), String> {
            self.respawn_calls.borrow_mut().push(server.to_string());
            let outcome = self
                .respawn_outcomes
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Err("not scripted".to_string()));
            if outcome.is_ok() {
                self.note_ready(server);
            }
            outcome
        }
        fn push_status(&self, payload: &McpServerStatusPayload) {
            self.pushes.borrow_mut().push(payload.clone());
        }
        fn begin_restart(&self, server: &str) -> bool {
            self.in_flight.borrow_mut().insert(server.to_string())
        }
        fn end_restart(&self, server: &str) {
            self.in_flight.borrow_mut().remove(server);
        }
        async fn is_http_server_configured(&self, server: &str) -> bool {
            self.http_configured.borrow().contains(server)
        }
        async fn reset_http_client(&self, server: &str) -> Result<(), String> {
            self.reset_calls.borrow_mut().push(server.to_string());
            self.reset_outcomes
                .borrow_mut()
                .get_mut(server)
                .and_then(|q| q.pop_front())
                .unwrap_or_else(|| Err("not scripted".to_string()))
        }
        fn unregister_server_tools(&self, server: &str) {
            self.unregister_calls.borrow_mut().push(server.to_string());
        }
        fn note_ready(&self, server: &str) {
            self.budgets
                .borrow_mut()
                .entry(server.to_string())
                .or_default()
                .note_ready(tokio::time::Instant::now());
        }
        fn classify_death(&self, server: &str, kind: McpClientEventKind) -> DeathDisposition {
            self.budgets
                .borrow_mut()
                .entry(server.to_string())
                .or_default()
                .classify_death(tokio::time::Instant::now(), kind)
        }
    }

    fn dyn_actions(mock: Rc<MockActions>) -> Rc<dyn RestartActions> {
        mock
    }

    /// A never-cancelled token for the happy-path tests.
    fn never_cancel() -> tokio_util::sync::CancellationToken {
        tokio_util::sync::CancellationToken::new()
    }

    async fn run_in_local<F, T>(f: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        let local = tokio::task::LocalSet::new();
        local.run_until(f).await
    }

    /// Contract: with all 3 attempts failing, respawn is called at
    /// `t=1s`, `t=5s`, `t=21s`. Uses `tokio::time::pause` +
    /// `advance(21s)`.
    #[tokio::test(start_paused = true)]
    async fn backoff_attempts_sequence() {
        run_in_local(async {
            let mock = Rc::new(MockActions::new());
            mock.configure("svr");
            mock.script_outcome(Err("e1".into()));
            mock.script_outcome(Err("e2".into()));
            mock.script_outcome(Err("e3".into()));

            let task = tokio::task::spawn_local(auto_restart_stdio(
                dyn_actions(mock.clone()),
                "sess-1".to_string(),
                "svr".to_string(),
                never_cancel(),
                StdDuration::ZERO,
            ));

            // t=0: nothing yet
            tokio::task::yield_now().await;
            assert_eq!(mock.respawn_call_count(), 0);

            // t=1s: first attempt fires
            tokio::time::advance(StdDuration::from_secs(1)).await;
            tokio::task::yield_now().await;
            assert_eq!(mock.respawn_call_count(), 1);

            // t=5s: second attempt fires (after 4s wait)
            tokio::time::advance(StdDuration::from_secs(4)).await;
            tokio::task::yield_now().await;
            assert_eq!(mock.respawn_call_count(), 2);

            // t=21s: third attempt fires (after 16s wait)
            tokio::time::advance(StdDuration::from_secs(16)).await;
            tokio::task::yield_now().await;
            assert_eq!(mock.respawn_call_count(), 3);

            task.await.unwrap();
        })
        .await;
    }

    /// Contract: if the server is removed from configs between the
    /// schedule call and the first backoff fires, respawn is NOT
    /// called and `mcp.auto_restart.skipped{reason="not_configured"}`
    /// is emitted (via `Reason::Disabled` push on the wire — see
    /// auto_restart_stdio rustdoc).
    #[tokio::test(start_paused = true)]
    async fn skip_when_not_configured() {
        run_in_local(async {
            let mock = Rc::new(MockActions::new());
            mock.configure("svr");
            let task = tokio::task::spawn_local(auto_restart_stdio(
                dyn_actions(mock.clone()),
                "sess-1".to_string(),
                "svr".to_string(),
                never_cancel(),
                StdDuration::ZERO,
            ));

            // Remove the config BEFORE the first 1s sleep elapses.
            mock.unconfigure("svr");
            tokio::time::advance(StdDuration::from_secs(1)).await;
            tokio::task::yield_now().await;
            task.await.unwrap();

            assert_eq!(
                mock.respawn_call_count(),
                0,
                "respawn must not run for an unconfigured server",
            );
            // The on-the-wire push is `Reason::Disabled` (not
            // `RestartFailed`) — see auto_restart_stdio rustdoc.
            let pushes = mock.pushes();
            assert_eq!(pushes.len(), 1);
            assert_eq!(pushes[0].reason, McpServerStatusReason::Disabled);
            assert_eq!(pushes[0].status, McpServerStatus::Unavailable);
        })
        .await;
    }

    /// Contract: same shape as `skip_when_not_configured` but the
    /// trigger is a toggle-disable (modeled the same way by
    /// `MockActions::unconfigure`). Verifies that the disabled-by-toggle
    /// path produces the same `Reason::Disabled` push that the
    /// not-configured path does — the wire schema is intentionally
    /// uniform here so the pager can render either with the same
    /// "disabled" affordance.
    #[tokio::test(start_paused = true)]
    async fn skip_when_disabled() {
        run_in_local(async {
            let mock = Rc::new(MockActions::new());
            mock.configure("svr");
            let task = tokio::task::spawn_local(auto_restart_stdio(
                dyn_actions(mock.clone()),
                "sess-1".to_string(),
                "svr".to_string(),
                never_cancel(),
                StdDuration::ZERO,
            ));

            // ToggleMcpServer(enabled=false) effectively drops the
            // entry from configs in the same way as a config-diff
            // removal.
            mock.unconfigure("svr");
            tokio::time::advance(StdDuration::from_secs(1)).await;
            tokio::task::yield_now().await;
            task.await.unwrap();

            assert_eq!(mock.respawn_call_count(), 0);
            let pushes = mock.pushes();
            assert_eq!(pushes.len(), 1);
            assert_eq!(pushes[0].reason, McpServerStatusReason::Disabled);
        })
        .await;
    }

    /// Contract: `maybe_schedule_restart` returns `false` (no task
    /// spawned) when the server is already in the dispatcher's
    /// `shutting_down` set. The kill_on_drop guard rail.
    #[tokio::test(start_paused = true)]
    async fn skip_when_in_shutting_down_set() {
        run_in_local(async {
            let mock = Rc::new(MockActions::new());
            mock.configure("svr");
            mock.mark_shutting_down("svr");

            let spawned = maybe_schedule_restart(
                dyn_actions(mock.clone()),
                "sess-1".to_string(),
                "svr".to_string(),
                McpClientEventKind::TransportClosed,
                never_cancel(),
            )
            .await;
            tokio::task::yield_now().await;

            assert!(!spawned);
            assert_eq!(mock.respawn_call_count(), 0);
            assert!(mock.pushes().is_empty());
        })
        .await;
    }

    /// Contract: HTTP-only servers never schedule a restart. We
    /// simulate the "not stdio" case by leaving the server
    /// **unconfigured** — production `is_stdio_server_configured`
    /// already returns `false` for HTTP/HttpAuth entries (see
    /// `acp_session.rs` impl). The dispatcher's TransportClosed event
    /// reaches `maybe_schedule_restart`, fails the stdio gate, emits
    /// `mcp.auto_restart.skipped{reason="not_configured"}`, and does
    /// NOT spawn the task.
    #[tokio::test(start_paused = true)]
    async fn http_event_does_not_trigger_restart() {
        run_in_local(async {
            let mock = Rc::new(MockActions::new());
            // Intentionally not configured as stdio — mirrors
            // production's gate behavior for HTTP servers.

            let spawned = maybe_schedule_restart(
                dyn_actions(mock.clone()),
                "sess-1".to_string(),
                "http-only".to_string(),
                McpClientEventKind::TransportClosed,
                never_cancel(),
            )
            .await;
            tokio::task::yield_now().await;

            assert!(
                !spawned,
                "HTTP/HttpAuth events must not spawn restart tasks"
            );
            assert_eq!(mock.respawn_call_count(), 0);
            assert!(mock.pushes().is_empty());
        })
        .await;
    }

    /// Contract (in-flight dedup): if a restart task
    /// is already in flight for a server, a second
    /// `maybe_schedule_restart` for the same server returns `false`,
    /// does NOT spawn a duplicate task, emits
    /// `mcp.auto_restart.skipped{reason="in_progress"}`, and — issue
    /// #45 — leaves the early-death budget untouched. Modeled by
    /// pre-claiming the in-flight slot (which the production
    /// `ShutdownState` set does atomically).
    ///
    /// The budget assertion is the load-bearing one: `run_dispatcher`
    /// calls us once per `(server, kind)` key, so one coalesce window
    /// carrying both a `TransportClosed` and a `HandshakeFailed` for the
    /// same server produces two calls. If the classifier ran before this
    /// guard, the rejected call would still burn a cycle and quietly
    /// halve the budget. `!spawned` / `respawn_call_count` / `pushes`
    /// all stay true while the counter moves, so none of them catch it.
    #[tokio::test(start_paused = true)]
    async fn dedup_skips_when_restart_already_in_flight() {
        run_in_local(async {
            let mock = Rc::new(MockActions::new());
            mock.configure("svr");
            // A handshake just landed, so a transport close now WOULD
            // classify as an early death — without this the classifier
            // would be a no-op and the counter assertion vacuous.
            mock.note_ready("svr");
            // Simulate an already-running restart task by claiming the
            // in-flight slot up front.
            assert!(mock.begin_restart("svr"));

            let spawned = maybe_schedule_restart(
                dyn_actions(mock.clone()),
                "sess-1".to_string(),
                "svr".to_string(),
                McpClientEventKind::TransportClosed,
                never_cancel(),
            )
            .await;
            tokio::task::yield_now().await;

            assert!(!spawned, "must not spawn a duplicate restart task");
            assert_eq!(mock.respawn_call_count(), 0);
            assert!(mock.pushes().is_empty());
            assert_eq!(
                mock.early_deaths("svr"),
                0,
                "a call rejected by the dedup guard must not burn an \
                 early-death cycle (classify AFTER begin_restart)",
            );
        })
        .await;
    }

    /// Contract (cancellation): cancelling the token
    /// before the first backoff sleep elapses aborts the task without
    /// calling `respawn_stdio` or emitting any wire push.
    #[tokio::test(start_paused = true)]
    async fn cancellation_aborts_backoff_before_respawn() {
        run_in_local(async {
            let mock = Rc::new(MockActions::new());
            mock.configure("svr");
            mock.script_outcome(Ok(()));
            let cancel = tokio_util::sync::CancellationToken::new();

            let task = tokio::task::spawn_local(auto_restart_stdio(
                dyn_actions(mock.clone()),
                "sess-1".to_string(),
                "svr".to_string(),
                cancel.clone(),
                StdDuration::ZERO,
            ));

            // Cancel during the first 1s backoff sleep.
            cancel.cancel();
            tokio::time::advance(StdDuration::from_secs(1)).await;
            tokio::task::yield_now().await;
            task.await.unwrap();

            assert_eq!(
                mock.respawn_call_count(),
                0,
                "cancelled task must not respawn",
            );
            assert!(
                mock.pushes().is_empty(),
                "cancelled task must not push status",
            );
        })
        .await;
    }

    /// Contract: a successful respawn pushes EXACTLY ONE wire
    /// notification, with `Reason::RestartSucceeded` (NOT
    /// `Initialized` — that's reserved for the first-time
    /// `ensure_initialized` Ready emit — AND NOT duplicated by a
    /// dispatcher-emitted `Initialized`).
    #[tokio::test(start_paused = true)]
    async fn respawn_emits_ready_with_reason_restart_succeeded() {
        run_in_local(async {
            let mock = Rc::new(MockActions::new());
            mock.configure("svr");
            mock.script_outcome(Ok(()));

            let task = tokio::task::spawn_local(auto_restart_stdio(
                dyn_actions(mock.clone()),
                "sess-1".to_string(),
                "svr".to_string(),
                never_cancel(),
                StdDuration::ZERO,
            ));

            tokio::time::advance(StdDuration::from_secs(1)).await;
            tokio::task::yield_now().await;
            task.await.unwrap();

            assert_eq!(mock.respawn_call_count(), 1);
            let pushes = mock.pushes();
            // Exactly one push per success. Production's respawn_stdio
            // wires set_event_tx AFTER ensure_initialized, so the
            // dispatcher's Ready-mapping does not also emit an
            // Initialized push (which would make two).
            assert_eq!(
                pushes.len(),
                1,
                "exactly one push per successful restart; got {pushes:?}"
            );
            assert_eq!(pushes[0].reason, McpServerStatusReason::RestartSucceeded);
            assert_ne!(pushes[0].reason, McpServerStatusReason::Initialized);
            assert_eq!(pushes[0].status, McpServerStatus::Ready);
        })
        .await;
    }

    /// Contract: three failed attempts produce three intermediate
    /// `Reason::RestartFailed` pushes (attempt 1, 2, 3) plus one final
    /// `Reason::RestartFailed` carrying `detail="exhausted after 3
    /// attempts"`.
    ///
    /// ## Telemetry coverage caveat
    ///
    /// The `mcp.auto_restart.exhausted` and per-attempt
    /// `mcp.auto_restart.attempted` counters are emitted via
    /// `tracing::info!` with metric-name `target:`s. This test does
    /// NOT install a `tracing` subscriber — if a future refactor
    /// accidentally deletes the `record_exhausted` / `record_attempted`
    /// calls, the wire-push assertion below would still pass while
    /// the counters silently disappear from telemetry. Acceptable because
    /// both call sites are right next to the wire push and likely to
    /// be deleted/edited together; tighter coverage is a follow-up.
    #[tokio::test(start_paused = true)]
    async fn all_three_attempts_fail_emits_exhausted_telemetry() {
        run_in_local(async {
            let mock = Rc::new(MockActions::new());
            mock.configure("svr");
            mock.script_outcome(Err("transport reset".into()));
            mock.script_outcome(Err("spawn failed".into()));
            mock.script_outcome(Err("handshake timeout".into()));

            let task = tokio::task::spawn_local(auto_restart_stdio(
                dyn_actions(mock.clone()),
                "sess-1".to_string(),
                "svr".to_string(),
                never_cancel(),
                StdDuration::ZERO,
            ));

            tokio::time::advance(StdDuration::from_secs(21)).await;
            tokio::task::yield_now().await;
            task.await.unwrap();

            assert_eq!(mock.respawn_call_count(), 3);
            let pushes = mock.pushes();
            // 3 per-attempt RestartFailed + 1 final exhausted RestartFailed.
            assert_eq!(pushes.len(), 4, "got pushes: {pushes:?}");
            for p in &pushes {
                assert_eq!(p.reason, McpServerStatusReason::RestartFailed);
                assert_eq!(p.status, McpServerStatus::Unavailable);
            }
            // Per-attempt details encode their attempt index.
            assert!(
                pushes[0]
                    .detail
                    .as_deref()
                    .map(|s| s.starts_with("attempt 1 of 3"))
                    .unwrap_or(false),
                "first push detail: {:?}",
                pushes[0].detail,
            );
            assert!(
                pushes[2]
                    .detail
                    .as_deref()
                    .map(|s| s.starts_with("attempt 3 of 3"))
                    .unwrap_or(false),
                "third push detail: {:?}",
                pushes[2].detail,
            );
            // Final push carries the exhausted marker.
            assert_eq!(
                pushes[3].detail.as_deref(),
                Some("exhausted after 3 attempts"),
            );
        })
        .await;
    }

    /// Contract: exhausting all three stdio respawn attempts unregisters
    /// the dead server's tools (so the model stops dispatching against a
    /// `not found` server) AND emits the four `RestartFailed` pushes.
    #[tokio::test(start_paused = true)]
    async fn exhaustion_unregisters_server_tools() {
        run_in_local(async {
            let mock = Rc::new(MockActions::new());
            mock.configure("svr");
            mock.script_outcome(Err("e1".into()));
            mock.script_outcome(Err("e2".into()));
            mock.script_outcome(Err("e3".into()));

            let task = tokio::task::spawn_local(auto_restart_stdio(
                dyn_actions(mock.clone()),
                "sess-1".to_string(),
                "svr".to_string(),
                never_cancel(),
                StdDuration::ZERO,
            ));

            tokio::time::advance(StdDuration::from_secs(21)).await;
            tokio::task::yield_now().await;
            task.await.unwrap();

            assert_eq!(mock.respawn_call_count(), 3);
            assert_eq!(
                mock.unregister_calls(),
                vec!["svr".to_string()],
                "exhausted restart must unregister the dead server's tools exactly once",
            );
        })
        .await;
    }

    /// Contract: a successful stdio respawn does NOT unregister tools —
    /// the recovered client serves the same registered tools.
    #[tokio::test(start_paused = true)]
    async fn successful_restart_keeps_tools_registered() {
        run_in_local(async {
            let mock = Rc::new(MockActions::new());
            mock.configure("svr");
            mock.script_outcome(Ok(()));

            let task = tokio::task::spawn_local(auto_restart_stdio(
                dyn_actions(mock.clone()),
                "sess-1".to_string(),
                "svr".to_string(),
                never_cancel(),
                StdDuration::ZERO,
            ));

            tokio::time::advance(StdDuration::from_secs(1)).await;
            tokio::task::yield_now().await;
            task.await.unwrap();

            assert!(
                mock.unregister_calls().is_empty(),
                "a recovered server must keep its tools registered",
            );
        })
        .await;
    }

    /// Contract: a `TransportClosed` for a configured HTTP server
    /// schedules an in-place `reset_http_client` (NOT a respawn) and emits
    /// no status of its own (`ensure_initialized` owns that).
    #[tokio::test(start_paused = true)]
    async fn http_recovery_schedules_reset_in_place() {
        run_in_local(async {
            let mock = Rc::new(MockActions::new());
            mock.configure_http("http-mcp-server");
            mock.script_reset("http-mcp-server", Ok(()));

            let spawned = maybe_schedule_http_recovery(
                dyn_actions(mock.clone()),
                "http-mcp-server".to_string(),
                never_cancel(),
            )
            .await;
            assert!(spawned);
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;

            assert_eq!(mock.reset_calls(), vec!["http-mcp-server".to_string()]);
            assert_eq!(
                mock.respawn_call_count(),
                0,
                "HTTP recovery must not spawn a stdio respawn",
            );
            assert!(
                mock.pushes().is_empty(),
                "HTTP recovery relies on ensure_initialized for status; no direct push",
            );
        })
        .await;
    }

    /// Contract: HTTP recovery is skipped for a server that is not a
    /// configured/enabled HTTP entry (e.g. removed, disabled, or stdio).
    #[tokio::test(start_paused = true)]
    async fn http_recovery_skips_when_not_http_configured() {
        run_in_local(async {
            let mock = Rc::new(MockActions::new());
            // Intentionally not http-configured.
            let spawned = maybe_schedule_http_recovery(
                dyn_actions(mock.clone()),
                "http-mcp-server".to_string(),
                never_cancel(),
            )
            .await;
            tokio::task::yield_now().await;

            assert!(!spawned);
            assert!(mock.reset_calls().is_empty());
        })
        .await;
    }

    /// Contract: HTTP recovery respects the `shutting_down` guard (config
    /// diff / toggle-off) — no reset is scheduled.
    #[tokio::test(start_paused = true)]
    async fn http_recovery_skips_when_shutting_down() {
        run_in_local(async {
            let mock = Rc::new(MockActions::new());
            mock.configure_http("http-mcp-server");
            mock.mark_shutting_down("http-mcp-server");

            let spawned = maybe_schedule_http_recovery(
                dyn_actions(mock.clone()),
                "http-mcp-server".to_string(),
                never_cancel(),
            )
            .await;
            tokio::task::yield_now().await;

            assert!(!spawned);
            assert!(mock.reset_calls().is_empty());
        })
        .await;
    }

    /// Contract: if the server is marked `shutting_down` AFTER scheduling
    /// but BEFORE the spawned task runs, the in-task re-check bails — no
    /// `reset_http_client`.
    #[tokio::test(start_paused = true)]
    async fn http_recovery_rechecks_shutting_down_before_reset() {
        run_in_local(async {
            let mock = Rc::new(MockActions::new());
            mock.configure_http("http-mcp-server");
            mock.script_reset("http-mcp-server", Ok(()));

            let spawned = maybe_schedule_http_recovery(
                dyn_actions(mock.clone()),
                "http-mcp-server".to_string(),
                never_cancel(),
            )
            .await;
            assert!(spawned);
            // Teardown lands before the spawned task gets to run.
            mock.mark_shutting_down("http-mcp-server");
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;

            assert!(
                mock.reset_calls().is_empty(),
                "task must re-check shutting_down and skip the reset",
            );
        })
        .await;
    }

    /// Contract: if the server is unconfigured/disabled AFTER scheduling but
    /// BEFORE the spawned task runs, the in-task re-check bails — no
    /// `reset_http_client`.
    #[tokio::test(start_paused = true)]
    async fn http_recovery_rechecks_configured_before_reset() {
        run_in_local(async {
            let mock = Rc::new(MockActions::new());
            mock.configure_http("http-mcp-server");
            mock.script_reset("http-mcp-server", Ok(()));

            let spawned = maybe_schedule_http_recovery(
                dyn_actions(mock.clone()),
                "http-mcp-server".to_string(),
                never_cancel(),
            )
            .await;
            assert!(spawned);
            // Server removed/disabled before the task runs.
            mock.http_configured.borrow_mut().remove("http-mcp-server");
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;

            assert!(
                mock.reset_calls().is_empty(),
                "task must re-check configured and skip the reset",
            );
        })
        .await;
    }

    /// Contract: HTTP recovery dedups against an in-flight recovery/restart
    /// for the same server (shared `begin_restart` slot).
    #[tokio::test(start_paused = true)]
    async fn http_recovery_dedups_when_already_in_flight() {
        run_in_local(async {
            let mock = Rc::new(MockActions::new());
            mock.configure_http("http-mcp-server");
            assert!(mock.begin_restart("http-mcp-server"));

            let spawned = maybe_schedule_http_recovery(
                dyn_actions(mock.clone()),
                "http-mcp-server".to_string(),
                never_cancel(),
            )
            .await;
            tokio::task::yield_now().await;

            assert!(!spawned, "must not schedule a duplicate recovery");
            assert!(mock.reset_calls().is_empty());
        })
        .await;
    }

    /// Contract: a failed first `reset_http_client` is retried on the
    /// [`HTTP_RECOVERY_BACKOFF`] ladder rather than parking after one shot.
    /// First attempt is immediate (`t=0`), the retry fires after the first
    /// backoff step (`t=1s`), and once it succeeds the loop stops.
    #[tokio::test(start_paused = true)]
    async fn http_recovery_retries_on_backoff_until_success() {
        run_in_local(async {
            let mock = Rc::new(MockActions::new());
            mock.configure_http("http-mcp-server");
            // First attempt fails (e.g. backend mid-redeploy), second wins.
            mock.script_reset("http-mcp-server", Err("transport closed".into()));
            mock.script_reset("http-mcp-server", Ok(()));

            let spawned = maybe_schedule_http_recovery(
                dyn_actions(mock.clone()),
                "http-mcp-server".to_string(),
                never_cancel(),
            )
            .await;
            assert!(spawned);

            // t=0: immediate first attempt fails; no retry yet.
            tokio::task::yield_now().await;
            assert_eq!(mock.reset_calls().len(), 1, "first attempt is immediate");

            // t=1s: first backoff step elapses → second attempt succeeds.
            tokio::time::advance(StdDuration::from_secs(1)).await;
            tokio::task::yield_now().await;
            assert_eq!(
                mock.reset_calls(),
                vec!["http-mcp-server".to_string(), "http-mcp-server".to_string()],
                "failed attempt must be retried after the first backoff step",
            );

            // No further attempts after success.
            tokio::time::advance(StdDuration::from_secs(60)).await;
            tokio::task::yield_now().await;
            assert_eq!(mock.reset_calls().len(), 2, "loop stops once recovered");
            assert!(
                mock.pushes().is_empty(),
                "HTTP recovery relies on ensure_initialized for status; no direct push",
            );
        })
        .await;
    }

    /// Contract: when every attempt fails, the loop tries once per
    /// `HTTP_RECOVERY_BACKOFF` step plus the immediate attempt, then parks
    /// the server (no more resets, no status push).
    #[tokio::test(start_paused = true)]
    async fn http_recovery_parks_after_exhausting_backoff() {
        run_in_local(async {
            let mock = Rc::new(MockActions::new());
            mock.configure_http("http-mcp-server");
            // Script one more failure than the total attempts so an
            // unexpected extra attempt would still be a scripted Err (and
            // the count assertion below catches it).
            for _ in 0..HTTP_RECOVERY_BACKOFF.len() + 2 {
                mock.script_reset("http-mcp-server", Err("still down".into()));
            }

            let spawned = maybe_schedule_http_recovery(
                dyn_actions(mock.clone()),
                "http-mcp-server".to_string(),
                never_cancel(),
            )
            .await;
            assert!(spawned);

            // Drive the whole ladder: immediate attempt + every backoff step.
            tokio::task::yield_now().await;
            for wait in HTTP_RECOVERY_BACKOFF {
                tokio::time::advance(wait).await;
                tokio::task::yield_now().await;
            }
            // Allow the parked/exhaustion path to run.
            tokio::time::advance(StdDuration::from_secs(60)).await;
            tokio::task::yield_now().await;

            assert_eq!(
                mock.reset_calls().len(),
                HTTP_RECOVERY_BACKOFF.len() + 1,
                "one immediate attempt plus one per backoff step, then park",
            );
            assert!(
                mock.pushes().is_empty(),
                "exhaustion parks silently; ensure_initialized owns status",
            );
        })
        .await;
    }

    /// `forward_status` and the dispatcher must agree on the wire
    /// method name. If someone renames
    /// `SERVER_STATUS_METHOD` only one path follows — this pinning
    /// test breaks loudly. We don't probe an actual ACP gateway —
    /// just assert the const referenced by `forward_status` is the
    /// same one re-exported by `mcp_dispatcher`.
    #[test]
    fn forward_status_uses_dispatcher_method() {
        assert_eq!(
            crate::session::mcp_dispatcher::SERVER_STATUS_METHOD,
            "x.ai/mcp/server_status",
            "wire method name pinned",
        );
        // The `forward_status` function uses
        // `mcp_dispatcher::SERVER_STATUS_METHOD` directly — same
        // const, no shadowing. If the import line at the top of
        // this file ever fans out a local copy, this test still
        // catches the wire name itself.
    }

    /// Drive one early-death restart cycle with a JoinHandle so the
    /// paused clock reliably wakes the task (same pattern as the
    /// existing BACKOFF unit tests).
    async fn drive_cycle(mock: &Rc<MockActions>, cycle_backoff: StdDuration) {
        let task = tokio::task::spawn_local(auto_restart_stdio(
            dyn_actions(mock.clone()),
            "sess-1".to_string(),
            "svr".to_string(),
            never_cancel(),
            cycle_backoff,
        ));
        // Register sleeps with the paused driver before advancing.
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        if !cycle_backoff.is_zero() {
            tokio::time::advance(cycle_backoff).await;
            for _ in 0..4 {
                tokio::task::yield_now().await;
            }
        }
        tokio::time::advance(StdDuration::from_secs(1)).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        task.await.unwrap();
    }

    fn early_park_detail(detail: Option<&str>) -> bool {
        detail.is_some_and(|d| d.contains("early exit") || d.contains("early-exit"))
    }

    /// Issue #45: handshake OK then immediate transport death must
    /// burn the early-death budget. After [`MAX_EARLY_DEATHS`] such
    /// cycles the server is parked with exactly one `RestartFailed`
    /// diagnostic — not one per cycle forever — and both the parking
    /// death and every subsequent already-parked death must drop the
    /// server's tools from the bridge (the client is already evicted by
    /// `drop_dead_clients`, so leaving them registered lets the model
    /// call a server that no longer exists).
    #[tokio::test(start_paused = true)]
    async fn early_death_cycles_exhaust_with_one_diagnostic() {
        run_in_local(async {
            let mock = Rc::new(MockActions::new());
            mock.configure("svr");
            for _ in 0..(MAX_EARLY_DEATHS + 3) {
                mock.script_outcome(Ok(()));
            }
            mock.note_ready("svr");

            for (cycle, &expected_backoff) in CYCLE_BACKOFF.iter().enumerate() {
                let disposition = mock.classify_death("svr", McpClientEventKind::TransportClosed);
                let DeathDisposition::Proceed { cycle_backoff } = disposition else {
                    panic!("cycle {cycle}: expected Proceed, got {disposition:?}");
                };
                assert_eq!(cycle_backoff, expected_backoff, "cycle {cycle} backoff",);
                drive_cycle(&mock, cycle_backoff).await;
                assert_eq!(
                    mock.respawn_call_count(),
                    cycle + 1,
                    "cycle {cycle}: expected one respawn per early-death cycle",
                );
            }

            // Next death goes through maybe_schedule_restart → Exhausted.
            let scheduled = maybe_schedule_restart(
                dyn_actions(mock.clone()),
                "sess-1".to_string(),
                "svr".to_string(),
                McpClientEventKind::TransportClosed,
                never_cancel(),
            )
            .await;
            assert!(
                !scheduled,
                "after {MAX_EARLY_DEATHS} early deaths auto-restart must stop",
            );
            assert_eq!(
                mock.respawn_call_count(),
                MAX_EARLY_DEATHS,
                "parked server must not respawn again",
            );

            let park_pushes: Vec<_> = mock
                .pushes()
                .into_iter()
                .filter(|p| {
                    p.reason == McpServerStatusReason::RestartFailed
                        && early_park_detail(p.detail.as_deref())
                })
                .collect();
            assert_eq!(
                park_pushes.len(),
                1,
                "exactly one early-death park diagnostic; got {:?}",
                mock.pushes(),
            );
            assert_eq!(park_pushes[0].status, McpServerStatus::Unavailable);
            assert_eq!(
                mock.unregister_calls(),
                vec!["svr".to_string()],
                "the parking death must drop the server's tools",
            );

            // Already parked → silent on the wire, but still evicts the
            // tools: `drop_dead_clients` evicted the client again, and
            // this branch is the only place left to notice.
            let scheduled = maybe_schedule_restart(
                dyn_actions(mock.clone()),
                "sess-1".to_string(),
                "svr".to_string(),
                McpClientEventKind::TransportClosed,
                never_cancel(),
            )
            .await;
            assert!(!scheduled, "a parked server must not schedule a restart");
            let park_again = mock
                .pushes()
                .into_iter()
                .filter(|p| {
                    p.reason == McpServerStatusReason::RestartFailed
                        && early_park_detail(p.detail.as_deref())
                })
                .count();
            assert_eq!(
                park_again, 1,
                "already-parked must not emit another diagnostic"
            );
            assert_eq!(
                mock.unregister_calls(),
                vec!["svr".to_string(), "svr".to_string()],
                "an already-parked death must also unregister the tools",
            );
        })
        .await;
    }

    /// Issue #45: successive early-death restart cycles must wait
    /// increasing [`CYCLE_BACKOFF`] before the attempt ladder, not
    /// only the intra-cycle [`BACKOFF`].
    #[tokio::test(start_paused = true)]
    async fn restart_cycles_use_increasing_backoff() {
        run_in_local(async {
            assert!(
                CYCLE_BACKOFF[1] > CYCLE_BACKOFF[0]
                    && CYCLE_BACKOFF[2] > CYCLE_BACKOFF[1]
                    && CYCLE_BACKOFF[3] >= CYCLE_BACKOFF[2],
                "CYCLE_BACKOFF must be increasing (last step may cap)",
            );

            let mock = Rc::new(MockActions::new());
            mock.configure("svr");
            mock.script_outcome(Ok(()));
            mock.script_outcome(Ok(()));
            mock.note_ready("svr");

            let DeathDisposition::Proceed { cycle_backoff } =
                mock.classify_death("svr", McpClientEventKind::TransportClosed)
            else {
                panic!("expected Proceed for first early death");
            };
            assert_eq!(cycle_backoff, CYCLE_BACKOFF[0]);

            let task = tokio::task::spawn_local(auto_restart_stdio(
                dyn_actions(mock.clone()),
                "sess-1".to_string(),
                "svr".to_string(),
                never_cancel(),
                cycle_backoff,
            ));
            // Register the cycle-backoff sleep with the paused driver
            // before any advance (otherwise the first advance is lost).
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            // 1s into the cycle backoff — must not respawn yet.
            tokio::time::advance(StdDuration::from_secs(1)).await;
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            assert_eq!(
                mock.respawn_call_count(),
                0,
                "cycle 1 must wait cycle backoff before the attempt ladder",
            );
            // Finish cycle backoff, then attempt backoff (separate
            // advances: a newly-created sleep is not covered by the
            // advance that woke the previous one).
            tokio::time::advance(cycle_backoff - StdDuration::from_secs(1)).await;
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            assert_eq!(
                mock.respawn_call_count(),
                0,
                "cycle backoff done but attempt backoff still pending",
            );
            tokio::time::advance(StdDuration::from_secs(1)).await;
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            assert_eq!(
                mock.respawn_call_count(),
                1,
                "cycle 1 respawn after cycle backoff + attempt backoff",
            );
            task.await.unwrap();

            let DeathDisposition::Proceed { cycle_backoff } =
                mock.classify_death("svr", McpClientEventKind::TransportClosed)
            else {
                panic!("expected Proceed for second early death");
            };
            assert_eq!(cycle_backoff, CYCLE_BACKOFF[1]);
            assert!(
                cycle_backoff > CYCLE_BACKOFF[0],
                "second early-death cycle backoff must exceed the first",
            );

            let task = tokio::task::spawn_local(auto_restart_stdio(
                dyn_actions(mock.clone()),
                "sess-1".to_string(),
                "svr".to_string(),
                never_cancel(),
                cycle_backoff,
            ));
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            // Advance only as far as cycle 1's total wait — must not fire.
            let cycle1_total = CYCLE_BACKOFF[0] + StdDuration::from_secs(1);
            tokio::time::advance(cycle1_total).await;
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            assert_eq!(
                mock.respawn_call_count(),
                1,
                "cycle 2 backoff must be strictly longer than cycle 1",
            );
            tokio::time::advance(cycle_backoff - cycle1_total).await;
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            assert_eq!(
                mock.respawn_call_count(),
                1,
                "cycle 2 cycle-backoff done; attempt backoff still pending",
            );
            tokio::time::advance(StdDuration::from_secs(1)).await;
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            assert_eq!(
                mock.respawn_call_count(),
                2,
                "cycle 2 respawn after its longer cycle backoff",
            );
            task.await.unwrap();
        })
        .await;
    }

    /// Issue #45 regression guard: surviving past [`STABILITY_WINDOW`]
    /// after a handshake resets the early-death budget so occasional
    /// healthy restarts are not mis-parked.
    #[tokio::test(start_paused = true)]
    async fn surviving_past_stability_window_resets_budget() {
        run_in_local(async {
            let mock = Rc::new(MockActions::new());
            mock.configure("svr");
            for _ in 0..(MAX_EARLY_DEATHS * 2 + 2) {
                mock.script_outcome(Ok(()));
            }
            mock.note_ready("svr");

            for cycle in 0..(MAX_EARLY_DEATHS - 1) {
                let DeathDisposition::Proceed { cycle_backoff } =
                    mock.classify_death("svr", McpClientEventKind::TransportClosed)
                else {
                    panic!("pre-reset cycle {cycle}: expected Proceed");
                };
                drive_cycle(&mock, cycle_backoff).await;
            }
            let after_partial = mock.respawn_call_count();
            assert_eq!(after_partial, MAX_EARLY_DEATHS - 1);

            // Survive past the stability window — budget resets.
            tokio::time::advance(STABILITY_WINDOW).await;
            tokio::task::yield_now().await;

            let DeathDisposition::Proceed { cycle_backoff } =
                mock.classify_death("svr", McpClientEventKind::TransportClosed)
            else {
                panic!("post-window death must be a healthy Proceed");
            };
            assert_eq!(
                cycle_backoff,
                StdDuration::ZERO,
                "death after stability window must have zero cycle backoff",
            );
            drive_cycle(&mock, cycle_backoff).await;
            assert_eq!(
                mock.respawn_call_count(),
                after_partial + 1,
                "death after stability window must restart (budget reset)",
            );

            // Full early-death budget again after the reset.
            for (cycle, &expected_backoff) in CYCLE_BACKOFF.iter().enumerate() {
                let DeathDisposition::Proceed { cycle_backoff } =
                    mock.classify_death("svr", McpClientEventKind::TransportClosed)
                else {
                    panic!("post-reset early cycle {cycle}: expected Proceed");
                };
                // This is the assertion that actually fires if the
                // reset regresses: without `early_deaths = 0` the
                // counter carries over from the pre-window cycles and
                // this walk starts partway down (or past the end of)
                // the table instead of at CYCLE_BACKOFF[0]. Read a
                // failure here as "the stability window did not reset
                // the budget", not as a backoff-table typo.
                assert_eq!(
                    cycle_backoff, expected_backoff,
                    "post-reset cycle {cycle} must resume the cycle-backoff \
                     table from the top; a later entry means surviving the \
                     stability window failed to reset early_deaths",
                );
                drive_cycle(&mock, cycle_backoff).await;
            }
            assert_eq!(
                mock.respawn_call_count(),
                after_partial + 1 + MAX_EARLY_DEATHS,
            );
            let scheduled = maybe_schedule_restart(
                dyn_actions(mock.clone()),
                "sess-1".to_string(),
                "svr".to_string(),
                McpClientEventKind::TransportClosed,
                never_cancel(),
            )
            .await;
            assert!(
                !scheduled,
                "full early-death budget after a reset must still park",
            );
        })
        .await;
    }

    /// Issue #45 scope guard: `HandshakeFailed` must never feed the
    /// early-death classifier.
    ///
    /// `HandshakeFailed` is emitted for *any* `ensure_initialized`
    /// error, including a `startup_timeout_sec` timeout on a server
    /// whose child is alive and whose transport never closed. A server
    /// that intermittently overruns its startup timeout would otherwise
    /// burn a cycle per hiccup and be parked — under a message claiming
    /// an "early exit within 30s of handshake" that never happened.
    #[tokio::test(start_paused = true)]
    async fn handshake_failed_never_burns_the_early_death_budget() {
        let mut budget = RestartBudget::default();
        budget.note_ready(tokio::time::Instant::now());

        // Far more handshake failures than the budget, all well inside
        // the stability window.
        for i in 0..(MAX_EARLY_DEATHS * 3) {
            let disposition = budget.classify_death(
                tokio::time::Instant::now(),
                McpClientEventKind::HandshakeFailed,
            );
            assert_eq!(
                disposition,
                DeathDisposition::Proceed {
                    cycle_backoff: Duration::ZERO
                },
                "handshake failure {i} must proceed on the legacy path",
            );
        }

        // The transport-close budget is untouched: the next real
        // transport death is early death #1, not a park.
        assert_eq!(
            budget.classify_death(
                tokio::time::Instant::now(),
                McpClientEventKind::TransportClosed,
            ),
            DeathDisposition::Proceed {
                cycle_backoff: CYCLE_BACKOFF[0]
            },
            "handshake failures must not have advanced the cycle counter",
        );
    }

    /// Issue #45 recovery, the ordering half: a park must NOT suppress a
    /// `HandshakeFailed`.
    ///
    /// After a park, the only producer of a `HandshakeFailed` for a stdio
    /// server is a fresh `start_mcp_server` — the client the user just
    /// asked for by fixing the command and toggling the server back on
    /// (a parked server's old client cannot re-handshake in place; see
    /// `classify_death`'s doc). If that first handshake overruns
    /// `startup_timeout_sec`, it must still get the full attempt ladder
    /// rather than zero attempts, or recovery dies on its first stumble
    /// with nothing telling the user why.
    ///
    /// Swapping the `kind` and `parked` checks back fails this test.
    #[tokio::test(start_paused = true)]
    async fn a_park_does_not_suppress_a_handshake_failure() {
        let mut budget = RestartBudget::default();
        budget.note_ready(tokio::time::Instant::now());
        for _ in 0..MAX_EARLY_DEATHS {
            budget.classify_death(
                tokio::time::Instant::now(),
                McpClientEventKind::TransportClosed,
            );
        }
        assert_eq!(
            budget.classify_death(
                tokio::time::Instant::now(),
                McpClientEventKind::TransportClosed,
            ),
            DeathDisposition::Exhausted,
            "budget exhaustion parks",
        );
        assert_eq!(
            budget.classify_death(
                tokio::time::Instant::now(),
                McpClientEventKind::TransportClosed,
            ),
            DeathDisposition::AlreadyParked,
            "further transport deaths stay parked",
        );

        assert_eq!(
            budget.classify_death(
                tokio::time::Instant::now(),
                McpClientEventKind::HandshakeFailed,
            ),
            DeathDisposition::Proceed {
                cycle_backoff: Duration::ZERO
            },
            "a handshake failure is outside this budget, so the park must \
             not swallow it — that is the user's re-added server failing \
             its first handshake, and it deserves the attempt ladder",
        );

        assert_eq!(
            budget.classify_death(
                tokio::time::Instant::now(),
                McpClientEventKind::TransportClosed,
            ),
            DeathDisposition::AlreadyParked,
            "and letting it through must not have disturbed the park",
        );
    }

    /// Companion to the above through the real scheduling path: a
    /// `HandshakeFailed` restart is scheduled with NO cycle backoff, so
    /// it respawns on the plain `BACKOFF[0]` ladder.
    #[tokio::test(start_paused = true)]
    async fn handshake_failed_schedules_without_cycle_backoff() {
        run_in_local(async {
            let mock = Rc::new(MockActions::new());
            mock.configure("svr");
            mock.script_outcome(Ok(()));
            // A handshake just landed: a TransportClosed now would be
            // early death #1 with a non-zero CYCLE_BACKOFF[0].
            mock.note_ready("svr");

            let spawned = maybe_schedule_restart(
                dyn_actions(mock.clone()),
                "sess-1".to_string(),
                "svr".to_string(),
                McpClientEventKind::HandshakeFailed,
                never_cancel(),
            )
            .await;
            assert!(spawned, "HandshakeFailed must still schedule a restart");
            assert_eq!(
                mock.early_deaths("svr"),
                0,
                "HandshakeFailed must not burn an early-death cycle",
            );

            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            tokio::time::advance(BACKOFF[0]).await;
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            assert_eq!(
                mock.respawn_call_count(),
                1,
                "HandshakeFailed restart must fire on BACKOFF[0] alone, \
                 with no inter-cycle delay in front of it",
            );
        })
        .await;
    }

    /// Issue #45 regression guard, the other side of
    /// `dispatcher_ready_unparks_and_restores_the_budget`: a dispatcher
    /// `Ready` for a server that is **not** parked must NOT refund the
    /// early-death budget.
    ///
    /// `flush_window` runs before `maybe_schedule_restart`, so if this
    /// reset were ungated, a `Ready` coalescing into the same window as
    /// a `TransportClosed` would zero the counter and re-increment from
    /// zero every cycle — the original bug, reintroduced through the
    /// recovery path. Deleting the `if self.parked` gate in
    /// `note_recovery_ready` fails this test.
    #[tokio::test(start_paused = true)]
    async fn dispatcher_ready_while_unparked_does_not_refund_the_budget() {
        let mut budget = RestartBudget::default();
        budget.note_ready(tokio::time::Instant::now());

        // Burn part of the budget without reaching a park.
        for (cycle, &expected_backoff) in CYCLE_BACKOFF.iter().enumerate().take(3) {
            let disposition = budget.classify_death(
                tokio::time::Instant::now(),
                McpClientEventKind::TransportClosed,
            );
            assert_eq!(
                disposition,
                DeathDisposition::Proceed {
                    cycle_backoff: expected_backoff
                },
                "cycle {cycle} walks the table",
            );
        }

        // A dispatcher-observed handshake lands mid-loop. It refreshes
        // the stability window, but must not hand back the budget.
        budget.note_recovery_ready(tokio::time::Instant::now());

        assert_eq!(
            budget.classify_death(
                tokio::time::Instant::now(),
                McpClientEventKind::TransportClosed,
            ),
            DeathDisposition::Proceed {
                cycle_backoff: CYCLE_BACKOFF[3]
            },
            "an un-parked Ready must not rewind the cycle counter",
        );

        // And the park still arrives on schedule rather than never.
        assert_eq!(
            budget.classify_death(
                tokio::time::Instant::now(),
                McpClientEventKind::TransportClosed,
            ),
            DeathDisposition::Proceed {
                cycle_backoff: CYCLE_BACKOFF[4]
            },
        );
        assert_eq!(
            budget.classify_death(
                tokio::time::Instant::now(),
                McpClientEventKind::TransportClosed,
            ),
            DeathDisposition::Exhausted,
            "the budget must still be exhaustible after an un-parked Ready",
        );
    }

    /// Issue #45: a park must be recoverable. A handshake observed by
    /// the *dispatcher* (`flush_window`'s `Ready` arm → the production
    /// `ShutdownState::note_recovery_ready`) clears the park and
    /// restores the full budget, so a user who fixes the server's
    /// command and toggles it off/on gets auto-restart back without
    /// restarting the session.
    ///
    /// The distinction matters: [`auto_restart_stdio`]'s own success
    /// path reports via `note_ready`, which deliberately does NOT clear
    /// the park — if it did, every successful restart would refund the
    /// budget and re-create the original bug.
    #[tokio::test(start_paused = true)]
    async fn dispatcher_ready_unparks_and_restores_the_budget() {
        run_in_local(async {
            let mock = Rc::new(MockActions::new());
            mock.configure("svr");
            for _ in 0..(MAX_EARLY_DEATHS * 2 + 2) {
                mock.script_outcome(Ok(()));
            }
            mock.note_ready("svr");

            // Burn the whole budget, then park on the next death.
            for cycle in 0..MAX_EARLY_DEATHS {
                let DeathDisposition::Proceed { cycle_backoff } =
                    mock.classify_death("svr", McpClientEventKind::TransportClosed)
                else {
                    panic!("cycle {cycle}: expected Proceed");
                };
                drive_cycle(&mock, cycle_backoff).await;
            }
            let parking = maybe_schedule_restart(
                dyn_actions(mock.clone()),
                "sess-1".to_string(),
                "svr".to_string(),
                McpClientEventKind::TransportClosed,
                never_cancel(),
            )
            .await;
            assert!(!parking, "budget exhaustion must park");
            let respawns_at_park = mock.respawn_call_count();

            // A restart-path handshake must NOT lift the park.
            mock.note_ready("svr");
            let still_parked = maybe_schedule_restart(
                dyn_actions(mock.clone()),
                "sess-1".to_string(),
                "svr".to_string(),
                McpClientEventKind::TransportClosed,
                never_cancel(),
            )
            .await;
            assert!(
                !still_parked,
                "respawn-success note_ready must not clear a park",
            );

            // The dispatcher observing `Ready` does.
            mock.note_recovery_ready("svr");
            let recovered = maybe_schedule_restart(
                dyn_actions(mock.clone()),
                "sess-1".to_string(),
                "svr".to_string(),
                McpClientEventKind::TransportClosed,
                never_cancel(),
            )
            .await;
            assert!(
                recovered,
                "a dispatcher-observed Ready must restore auto-restart",
            );
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            // Back at the top of the table: CYCLE_BACKOFF[0] then
            // BACKOFF[0], in separate advances (a sleep created after an
            // advance is not covered by the advance that woke its
            // predecessor).
            tokio::time::advance(CYCLE_BACKOFF[0]).await;
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            tokio::time::advance(BACKOFF[0]).await;
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            assert_eq!(
                mock.respawn_call_count(),
                respawns_at_park + 1,
                "the unparked server must actually respawn again",
            );

            // ... and the budget is full again, not one death from a
            // re-park: MAX_EARLY_DEATHS - 1 further cycles all proceed.
            for (cycle, &expected_backoff) in CYCLE_BACKOFF.iter().enumerate().skip(1) {
                let DeathDisposition::Proceed { cycle_backoff } =
                    mock.classify_death("svr", McpClientEventKind::TransportClosed)
                else {
                    panic!("post-unpark cycle {cycle}: expected Proceed");
                };
                assert_eq!(
                    cycle_backoff, expected_backoff,
                    "post-unpark cycle {cycle} must walk the table from \
                     the top; an unpark that left early_deaths set would \
                     land further down it",
                );
                drive_cycle(&mock, cycle_backoff).await;
            }
            let re_parked = maybe_schedule_restart(
                dyn_actions(mock.clone()),
                "sess-1".to_string(),
                "svr".to_string(),
                McpClientEventKind::TransportClosed,
                never_cancel(),
            )
            .await;
            assert!(
                !re_parked,
                "an unparked server must still be parkable a second time",
            );
        })
        .await;
    }
}
