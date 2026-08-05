//! CLI argument parsing for the pager.
pub use crate::headless::OutputFormat;
use base64::Engine as _;
use clap::{ArgAction, Parser, Subcommand, ValueEnum, ValueHint};
use clap_complete::Shell;
use rand::TryRngCore as _;
use std::net::SocketAddr;
use std::path::PathBuf;
/// Top-level commands for the pager binary.
/// Help strings that name the state directory, for the places a `///` comment
/// cannot reach.
///
/// A doc comment becomes clap's help text verbatim and cannot interpolate, so
/// every path written in one is frozen at whatever it said when it was typed.
/// These moved to attributes for the same reason the runtime messages did: the
/// directory is `~/.medley` on a fresh install, an existing `~/.grok` on a
/// migrated one, and anywhere at all under `MEDLEY_HOME`.
fn setup_json_help() -> String {
    format!(
        "Print the fetched configuration as JSON instead of installing it; writes nothing to {}.",
        crate::util::display_grok_home_prefix()
    )
}

fn dashboard_help() -> String {
    format!(
        "Centralised, agent-native overview of every session (top-level and \
         subagents). Disabled when `[dashboard].enabled = false` in {} or when \
         the `GROK_AGENT_DASHBOARD=0` env var is set.",
        crate::util::display_user_grok_path("config.toml")
    )
}

fn leader_socket_help() -> String {
    format!(
        "Use a custom leader socket path instead of the default `{}`.",
        crate::util::display_user_grok_path("leader.sock")
    )
}

fn minimal_help() -> String {
    format!(
        "Experimental: scrollback-native rendering. Finalized blocks are printed \
         into the terminal's native scrollback (use the terminal's own scroll / \
         selection); a small pinned region holds the prompt + running turn. \
         Session-scoped only — does not write config. To default plain `grok` to \
         minimal, set `[ui] screen_mode = \"minimal\"` in {}.",
        crate::util::display_user_grok_path("config.toml")
    )
}

fn log_sampling_help() -> String {
    format!(
        "Write sampling events to {}.",
        crate::util::display_user_grok_path("logs/sampling.jsonl")
    )
}

/// `grok wrap --help`'s long form, built at runtime so the README pointer
/// names the directory this install actually uses. As a literal it read
/// `~/.grok/README.md`, which is neither where medley writes that file nor
/// where a `MEDLEY_HOME` install keeps it — a "see this file" line pointing at
/// a file that is not there.
fn wrap_long_about() -> String {
    format!(
        "\
Run any command inside a local PTY that forwards its clipboard to yours.

Wraps an arbitrary command (for example `docker exec`, `kubectl exec`, or a
remote shell) in a local pseudo-terminal, intercepts OSC 52 clipboard escape
sequences from its output, and writes them to your local system clipboard. This
makes copy work when the program runs somewhere that cannot reach your
clipboard (containers, SSH) and your terminal does not handle OSC 52 itself
(for example Apple Terminal). The wrapped command's terminal is also kept in
sync with your window size.

Examples:
  grok wrap docker exec -it my-container bash
  grok wrap kubectl exec -it my-pod -- bash

See {readme} for more information.
",
        readme = crate::util::display_user_grok_path("README.md")
    )
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Run Grok without the interactive UI
    Agent(Box<AgentArgs>),
    /// Show the configuration Grok discovers for this directory
    Inspect {
        /// Emit machine-readable JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Check terminal, clipboard, color, and input support without starting Grok
    Doctor(crate::doctor_cmd::DoctorArgs),
    /// Manage running leader processes
    Leader(LeaderMgmtArgs),
    /// Sign out and clear cached credentials
    Logout {
        /// Credential provider to sign out from.
        #[arg(long, value_enum, default_value = "xai")]
        provider: LoginProvider,
    },
    /// Sign in to Grok
    Login {
        /// Credential provider to sign in to.
        #[arg(long, value_enum, default_value = "xai")]
        provider: LoginProvider,
        /// Ignored (kept for backwards compatibility). OAuth2 is now the only auth method.
        #[arg(long, hide = true)]
        legacy: bool,
        /// Use Grok OAuth via auth.x.ai.
        #[arg(long = "oauth", alias = "oidc", conflicts_with_all = ["device_auth"])]
        oauth: bool,
        /// Use device-code authentication for headless/remote environments.
        #[arg(
            long = "device-auth",
            visible_alias = "device-code",
            conflicts_with_all = ["oauth"]
        )]
        device_auth: bool,
        /// Authenticate for remote development environments (hidden).
        ///
        /// Field is always present so match arms stay feature-unification-safe
        /// across Bazel/cargo graphs; clap only registers `--devbox` when
        /// `devbox-login` is enabled (`arg(skip)` otherwise → always false).
        #[arg(skip)]
        devbox: bool,
    },
    /// Inspect provider-scoped authentication state.
    Auth(AuthArgs),
    /// Manage MCP server configurations
    Mcp(crate::mcp_cmd::McpArgs),
    /// Manage plugins and marketplace sources
    Plugin(crate::plugin_cmd::PluginArgs),
    /// Manage cross-session memory
    Memory(crate::memory_cmd::MemoryArgs),
    /// List available models and exit
    Models,
    /// List, search, or restore sessions
    Sessions(crate::sessions_cmd::SessionsArgs),
    /// Fetch and install managed configuration
    Setup {
        /// Print the fetched configuration as JSON instead of installing it;
        /// writes nothing to the user state directory.
        #[arg(long, long_help = setup_json_help())]
        json: bool,
    },
    /// Share a session and print the share URL
    #[command(hide = true)]
    Share(crate::share_cmd::ShareArgs),
    /// Run any command with local clipboard support (OSC 52 → system clipboard).
    #[cfg_attr(not(any(unix, windows)), command(hide = true))]
    #[command(long_about = wrap_long_about())]
    Wrap(WrapArgs),
    /// Export a session transcript as Markdown
    Export(crate::export_cmd::ExportArgs),
    /// Export or upload session trace data
    Trace(crate::trace_cmd::TraceArgs),
    /// Check for updates or install a specific version
    Update {
        /// Check for updates without installing.
        #[arg(long)]
        check: bool,
        /// Emit machine-readable JSON output (for --check).
        #[arg(long)]
        json: bool,
        /// Force re-download and install even if already up to date.
        #[arg(long)]
        force_reinstall: bool,
        /// Install a specific version (e.g. 0.1.150 or 0.1.151-alpha.2).
        #[arg(long)]
        version: Option<String>,
        /// Switch to the alpha release channel (faster updates, may have bugs).
        #[arg(long, conflicts_with_all = ["stable", "enterprise"])]
        alpha: bool,
        /// Switch to the stable release channel (default, weekly releases).
        #[arg(long, conflicts_with_all = ["alpha", "enterprise"])]
        stable: bool,
        /// Switch to the enterprise release channel.
        #[arg(long, conflicts_with_all = ["alpha", "stable"], hide = true)]
        enterprise: bool,
    },
    /// Print version information
    #[command(visible_alias = "v")]
    Version {
        /// Emit machine-readable JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Generate shell completion scripts (bash, zsh, fish, powershell, ...)
    Completions {
        /// Target shell
        #[arg(value_enum)]
        shell: Shell,
    },
    /// Manage git worktrees
    Worktree(crate::worktree_cmd::WorktreeArgs),
    /// Expose this workspace to the Computer Hub (via the leader).
    ///
    /// Disabled by default and enabled server-side per account; set
    /// `GROK_WORKSPACE_COMMAND=1` to enable it locally for testing.
    #[command(hide = true)]
    Workspace(WorkspaceMgmtArgs),
    /// Open the Agent Dashboard view at startup.
    #[command(long_about = dashboard_help())]
    Dashboard,
}

/// Authentication provider selected by login/logout/status commands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum LoginProvider {
    /// The normal Grok/xAI account session.
    #[default]
    Xai,
    /// ChatGPT Codex OAuth, stored separately from the Grok session.
    OpenaiCodex,
}

impl LoginProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Xai => "xai",
            Self::OpenaiCodex => "openai-codex",
        }
    }

    /// Provider-aware validation kept separate from clap because these legacy
    /// flags remain valid for the default xAI login but must never alter the
    /// fixed Codex OAuth flow.
    pub fn validate_login_flags(
        self,
        legacy: bool,
        oauth: bool,
        device_auth: bool,
        devbox: bool,
    ) -> Result<(), &'static str> {
        if self == Self::OpenaiCodex && (legacy || oauth || device_auth || devbox) {
            return Err(
                "--oauth, --device-auth, --devbox, and --legacy are xAI-only; omit them when \
                 using --provider openai-codex",
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, clap::Args)]
pub struct AuthArgs {
    #[command(subcommand)]
    pub command: AuthCommand,
}

#[derive(Debug, Clone, Subcommand)]
pub enum AuthCommand {
    /// Show non-secret authentication readiness for a provider.
    Status {
        #[arg(long, value_enum)]
        provider: LoginProvider,
        /// Emit machine-readable JSON output.
        #[arg(long)]
        json: bool,
    },
}
/// Arguments for the `wrap` subcommand: the command to run, then its args.
#[derive(Debug, clap::Args, Clone)]
pub struct WrapArgs {
    /// Command to run, followed by its arguments
    /// (e.g. `docker exec -it my-container bash`).
    /// On Unix a single quoted string or an aliased command runs via `$SHELL -i -c`.
    #[arg(
        required = true,
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "CMD"
    )]
    pub command: Vec<String>,
}
/// Targets a running leader process by PID (used by `grok leader` / `grok workspace`).
#[derive(Debug, clap::Args, Clone, Default)]
pub struct LeaderTargetArgs {
    /// Leader process ID from `grok leader list`.
    #[arg(long)]
    pub pid: Option<u32>,
}
#[derive(Debug, clap::Args, Clone)]
pub struct LeaderMgmtArgs {
    #[command(subcommand)]
    pub command: LeaderMgmtCommand,
}
#[derive(Debug, Subcommand, Clone)]
pub enum LeaderMgmtCommand {
    /// List running leader processes
    List {
        /// Emit machine-readable JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Show details for a leader process
    Info {
        #[command(flatten)]
        target: LeaderTargetArgs,
        /// Emit machine-readable JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Stop all running leader processes
    Kill,
}
#[derive(Debug, clap::Args, Clone)]
pub struct WorkspaceMgmtArgs {
    #[command(subcommand)]
    pub command: WorkspaceMgmtCommand,
}
#[derive(Subcommand, Clone)]
pub enum WorkspaceMgmtCommand {
    /// Start (or update) the workspace→hub exposure.
    Start(WorkspaceStartArgs),
    /// Drain and disconnect from the hub, keeping the exposure warm.
    Pause {
        #[command(flatten)]
        target: LeaderTargetArgs,
        /// Emit machine-readable JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Reconnect a paused exposure to the hub.
    Resume {
        #[command(flatten)]
        target: LeaderTargetArgs,
        /// Emit machine-readable JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Stop exposing the workspace (the leader keeps running).
    Stop {
        #[command(flatten)]
        target: LeaderTargetArgs,
        /// Emit machine-readable JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Restart the exposure (stop, then start with the given options).
    Restart(WorkspaceStartArgs),
    /// Show the current workspace-exposure status.
    #[command(visible_alias = "list")]
    Status {
        #[command(flatten)]
        target: LeaderTargetArgs,
        /// Emit machine-readable JSON output.
        #[arg(long)]
        json: bool,
    },
}
#[derive(clap::Args, Clone)]
pub struct WorkspaceStartArgs {
    /// Computer Hub WebSocket URL (default: `[hub].url`, then the prod hub).
    #[arg(long, value_name = "URL")]
    pub hub_url: Option<String>,
    /// Workspace root directory to expose. Defaults to the current directory.
    #[arg(long, value_name = "DIR", value_hint = ValueHint::DirPath)]
    pub cwd: Option<PathBuf>,
    /// Force leader mode for this command, overriding config.
    #[arg(long, conflicts_with = "no_leader")]
    pub leader: bool,
    /// Refuse to start even when config enables leader mode.
    #[arg(long, conflicts_with = "leader")]
    pub no_leader: bool,
    /// Emit machine-readable JSON output.
    #[arg(long)]
    pub json: bool,
}

impl std::fmt::Debug for WorkspaceMgmtCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Start(args) => f.debug_tuple("Start").field(args).finish(),
            Self::Pause { target, json } => f
                .debug_struct("Pause")
                .field("target", target)
                .field("json", json)
                .finish(),
            Self::Resume { target, json } => f
                .debug_struct("Resume")
                .field("target", target)
                .field("json", json)
                .finish(),
            Self::Stop { target, json } => f
                .debug_struct("Stop")
                .field("target", target)
                .field("json", json)
                .finish(),
            Self::Restart(args) => f.debug_tuple("Restart").field(args).finish(),
            Self::Status { target, json } => f
                .debug_struct("Status")
                .field("target", target)
                .field("json", json)
                .finish(),
        }
    }
}

impl std::fmt::Debug for WorkspaceStartArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceStartArgs")
            .field("hub_url_present", &self.hub_url.is_some())
            .field("cwd", &self.cwd)
            .field("leader", &self.leader)
            .field("no_leader", &self.no_leader)
            .field("json", &self.json)
            .finish()
    }
}
/// Arguments for the `agent` subcommand.
#[derive(clap::Args, Clone)]
pub struct AgentArgs {
    /// Run authentication before starting the agent
    #[arg(
        long = "reauth",
        visible_alias = "--reauthenticate",
        default_value = "false"
    )]
    pub reauthenticate: bool,
    /// Model ID to use
    #[arg(short = 'm', long = "model", value_name = "MODEL")]
    pub model: Option<String>,
    /// Reasoning effort for reasoning models
    #[clap(
        long = "reasoning-effort",
        visible_alias = "effort",
        value_name = "EFFORT",
        overrides_with = "reasoning_effort"
    )]
    pub reasoning_effort: Option<String>,
    /// Auto-approve all tool executions
    #[arg(long = "always-approve", alias = "yolo")]
    pub yolo: bool,
    /// Path to an agent profile file.
    #[arg(long = "agent-profile", value_name = "PATH")]
    pub agent_profile: Option<PathBuf>,
    /// Load a plugin from this directory for this process only (repeatable).
    /// Highest-priority plugin scope; always trusted — hooks and MCP servers
    /// activate without a prompt. Used by the Agent SDKs to inject
    /// per-connection plugins.
    #[arg(long = "plugin-dir", value_name = "DIR", value_hint = ValueHint::DirPath)]
    pub plugin_dirs: Vec<PathBuf>,
    /// Connect to a shared leader process instead of starting a new agent.
    /// Allows multiple clients to share one backend.
    /// Defaults to [cli] use_leader in config.toml.
    #[arg(long, conflicts_with = "no_leader")]
    pub leader: bool,
    /// Start a new agent even when config enables leader mode.
    #[arg(long, conflicts_with = "leader")]
    pub no_leader: bool,
    #[command(flatten)]
    pub headless: HeadlessArgs,
    /// Override the CLI chat proxy base URL.
    #[arg(long = "cli-chat-proxy-base-url")]
    pub cli_chat_proxy_base_url: Option<String>,
    /// Override the public xAI API base URL.
    #[arg(long = "xai-api-base-url")]
    pub xai_api_base_url: Option<String>,
    /// Agent runtime mode
    #[command(subcommand)]
    pub mode: Option<AgentCmd>,
}

impl std::fmt::Debug for AgentArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentArgs")
            .field("reauthenticate", &self.reauthenticate)
            .field("model_present", &self.model.is_some())
            .field("reasoning_effort_present", &self.reasoning_effort.is_some())
            .field("yolo", &self.yolo)
            .field("agent_profile_present", &self.agent_profile.is_some())
            .field("plugin_dir_count", &self.plugin_dirs.len())
            .field("leader", &self.leader)
            .field("no_leader", &self.no_leader)
            .field("headless", &self.headless)
            .field(
                "cli_chat_proxy_base_url_present",
                &self.cli_chat_proxy_base_url.is_some(),
            )
            .field("xai_api_base_url_present", &self.xai_api_base_url.is_some())
            .field("mode", &self.mode)
            .finish()
    }
}
impl AgentArgs {
    /// Canonicalized `--plugin-dir` paths, warning to stderr and skipping
    /// anything that isn't an existing directory (stderr is safe: JSON-RPC
    /// rides stdout).
    pub fn canonical_plugin_dirs(&self) -> Vec<PathBuf> {
        self.plugin_dirs
            .iter()
            .filter_map(|p| match dunce::canonicalize(p) {
                Ok(canonical) if canonical.is_dir() => Some(canonical),
                Ok(_) => {
                    eprintln!(
                        "grok: --plugin-dir {}: not a directory; skipping",
                        p.display()
                    );
                    None
                }
                Err(e) => {
                    eprintln!("grok: --plugin-dir {}: {e}; skipping", p.display());
                    None
                }
            })
            .collect()
    }
}
/// Agent sub-subcommands.
#[derive(Subcommand, Clone)]
pub enum AgentCmd {
    /// Run the agent over stdio
    Stdio,
    /// Run the agent headlessly over the Grok WebSocket relay
    Headless(HeadlessArgs),
    /// Run the agent as a WebSocket server
    Serve(ServeArgs),
    /// Run as the shared leader process for other clients
    Leader(LeaderArgs),
}

impl std::fmt::Debug for AgentCmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stdio => f.write_str("Stdio"),
            Self::Headless(args) => f.debug_tuple("Headless").field(args).finish(),
            Self::Serve(args) => f.debug_tuple("Serve").field(args).finish(),
            Self::Leader(args) => f.debug_tuple("Leader").field(args).finish(),
        }
    }
}
/// WebSocket URL override arguments, used by headless / leader / serve modes.
#[derive(clap::Args, Clone, Default)]
pub struct HeadlessArgs {
    #[arg(long = "grok-ws-origin")]
    pub grok_ws_origin: Option<String>,
    #[arg(long = "grok-ws-url")]
    pub grok_ws_url: Option<String>,
}

impl std::fmt::Debug for HeadlessArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeadlessArgs")
            .field("grok_ws_origin_present", &self.grok_ws_origin.is_some())
            .field("grok_ws_url_present", &self.grok_ws_url.is_some())
            .finish()
    }
}
/// Arguments for the `agent serve` subcommand.
#[derive(clap::Args, Clone)]
pub struct ServeArgs {
    /// Address for the server to listen on
    #[arg(long, default_value = "127.0.0.1:2419")]
    pub bind: SocketAddr,
    /// Secret token for client authentication
    #[arg(long, env = "GROK_AGENT_SECRET", hide_env_values = true)]
    pub secret: Option<String>,
    /// Acknowledge binding the plaintext WebSocket listener beyond loopback
    #[arg(long)]
    pub allow_remote: bool,
    /// Remote agent URL for proxy mode
    #[arg(long)]
    pub remote: Option<String>,
    /// Authentication and WebSocket URL overrides
    #[command(flatten)]
    pub headless: HeadlessArgs,
}

impl std::fmt::Debug for ServeArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServeArgs")
            .field("bind", &self.bind)
            .field("secret_present", &self.secret.is_some())
            .field("allow_remote", &self.allow_remote)
            .field("remote_present", &self.remote.is_some())
            .field(
                "grok_ws_origin_present",
                &self.headless.grok_ws_origin.is_some(),
            )
            .field("grok_ws_url_present", &self.headless.grok_ws_url.is_some())
            .finish()
    }
}

impl ServeArgs {
    /// Generate a 256-bit URL-safe server secret from the operating system RNG.
    pub fn generate_secret() -> anyhow::Result<String> {
        let mut bytes = [0_u8; 32];
        rand::rngs::OsRng
            .try_fill_bytes(&mut bytes)
            .map_err(|error| anyhow::anyhow!("failed to generate agent server secret: {error}"))?;
        Ok(encode_secret(&bytes))
    }
}

fn encode_secret(bytes: &[u8; 32]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}
/// Arguments for the `agent leader` subcommand.
#[derive(clap::Args, Clone)]
pub struct LeaderArgs {
    /// Keep the leader running after the last client disconnects.
    #[arg(long)]
    pub no_exit_on_disconnect: bool,
    /// Defer the grok.com relay WebSocket until the first headless IPC client
    /// registers. Without this flag the leader connects the relay eagerly at
    /// startup — required for bare leaders (headless remote env / systemd) that
    /// receive remote prompts *through* the relay. Passed by leaders auto-spawned
    /// from interactive clients (TUI/IDE), which only need the relay if a
    /// headless client appears.
    #[arg(long)]
    pub relay_on_demand: bool,
    /// Disable periodic auto-update checks for the leader.
    #[arg(long)]
    pub no_auto_update: bool,
    /// All environment URL overrides (passed from follower process)
    #[command(flatten)]
    pub headless: HeadlessArgs,
}

impl std::fmt::Debug for LeaderArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeaderArgs")
            .field("no_exit_on_disconnect", &self.no_exit_on_disconnect)
            .field("relay_on_demand", &self.relay_on_demand)
            .field("no_auto_update", &self.no_auto_update)
            .field("headless", &self.headless)
            .finish()
    }
}
#[derive(Debug, Clone, Parser)]
#[command(
    name = "grok",
    version = env!("VERSION_WITH_COMMIT"),
    about = "Grok Build TUI",
    disable_version_flag = true,
    next_display_order = None,
    help_template = "\
{before-help}{about-with-newline}
{usage-heading} {usage}

Arguments:
{positionals}

Options:
{options}

Commands:
{subcommands}{after-help}\
"
)]
pub struct PagerArgs {
    /// Print version
    #[arg(short = 'v', short_alias = 'V', long = "version", action = ArgAction::SetTrue)]
    pub version: bool,
    /// Working directory.
    #[arg(long)]
    pub cwd: Option<PathBuf>,
    /// Use a custom leader socket path instead of the default one under the
    /// user state directory.
    #[arg(
        long_help = leader_socket_help(),
        long = "leader-socket",
        value_name = "PATH",
        global = true,
        value_hint = ValueHint::FilePath
    )]
    pub leader_socket: Option<PathBuf>,
    /// Enable debug logging.
    #[arg(long = "debug", global = true)]
    pub debug: bool,
    /// Write debug logs to FILE.
    #[arg(
        long = "debug-file",
        value_name = "FILE",
        global = true,
        value_hint = ValueHint::FilePath
    )]
    pub debug_file: Option<PathBuf>,
    /// Auto-approve all tool executions.
    #[clap(
        long = "always-approve",
        alias = "yolo",
        alias = "dangerously-skip-permissions"
    )]
    pub yolo: bool,
    /// Trust this folder and persist the decision to the trust store.
    #[arg(long = "trust", alias = "trust-folder", hide = true)]
    pub trust: bool,
    /// Permission allow rule (compat alias: --allowedTools).
    #[arg(
        long = "allow",
        alias = "allowedTools",
        value_name = "RULE",
        value_delimiter = ','
    )]
    pub allow_rules: Vec<String>,
    /// Permission deny rule (compat alias: --disallowedTools).
    #[arg(
        long = "deny",
        alias = "disallowedTools",
        value_name = "RULE",
        value_delimiter = ','
    )]
    pub deny_rules: Vec<String>,
    /// Single-turn prompt. Prints the response to stdout and exits.
    #[clap(
        short = 'p',
        long = "single",
        alias = "print",
        value_name = "PROMPT",
        conflicts_with_all = &["prompt_json",
        "prompt_file"]
    )]
    pub single: Option<String>,
    /// Single-turn prompt as JSON content blocks.
    #[clap(
        long = "prompt-json",
        value_name = "JSON",
        conflicts_with_all = &["single",
        "prompt_file"]
    )]
    pub prompt_json: Option<String>,
    /// Single-turn prompt from a file.
    #[clap(
        long = "prompt-file",
        value_name = "PATH",
        conflicts_with_all = &["single",
        "prompt_json"],
        value_hint = ValueHint::FilePath
    )]
    pub prompt_file: Option<PathBuf>,
    /// Send the prompt exactly as given.
    #[clap(long)]
    pub verbatim: bool,
    /// Output format for headless mode.
    #[clap(long = "output-format", value_enum, default_value = "plain")]
    pub output_format: OutputFormat,
    /// Emit incremental `stream_event` lines (text/thinking deltas) alongside
    /// whole messages. Only affects `--output-format streaming-messages-json`.
    #[clap(long = "include-partial-messages")]
    pub include_partial_messages: bool,
    /// JSON Schema for structured output. When set, the model is constrained to
    /// produce JSON matching this schema. Implies --output-format json.
    /// Example: --json-schema '{"type":"object","properties":{"name":{"type":"string"}}}'
    #[clap(long = "json-schema", value_name = "SCHEMA")]
    pub json_schema: Option<String>,
    /// Model ID to use.
    #[clap(short = 'm', long = "model", value_name = "MODEL")]
    pub model: Option<String>,
    /// Reasoning effort for reasoning models
    #[clap(
        long = "reasoning-effort",
        visible_alias = "effort",
        value_name = "EFFORT",
        overrides_with = "reasoning_effort"
    )]
    pub reasoning_effort: Option<String>,
    /// Extra rules to append to the system prompt.
    #[clap(long = "rules", alias = "append-system-prompt")]
    pub rules: Option<String>,
    /// Compaction mode [summary|transcript|segments]: `summary` (default) adds
    /// no pointer; `transcript` points at the raw transcript; `segments`
    /// persists per-segment markdown to grep. Sets `GROK_COMPACTION_MODE`.
    #[clap(long = "compaction-mode", value_name = "MODE", hide = true)]
    pub compaction_mode: Option<String>,
    /// Segments verbatim detail [none|minimal|balanced|verbose] (default
    /// `verbose`). Only affects `--compaction-mode segments`. Sets
    /// `GROK_COMPACTION_DETAIL`.
    #[clap(long = "compaction-detail", value_name = "DETAIL", hide = true)]
    pub compaction_detail: Option<String>,
    /// Override the agent's system prompt (compat alias: --system-prompt).
    #[clap(
        long = "system-prompt-override",
        alias = "system-prompt",
        value_name = "PROMPT"
    )]
    pub system_prompt_override: Option<String>,
    /// Resume a session by ID or title, or the most recent if omitted.
    /// Non-ID values match session titles for the current directory
    /// (ignoring letter case; a sole renamed match wins among duplicates,
    /// otherwise ambiguity errors; UUID-shaped values always mean IDs).
    #[arg(
        long = "resume",
        short = 'r',
        value_name = "SESSION_ID_OR_TITLE",
        num_args = 0..= 1,
        default_missing_value = "",
        conflicts_with_all = ["continue_last_session"]
    )]
    pub resume_session: Option<String>,
    /// Resume a previous session by session ID (alias for --resume).
    #[arg(
        long = "load",
        value_name = "SESSION_ID",
        hide = true,
        conflicts_with_all = ["continue_last_session"]
    )]
    pub load_session: Option<String>,
    /// Set by [`Self::pin_local_resume_target`]: the resume target was
    /// resolved (or definitively missed) before the OS sandbox, so
    /// materialization must not re-run local title selection.
    #[clap(skip)]
    pub resume_target_pinned: bool,
    /// Sandbox profile of the title-pinned session, captured at pin time from
    /// the selected summary (outer `None` = no title pin happened). The
    /// id-based peek cannot re-derive it: a legacy id duplicated across cwd
    /// dirs makes that lookup ambiguous.
    #[clap(skip)]
    pub(crate) pinned_resume_profile: Option<Option<String>>,
    /// Continue the most recent session for the current working directory.
    #[arg(
        short = 'c',
        long = "continue",
        conflicts_with_all = ["resume_session",
        "load_session"]
    )]
    pub continue_last_session: bool,
    /// Use a specific session UUID for a **new** conversation (must be a valid
    /// UUID and must not already exist under the target session directory).
    /// With `--resume`/`--continue`, only valid together with `--fork-session`
    /// (names the forked session). Does not resume existing sessions — use
    /// `--resume` / `--continue` instead.
    #[arg(short = 's', long = "session-id", value_name = "SESSION_ID")]
    pub session_id: Option<String>,
    /// When resuming (`--resume` / `--continue`), create a new session ID
    /// instead of reusing the original (optionally set via `--session-id`).
    #[arg(long = "fork-session")]
    pub fork_session: bool,
    /// Start the session in a new git worktree, optionally named.
    #[arg(short = 'w', long = "worktree", num_args = 0..= 1, default_missing_value = "")]
    pub worktree: Option<String>,
    /// Branch, tag, or commit to base the worktree on (with `--worktree`).
    /// Defaults to the current HEAD of the source checkout when omitted.
    #[arg(long = "worktree-ref", visible_alias = "ref", requires = "worktree")]
    pub worktree_ref: Option<String>,
    /// Check out the original session's commit when resuming.
    #[arg(long = "restore-code", requires = "resume_session")]
    pub restore_code: bool,
    /// Disable plan mode.
    #[arg(long = "no-plan")]
    pub no_plan: bool,
    /// Own a local `workspace_server` (replaces remote sandbox). Requires `--chat`.
    ///
    /// Compiled only with `--features local-workspace` (not implied by `chat`).
    #[cfg(feature = "local-workspace")]
    #[arg(
        long = "local-workspace",
        num_args = 0..= 1,
        value_name = "CWD",
        conflicts_with = "local_workspace_attach",
        requires = "chat"
    )]
    pub local_workspace: Option<Option<PathBuf>>,
    /// Attach an existing local `workspace_server` by `server_id`,
    /// replacing the chat sandbox (ExistingWorkspace only). Requires `--chat`.
    #[cfg(feature = "local-workspace")]
    #[arg(
        long = "local-workspace-attach",
        value_name = "SERVER_ID",
        conflicts_with = "local_workspace",
        requires = "chat"
    )]
    pub local_workspace_attach: Option<String>,
    /// Cwd override for local-workspace attach/own. Requires `--chat`.
    #[cfg(feature = "local-workspace")]
    #[arg(long = "local-workspace-cwd", value_name = "PATH", requires = "chat")]
    pub local_workspace_cwd: Option<PathBuf>,
    /// Disable subagent spawning.
    #[arg(long = "no-subagents")]
    pub no_subagents: bool,
    /// Disable structured question prompts from the agent.
    #[arg(long = "no-ask-user", hide = true)]
    pub no_ask_user: bool,
    /// Enable cross-session memory.
    #[arg(long = "experimental-memory", conflicts_with = "no_memory")]
    pub experimental_memory: bool,
    /// Disable cross-session memory for this session.
    #[arg(long = "no-memory", conflicts_with = "experimental_memory")]
    pub no_memory: bool,
    /// Agent name or definition file path.
    #[arg(long = "agent", value_name = "NAME")]
    pub agent: Option<String>,
    /// Inline subagent definitions as JSON.
    #[arg(long = "agents", value_name = "JSON")]
    pub agents_json: Option<String>,
    /// Built-in tools to allow (comma-separated).
    #[arg(long = "tools", value_name = "TOOLS")]
    pub cli_tools: Option<String>,
    /// Built-in tools to remove (comma-separated).
    #[arg(long = "disallowed-tools", value_name = "TOOLS")]
    pub cli_disallowed_tools: Option<String>,
    /// Maximum number of agent turns.
    #[arg(
        long = "max-turns",
        value_name = "N",
        value_parser = clap::value_parser!(u32).range(1..)
    )]
    pub max_turns: Option<u32>,
    /// Permission mode.
    #[arg(
        long = "permission-mode",
        value_name = "MODE",
        value_parser = clap::builder::PossibleValuesParser::new(
            xai_grok_shell::agent::config::PermissionMode::VALID_VALUES
        )
    )]
    pub permission_mode_flag: Option<String>,
    /// Disable web search and web fetch tools.
    #[arg(long = "disable-web-search")]
    pub disable_web_search: bool,
    /// Exit as soon as the first agent turn ends, without waiting for pending
    /// background bash/monitor tasks or background subagents (headless only).
    /// Default for all `grok -p` runs is to wait (up to `--background-wait-timeout`)
    /// so eval harnesses see full task completion. Use this for fast scripts that
    /// only need the first turn's text. Does not wait for server-side auto-wake
    /// output or persistent monitors (those hit the timeout).
    #[arg(long = "no-wait-for-background", hide = true)]
    pub no_wait_for_background: bool,
    /// Max seconds to wait for background work after the first turn ends
    /// (headless only). Applies to bash/monitor `task_completed`, background
    /// subagents (`SubagentFinished`), and any still-running non-persistent
    /// work. Persistent `monitor(persistent:true)` never completes and always
    /// waits the full timeout — use `--no-wait-for-background` or a lower
    /// timeout for throughput. Conflicts with `--no-wait-for-background`.
    #[arg(
        long = "background-wait-timeout",
        value_name = "SECS",
        default_value = "600",
        conflicts_with = "no_wait_for_background",
        hide = true,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    pub background_wait_timeout_secs: u64,
    /// Sandbox profile for filesystem and network access.
    #[arg(long, env = "GROK_SANDBOX", value_name = "PROFILE")]
    pub sandbox: Option<String>,
    /// Session storage mode: local or writeback.
    #[arg(long = "storage-mode", value_name = "MODE", hide = true)]
    pub storage_mode: Option<String>,
    /// Override the client identifier sent to the agent.
    #[arg(long = "client-identifier", value_name = "ID", hide = true)]
    pub client_identifier: Option<String>,
    /// Hunk tracker mode: agent_only, all_dirty, or off ("disabled" is an
    /// alias for off, which turns the hunk tracker off entirely).
    #[arg(long = "hunk-tracker-mode", value_name = "MODE", hide = true)]
    pub hunk_tracker_mode: Option<String>,
    /// Enable terminal support for the agent.
    #[arg(long = "terminal", hide = true)]
    pub terminal: bool,
    /// Enable client-side file reads.
    #[arg(long = "fs-read", hide = true)]
    pub fs_read: bool,
    /// Enable client-side file writes.
    #[arg(long = "fs-write", hide = true)]
    pub fs_write: bool,
    /// Disable automatic updates for this session.
    #[arg(long = "no-auto-update", hide = true)]
    pub no_auto_update: bool,
    /// Enable the runtime turn-end TodoGate for this session.
    ///
    /// Session-scoped (not persisted). Highest precedence —
    /// overrides remote `todo_gate_enabled` and the built-in
    /// default (which is `false`).
    #[arg(long = "todo-gate", hide = true)]
    pub todo_gate: bool,
    /// Set the installer field in config.toml.
    #[arg(long = "installer", value_name = "VALUE", hide = true)]
    pub installer: Option<String>,
    /// Run inline instead of using the terminal alternate screen.
    #[arg(long = "no-alt-screen")]
    pub no_alt_screen: bool,
    /// Experimental: scrollback-native rendering. Finalized blocks are printed
    /// into the terminal's native scrollback (use the terminal's own scroll /
    /// selection); a small pinned region holds the prompt + running turn.
    /// Session-scoped only — does not write config. To default plain `grok` to
    /// minimal, set `[ui] screen_mode = "minimal"` in the user config.
    #[arg(long = "minimal", long_help = minimal_help())]
    pub minimal: bool,
    /// Open in the standard fullscreen TUI for this session, overriding a
    /// config `[ui] screen_mode = "minimal"` preference. Session-scoped only —
    /// does not write config. Fullscreen-vs-inline still follows the alt-screen
    /// policy (--no-alt-screen, [terminal] alt_screen, terminal auto-detection).
    #[arg(long = "fullscreen", conflicts_with = "minimal")]
    pub fullscreen: bool,
    /// Write sampling events to the user state directory.
    #[arg(
        long_help = log_sampling_help(),
        long = "log-sampling",
        env = "GROK_LOG_SAMPLING",
        hide = true
    )]
    pub log_sampling: bool,
    /// Show the login screen even when credentials are already available.
    #[arg(long = "force-login", hide = true)]
    pub force_login: bool,
    /// Use OAuth when the welcome screen starts authentication.
    #[arg(long = "oauth")]
    pub oauth: bool,
    /// Connect to a shared leader process.
    #[arg(long, conflicts_with = "no_leader", hide = true)]
    pub leader: bool,
    /// Run standalone even when leader mode is configured.
    #[arg(long, conflicts_with = "leader", hide = true)]
    pub no_leader: bool,
    /// Initial prompt for the interactive session, e.g. `grok "fix the bug"` or `grok --worktree=feat "create this feature"`.
    #[arg(
        value_name = "PROMPT",
        conflicts_with_all = &["single",
        "prompt_json",
        "prompt_file"]
    )]
    pub prompt: Option<String>,
    /// Subcommand (e.g., `agent`).
    #[command(subcommand, next_display_order = 0)]
    pub command: Option<Command>,
}
/// Outcome of resolving the startup sandbox profile for a (possibly resumed)
/// session. See [`PagerArgs::startup_sandbox_profile`].
#[derive(Debug, PartialEq, Eq)]
pub enum SandboxStartup {
    /// Apply this profile. `None` means fall through to config/`off`.
    Apply(Option<String>),
    /// Resume requested a profile that differs from the one the session was
    /// created with. Refused so resuming can't silently change the sandbox.
    Conflict { requested: String, saved: String },
}
/// How resume-selection flags resolve for sandbox profile lookup.
/// Derived from [`PagerArgs::session_startup_intent`]; new-with-id is not a resume.
#[derive(Debug, PartialEq, Eq)]
pub enum ResumeTarget {
    /// Resume (or fork-from) a specific session id.
    SessionId(String),
    /// Resume (or fork-from) the most recent session for the current directory.
    MostRecentForCwd,
    /// Not resuming an existing session (new auto or new-with-id).
    None,
}
fn anchor_to_launch_dir(path: PathBuf, launch_dir: Option<&std::path::Path>) -> PathBuf {
    if path.is_absolute() {
        strip_cur_dir(path)
    } else if let Some(launch_dir) = launch_dir {
        strip_cur_dir(launch_dir.join(path))
    } else {
        strip_cur_dir(path)
    }
}
fn strip_cur_dir(path: PathBuf) -> PathBuf {
    path.components()
        .filter(|component| !matches!(component, std::path::Component::CurDir))
        .collect()
}
/// Re-export: lives in `xai-grok-config` so the shell crate can reach it too
/// (dependency runs pager → shell; an accessor here would be unreachable).
pub use xai_grok_config::program_name::{
    FALLBACK_PROGRAM_NAME, program_name, program_name_for_instruction,
};

impl PagerArgs {
    /// Parse CLI arguments without applying side effects.
    pub fn parse_cli() -> Self {
        // `program_name()` self-initializes from argv[0]; clap's bin_name is
        // the same value so usage lines match what we call ourselves elsewhere.
        let bin_name = program_name().to_owned();
        Self::parse_from(std::iter::once(bin_name).chain(std::env::args().skip(1)))
    }
    /// Apply launch-directory path anchoring and `--cwd` after early commands
    /// have been dispatched without filesystem or process initialization.
    pub fn apply_cwd(self) -> anyhow::Result<Self> {
        let launch_dir = std::env::current_dir().ok();
        self.apply_cwd_from(launch_dir.as_deref())
    }
    fn apply_cwd_from(mut self, launch_dir: Option<&std::path::Path>) -> anyhow::Result<Self> {
        if let Some(socket) = self.leader_socket.take() {
            self.leader_socket = Some(anchor_to_launch_dir(socket, launch_dir));
        }
        if let Some(file) = self.debug_file.take() {
            self.debug_file = Some(anchor_to_launch_dir(file, launch_dir));
        }
        if let Some(ref cwd) = self.cwd {
            std::env::set_current_dir(cwd).map_err(|e| {
                anyhow::anyhow!("Failed to set working directory to {:?}: {}", cwd, e)
            })?;
        }
        Ok(self)
    }
    /// Optional-flag accessor; always `false` in builds without the optional
    /// feature, so call sites need no `cfg` of their own.
    pub fn chat(&self) -> bool {
        false
    }
    /// `--local-workspace[=cwd]` own-mode flag.
    #[cfg(feature = "local-workspace")]
    pub fn local_workspace(&self) -> Option<Option<&std::path::Path>> {
        self.local_workspace.as_ref().map(|inner| inner.as_deref())
    }
    /// `--local-workspace-attach=<server_id>`.
    #[cfg(feature = "local-workspace")]
    pub fn local_workspace_attach(&self) -> Option<&str> {
        self.local_workspace_attach.as_deref()
    }
    /// `--local-workspace-cwd=<path>`.
    #[cfg(feature = "local-workspace")]
    pub fn local_workspace_cwd(&self) -> Option<&std::path::Path> {
        self.local_workspace_cwd.as_deref()
    }
    /// Get the session ID to resume, from either --resume or --load (hidden alias).
    ///
    /// Returns `None` when `--resume` was used without a value (the empty-string
    /// sentinel). Use [`resume_most_recent`] to detect that case.
    pub fn session_to_resume(&self) -> Option<&str> {
        self.resume_session
            .as_deref()
            .or(self.load_session.as_deref())
            .filter(|s| !s.is_empty())
    }
    /// Whether `--resume` was used without a session ID (meaning "resume most recent").
    pub fn resume_most_recent(&self) -> bool {
        self.resume_session.as_deref() == Some("")
    }
    /// Classify flags for sandbox profile lookup on an existing session.
    ///
    /// Uses [`Self::session_startup_intent`]; invalid combos fall through to
    /// `None` (caller should have rejected intent errors earlier at startup).
    pub fn resume_target(&self) -> ResumeTarget {
        use crate::app::session_startup::SessionStartupIntent;
        match self.session_startup_intent() {
            Ok(SessionStartupIntent::Resume {
                session_id: Some(id),
                ..
            })
            | Ok(SessionStartupIntent::ForkFrom {
                source_session_id: Some(id),
                ..
            }) => ResumeTarget::SessionId(id),
            Ok(SessionStartupIntent::Resume {
                most_recent_for_cwd: true,
                ..
            })
            | Ok(SessionStartupIntent::ForkFrom {
                most_recent_for_cwd: true,
                ..
            }) => ResumeTarget::MostRecentForCwd,
            _ => ResumeTarget::None,
        }
    }
    /// Resolve the sandbox profile to apply at startup, accounting for the
    /// profile the resumed session was created with. `saved` is the resumed
    /// session's persisted profile (read once via [`Self::saved_resume_profile`]).
    ///
    /// A session's profile is fixed at creation. Resuming restores it; passing an
    /// explicit `--sandbox`/`GROK_SANDBOX` that differs from the saved profile is
    /// refused (changing a session's sandbox on resume is a safety footgun). A
    /// matching flag, or no flag, resumes with the saved profile.
    pub fn startup_sandbox_profile(&self, saved: Option<&str>) -> SandboxStartup {
        let explicit = self.sandbox.as_deref().filter(|s| !s.is_empty());
        Self::resolve_startup_sandbox(explicit, saved.map(String::from))
    }
    /// Pin an explicit non-UUID, non-chat resume/load target to its canonical
    /// local session id, before the (irreversible) OS sandbox is applied.
    ///
    /// Resolving once — recorded via `resume_target_pinned` so materialization
    /// never re-runs local title selection — makes the saved-profile peek and
    /// materialization consume the same immutable target; re-selecting after
    /// the sandbox would race a concurrent rename/create. Listing failures
    /// and ambiguity are hard errors here (fail closed / surfaced before the
    /// sandbox); a definitive no-match keeps the raw arg for the legacy
    /// remote/worktree id path.
    pub fn pin_local_resume_target(&mut self) -> anyhow::Result<()> {
        let cwd_buf = std::env::current_dir().ok();
        let cwd_str = cwd_buf.as_deref().map(|p| p.to_string_lossy());
        self.pin_local_resume_target_for_cwd(cwd_str.as_deref())
    }
    /// Same as [`Self::pin_local_resume_target`] with an explicit cwd, so
    /// tests never mutate the process cwd.
    pub fn pin_local_resume_target_for_cwd(&mut self, cwd: Option<&str>) -> anyhow::Result<()> {
        if self.chat() {
            return Ok(());
        }
        let Some(target) = self.session_to_resume().map(str::to_owned) else {
            return Ok(());
        };
        use crate::app::session_title_resolve::{PinnedResumeTarget, presandbox_resume_target};
        let pinned = presandbox_resume_target(&target, cwd)?;
        self.resume_target_pinned = true;
        if let PinnedResumeTarget::Title {
            ref id,
            ref sandbox_profile,
        } = pinned
        {
            eprintln!("Resuming session {} (matched by title)", id);
            self.pinned_resume_profile = Some(sandbox_profile.clone());
        }
        let Some(id) = pinned.id() else {
            return Ok(());
        };
        if self
            .resume_session
            .as_deref()
            .is_some_and(|s| !s.is_empty())
        {
            self.resume_session = Some(id);
        } else if self.load_session.as_deref().is_some_and(|s| !s.is_empty()) {
            self.load_session = Some(id);
        }
        Ok(())
    }
    /// The sandbox profile persisted with the session being resumed, if any.
    /// Local, best-effort; `None` when not resuming or nothing is found. Read once
    /// for the profile resume resolution.
    pub fn saved_resume_profile(&self) -> Option<String> {
        let cwd_buf = std::env::current_dir().ok();
        let cwd_str = cwd_buf.as_deref().map(|p| p.to_string_lossy());
        self.saved_resume_profile_for_cwd(cwd_str.as_deref())
    }
    /// Same as [`Self::saved_resume_profile`] with an explicit cwd, so tests
    /// never mutate the process cwd.
    pub fn saved_resume_profile_for_cwd(&self, cwd: Option<&str>) -> Option<String> {
        if let Some(pinned) = &self.pinned_resume_profile {
            return pinned.clone();
        }
        match self.resume_target() {
            ResumeTarget::SessionId(id) => {
                xai_grok_shell::session::persistence::resumed_session_sandbox_profile(
                    Some(&id),
                    cwd,
                )
            }
            ResumeTarget::MostRecentForCwd => {
                xai_grok_shell::session::persistence::resumed_session_sandbox_profile(None, cwd)
            }
            ResumeTarget::None => None,
        }
    }
    /// Pure resolution of the explicit flag against the resumed session's saved
    /// profile. Separated from disk access so it can be unit-tested.
    fn resolve_startup_sandbox(explicit: Option<&str>, saved: Option<String>) -> SandboxStartup {
        match (explicit, saved) {
            (Some(x), Some(s))
                if x.parse::<xai_grok_sandbox::ProfileName>().ok()
                    != s.parse::<xai_grok_sandbox::ProfileName>().ok() =>
            {
                SandboxStartup::Conflict {
                    requested: x.to_owned(),
                    saved: s,
                }
            }
            (Some(x), _) => SandboxStartup::Apply(Some(x.to_owned())),
            (None, saved) => SandboxStartup::Apply(saved),
        }
    }
    /// The initial interactive prompt from the positional argument, trimmed.
    ///
    /// Returns `None` when no positional prompt was given or it is only
    /// whitespace. This is the `grok "<prompt>"` launch form; the headless
    /// `-p`/`--single` path is handled separately.
    pub fn initial_prompt(&self) -> Option<&str> {
        self.prompt
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn assert_no_secret_fragments(output: &str, sentinel: &str) {
        assert!(!output.contains(sentinel));
        for window in sentinel.as_bytes().windows(8) {
            let fragment = std::str::from_utf8(window).expect("ASCII sentinel");
            assert!(
                !output.contains(fragment),
                "credential fragment {fragment:?} leaked in {output:?}"
            );
        }
    }

    #[test]
    fn credential_bearing_cli_debug_is_presence_only_through_all_wrappers() {
        const SENTINEL: &str = "GB002CLI-Q7w5E3r1T9y7Z6x4C2v8";
        let url = |scheme: &str, path: &str| {
            format!("{scheme}://{SENTINEL}:password@example.test/{path}?token={SENTINEL}")
        };
        let headless = HeadlessArgs {
            grok_ws_origin: Some(url("https", "origin")),
            grok_ws_url: Some(url("wss", "ws")),
        };
        let serve = ServeArgs {
            bind: "127.0.0.1:2419".parse().expect("valid socket address"),
            secret: Some(SENTINEL.to_owned()),
            allow_remote: true,
            remote: Some(url("https", "remote")),
            headless: headless.clone(),
        };
        let leader = LeaderArgs {
            no_exit_on_disconnect: true,
            relay_on_demand: true,
            no_auto_update: true,
            headless: headless.clone(),
        };
        let agent = AgentArgs {
            reauthenticate: true,
            model: Some("grok-safe".to_owned()),
            reasoning_effort: Some("high".to_owned()),
            yolo: true,
            agent_profile: Some(PathBuf::from("profile.toml")),
            plugin_dirs: vec![PathBuf::from("plugin")],
            leader: true,
            no_leader: false,
            headless: headless.clone(),
            cli_chat_proxy_base_url: Some(url("https", "chat-proxy")),
            xai_api_base_url: Some(url("https", "api")),
            mode: Some(AgentCmd::Serve(serve.clone())),
        };
        let workspace = WorkspaceStartArgs {
            hub_url: Some(url("wss", "hub")),
            cwd: Some(PathBuf::from("/tmp/workspace")),
            leader: true,
            no_leader: false,
            json: true,
        };

        let outputs = [
            format!("{headless:?}"),
            format!("{serve:?}"),
            format!("{leader:?}"),
            format!("{:?}", AgentCmd::Headless(headless.clone())),
            format!("{:?}", AgentCmd::Serve(serve.clone())),
            format!("{:?}", AgentCmd::Leader(leader.clone())),
            format!("{agent:?}"),
            format!("{:?}", Command::Agent(Box::new(agent))),
            format!("{workspace:?}"),
            format!("{:?}", WorkspaceMgmtCommand::Start(workspace.clone())),
            format!("{:?}", WorkspaceMgmtCommand::Restart(workspace.clone())),
            format!(
                "{:?}",
                WorkspaceMgmtArgs {
                    command: WorkspaceMgmtCommand::Start(workspace.clone()),
                }
            ),
            format!(
                "{:?}",
                Command::Workspace(WorkspaceMgmtArgs {
                    command: WorkspaceMgmtCommand::Start(workspace),
                })
            ),
        ];
        for output in outputs {
            assert_no_secret_fragments(&output, SENTINEL);
        }
        let serve_output = format!("{serve:?}");
        assert!(serve_output.contains("secret_present: true"));
        assert!(serve_output.contains("allow_remote: true"));
        assert!(serve_output.contains("remote_present: true"));
        assert!(serve_output.contains("grok_ws_origin_present: true"));
        assert!(serve_output.contains("grok_ws_url_present: true"));
    }

    #[test]
    fn issue8_agent_serve_security_allow_remote_is_explicit_and_defaults_closed() {
        let default = PagerArgs::try_parse_from(["grok", "agent", "serve"]).unwrap();
        let Some(Command::Agent(agent)) = default.command else {
            panic!("expected agent command");
        };
        let Some(AgentCmd::Serve(serve)) = agent.mode else {
            panic!("expected serve mode");
        };
        assert!(!serve.allow_remote);

        let enabled =
            PagerArgs::try_parse_from(["grok", "agent", "serve", "--allow-remote"]).unwrap();
        let Some(Command::Agent(agent)) = enabled.command else {
            panic!("expected agent command");
        };
        let Some(AgentCmd::Serve(serve)) = agent.mode else {
            panic!("expected serve mode");
        };
        assert!(serve.allow_remote);
    }

    #[test]
    fn issue8_agent_serve_security_generated_secret_is_256_bit_url_safe_without_padding() {
        let secret = ServeArgs::generate_secret().expect("OS RNG should be available");
        assert_eq!(secret.len(), 43);
        assert!(!secret.contains('='));
        assert!(
            secret
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        );
    }

    #[test]
    fn issue8_agent_serve_security_secret_encoder_uses_all_32_bytes() {
        let baseline = encode_secret(&[0_u8; 32]);
        assert_eq!(baseline.len(), 43);
        for index in 0..32 {
            let mut bytes = [0_u8; 32];
            bytes[index] = 1;
            assert_ne!(encode_secret(&bytes), baseline, "byte {index} was ignored");
        }
    }

    #[test]
    #[serial_test::serial(GROK_AGENT_SECRET)]
    fn issue8_agent_serve_security_help_hides_environment_secret_value() {
        use clap::CommandFactory as _;

        const SENTINEL: &str = "issue8-help-secret-Q7w5E3r1T9y7Z6x4";
        let _guard = crate::test_util::EnvVarGuard::set("GROK_AGENT_SECRET", SENTINEL);
        let mut command = PagerArgs::command();
        let serve = command
            .find_subcommand_mut("agent")
            .and_then(|agent| agent.find_subcommand_mut("serve"))
            .expect("agent serve subcommand");
        let help = serve.render_long_help().to_string();

        assert!(help.contains("GROK_AGENT_SECRET"));
        assert_no_secret_fragments(&help, SENTINEL);
    }

    #[test]
    fn version_flags_parse_as_early_intent_without_exiting() {
        for flag in ["--version", "-v", "-V"] {
            let args = PagerArgs::try_parse_from(["grok", flag]).expect("version flag parses");
            assert!(args.version, "{flag} must set the early version intent");
            assert!(args.command.is_none());
        }
    }
    #[test]
    fn ordinary_and_doctor_parsing_do_not_set_version_intent() {
        assert!(!PagerArgs::try_parse_from(["grok"]).unwrap().version);
        assert!(
            !PagerArgs::try_parse_from(["grok", "doctor"])
                .unwrap()
                .version
        );
        assert!(matches!(
            PagerArgs::try_parse_from(["grok", "version"])
                .unwrap()
                .command,
            Some(Command::Version { json: false })
        ));
    }
    #[test]
    fn doctor_accepts_report_and_explicit_fix_forms() {
        let bare = PagerArgs::try_parse_from(["grok", "doctor"]).expect("bare doctor parses");
        assert!(matches!(
            bare.command,
            Some(Command::Doctor(crate::doctor_cmd::DoctorArgs {
                json: false,
                command: None,
            }))
        ));
        let json =
            PagerArgs::try_parse_from(["grok", "doctor", "--json"]).expect("doctor --json parses");
        assert!(matches!(
            json.command,
            Some(Command::Doctor(crate::doctor_cmd::DoctorArgs {
                json: true,
                command: None,
            }))
        ));
        for id in [
            "terminal.ssh-wrap",
            "tmux-clipboard",
            "terminal.dcs-passthrough",
            "tmux-extended-keys",
        ] {
            let fix = PagerArgs::try_parse_from(["grok", "doctor", "fix", id, "--yes"])
                .expect("doctor fix parses");
            assert!(matches!(
                fix.command,
                Some(Command::Doctor(crate::doctor_cmd::DoctorArgs {
                    json: false,
                    command: Some(crate::doctor_cmd::DoctorCommand::Fix(
                        crate::doctor_cmd::FixArgs { id: Some(ref parsed), yes: true }
                    )),
                })) if parsed == id
            ));
        }
        let list = PagerArgs::try_parse_from(["grok", "doctor", "fix"])
            .expect("doctor fix without an ID lists applicable fixes");
        assert!(matches!(
            list.command,
            Some(Command::Doctor(crate::doctor_cmd::DoctorArgs {
                json: false,
                command: Some(crate::doctor_cmd::DoctorCommand::Fix(
                    crate::doctor_cmd::FixArgs {
                        id: None,
                        yes: false
                    }
                )),
            }))
        ));
        for unsupported in [
            vec!["grok", "doctor", "all"],
            vec!["grok", "doctor", "fix", "ssh-wrap", "extra"],
            vec!["grok", "doctor", "fix", "--yes"],
            vec!["grok", "doctor", "--json", "fix", "terminal.ssh-wrap"],
        ] {
            let error = PagerArgs::try_parse_from(unsupported)
                .expect_err("unsupported doctor form must fail");
            assert_eq!(error.exit_code(), 2);
        }
    }
    #[test]
    fn resume_target_classifies_flags() {
        assert_eq!(
            PagerArgs::try_parse_from(["grok"]).unwrap().resume_target(),
            ResumeTarget::None
        );
        assert_eq!(
            PagerArgs::try_parse_from(["grok", "-c"])
                .unwrap()
                .resume_target(),
            ResumeTarget::MostRecentForCwd
        );
        assert_eq!(
            PagerArgs::try_parse_from(["grok", "--resume"])
                .unwrap()
                .resume_target(),
            ResumeTarget::MostRecentForCwd
        );
        assert_eq!(
            PagerArgs::try_parse_from(["grok", "--resume", "sess-1"])
                .unwrap()
                .resume_target(),
            ResumeTarget::SessionId("sess-1".to_string())
        );
        assert_eq!(
            PagerArgs::try_parse_from(["grok", "-s", "sess-2"])
                .unwrap()
                .resume_target(),
            ResumeTarget::None
        );
        assert_eq!(
            PagerArgs::try_parse_from(["grok", "-r", "old", "--fork-session"])
                .unwrap()
                .resume_target(),
            ResumeTarget::SessionId("old".to_string())
        );
    }
    /// The screen-mode flags are mutually exclusive: the pair exists so one
    /// can override the other's sticky config value, so accepting both in one
    /// invocation would be ambiguous.
    #[test]
    fn minimal_and_fullscreen_flags_conflict() {
        let args = PagerArgs::try_parse_from(["grok", "--minimal"]).unwrap();
        assert!(args.minimal && !args.fullscreen);
        let args = PagerArgs::try_parse_from(["grok", "--fullscreen"]).unwrap();
        assert!(args.fullscreen && !args.minimal);
        let err = PagerArgs::try_parse_from(["grok", "--minimal", "--fullscreen"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }
    #[test]
    fn agent_plugin_dir_repeatable_and_canonicalized() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plugin");
        std::fs::create_dir(&dir).unwrap();
        let file = tmp.path().join("file.txt");
        std::fs::write(&file, "x").unwrap();
        let missing = tmp.path().join("missing");
        let args = PagerArgs::try_parse_from([
            "grok".as_ref(),
            "agent".as_ref(),
            "--no-leader".as_ref(),
            "--plugin-dir".as_ref(),
            dir.as_os_str(),
            "--plugin-dir".as_ref(),
            file.as_os_str(),
            "--plugin-dir".as_ref(),
            missing.as_os_str(),
            "stdio".as_ref(),
        ])
        .unwrap();
        let Some(Command::Agent(agent)) = args.command else {
            panic!("expected agent subcommand");
        };
        assert_eq!(agent.plugin_dirs, vec![dir.clone(), file, missing]);
        assert!(matches!(agent.mode, Some(AgentCmd::Stdio)));
        assert!(agent.no_leader);
        assert_eq!(
            agent.canonical_plugin_dirs(),
            vec![dunce::canonicalize(&dir).unwrap()]
        );
    }
    #[test]
    fn resolve_startup_sandbox_cases() {
        use SandboxStartup::{Apply, Conflict};
        assert_eq!(
            PagerArgs::resolve_startup_sandbox(Some("strict"), None),
            Apply(Some("strict".to_string()))
        );
        assert_eq!(
            PagerArgs::resolve_startup_sandbox(Some("workspace"), Some("workspace".to_string())),
            Apply(Some("workspace".to_string()))
        );
        assert_eq!(
            PagerArgs::resolve_startup_sandbox(Some("read-only"), Some("workspace".to_string())),
            Conflict {
                requested: "read-only".to_string(),
                saved: "workspace".to_string(),
            }
        );
        assert_eq!(
            PagerArgs::resolve_startup_sandbox(None, Some("workspace".to_string())),
            Apply(Some("workspace".to_string()))
        );
        assert_eq!(PagerArgs::resolve_startup_sandbox(None, None), Apply(None));
        assert_eq!(
            PagerArgs::resolve_startup_sandbox(Some("readonly"), Some("read-only".to_string())),
            Apply(Some("readonly".to_string()))
        );
        assert_eq!(
            PagerArgs::resolve_startup_sandbox(Some("none"), Some("off".to_string())),
            Apply(Some("none".to_string()))
        );
    }
    #[test]
    fn startup_sandbox_profile_no_resume() {
        assert_eq!(
            PagerArgs::try_parse_from(["grok", "--sandbox", "strict"])
                .unwrap()
                .startup_sandbox_profile(None),
            SandboxStartup::Apply(Some("strict".to_string()))
        );
        assert_eq!(
            PagerArgs::try_parse_from(["grok", "--sandbox", ""])
                .unwrap()
                .startup_sandbox_profile(None),
            SandboxStartup::Apply(None)
        );
        assert_eq!(
            PagerArgs::try_parse_from(["grok"])
                .unwrap()
                .startup_sandbox_profile(None),
            SandboxStartup::Apply(None)
        );
    }
    #[test]
    fn launch_directory_anchoring_precedes_cwd_change() {
        let args = PagerArgs::try_parse_from([
            "grok",
            "--leader-socket",
            "relative.sock",
            "--debug-file",
            "relative.log",
        ])
        .unwrap()
        .apply_cwd_from(Some(std::path::Path::new("/launch")))
        .unwrap();
        assert_eq!(
            args.leader_socket.as_deref(),
            Some(std::path::Path::new("/launch/relative.sock"))
        );
        assert_eq!(
            args.debug_file.as_deref(),
            Some(std::path::Path::new("/launch/relative.log"))
        );
    }
    #[test]
    fn launch_directory_anchoring_normalizes_dot_components() {
        for (input, expected) in [
            ("./leader.sock", "/launch/leader.sock"),
            ("logs/../debug.log", "/launch/logs/../debug.log"),
            ("../leader.sock", "/launch/../leader.sock"),
        ] {
            assert_eq!(
                anchor_to_launch_dir(PathBuf::from(input), Some(std::path::Path::new("/launch"))),
                PathBuf::from(expected),
                "input: {input}"
            );
        }
    }
    #[test]
    fn leader_socket_flag_parses_at_root() {
        let args = PagerArgs::try_parse_from(["grok", "--leader-socket", "/tmp/leader-x.sock"])
            .expect("--leader-socket parses at the root");
        assert_eq!(
            args.leader_socket.as_deref(),
            Some(std::path::Path::new("/tmp/leader-x.sock"))
        );
    }
    #[test]
    fn leader_socket_flag_is_global_for_subcommands() {
        let args = PagerArgs::try_parse_from([
            "grok",
            "agent",
            "leader",
            "--leader-socket",
            "/tmp/leader-y.sock",
        ])
        .expect("--leader-socket parses after a subcommand (global)");
        assert_eq!(
            args.leader_socket.as_deref(),
            Some(std::path::Path::new("/tmp/leader-y.sock"))
        );
    }
    #[test]
    fn leader_socket_flag_defaults_to_none() {
        let args = PagerArgs::try_parse_from(["grok"]).expect("bare grok parses");
        assert!(args.leader_socket.is_none());
    }
    #[test]
    fn leader_mgmt_list_info_kill_parse() {
        let list = PagerArgs::try_parse_from(["grok", "leader", "list", "--json"])
            .expect("grok leader list --json");
        assert!(matches!(
            list.command,
            Some(Command::Leader(LeaderMgmtArgs {
                command: LeaderMgmtCommand::List { json: true },
            }))
        ));
        let info = PagerArgs::try_parse_from(["grok", "leader", "info", "--pid", "42"])
            .expect("grok leader info --pid");
        assert!(matches!(
            info.command,
            Some(Command::Leader(LeaderMgmtArgs {
                command: LeaderMgmtCommand::Info {
                    target: LeaderTargetArgs { pid: Some(42) },
                    json: false,
                },
            }))
        ));
        let kill = PagerArgs::try_parse_from(["grok", "leader", "kill"]).expect("grok leader kill");
        assert!(matches!(
            kill.command,
            Some(Command::Leader(LeaderMgmtArgs {
                command: LeaderMgmtCommand::Kill,
            }))
        ));
        assert!(PagerArgs::try_parse_from(["grok", "leader", "profile"]).is_err());
    }
    #[test]
    fn debug_file_flag_parses_and_is_global() {
        let root = PagerArgs::try_parse_from(["grok", "--debug-file", "/tmp/fire.txt"])
            .expect("--debug-file parses at the root");
        assert_eq!(
            root.debug_file.as_deref(),
            Some(std::path::Path::new("/tmp/fire.txt"))
        );
        let sub =
            PagerArgs::try_parse_from(["grok", "agent", "stdio", "--debug-file", "/tmp/f.txt"])
                .expect("--debug-file parses after a subcommand (global)");
        assert_eq!(
            sub.debug_file.as_deref(),
            Some(std::path::Path::new("/tmp/f.txt"))
        );
    }
    #[test]
    fn debug_file_flag_defaults_to_none() {
        let args = PagerArgs::try_parse_from(["grok"]).expect("bare grok parses");
        assert!(args.debug_file.is_none());
    }
    #[test]
    fn positional_prompt_seeds_interactive_session() {
        let args =
            PagerArgs::try_parse_from(["grok", "fix the bug"]).expect("positional prompt parses");
        assert_eq!(args.initial_prompt(), Some("fix the bug"));
        assert!(args.command.is_none());
        assert!(args.single.is_none());
    }
    #[test]
    fn bare_grok_has_no_initial_prompt() {
        let args = PagerArgs::try_parse_from(["grok"]).expect("bare grok parses");
        assert_eq!(args.initial_prompt(), None);
    }
    #[test]
    fn initial_prompt_trims_and_ignores_whitespace_only() {
        let args = PagerArgs::try_parse_from(["grok", "  spaced  "]).expect("padded prompt parses");
        assert_eq!(args.initial_prompt(), Some("spaced"));
        let blank = PagerArgs::try_parse_from(["grok", "   "]).expect("blank prompt parses");
        assert_eq!(blank.initial_prompt(), None);
    }
    #[test]
    fn subcommand_takes_precedence_over_positional_prompt() {
        let args = PagerArgs::try_parse_from(["grok", "logout"]).expect("subcommand parses");
        assert!(matches!(
            args.command,
            Some(Command::Logout {
                provider: LoginProvider::Xai
            })
        ));
        assert!(args.prompt.is_none());
    }

    #[test]
    fn provider_scoped_auth_commands_parse_without_changing_xai_defaults() {
        let login = PagerArgs::try_parse_from(["grok", "login"]).expect("default login parses");
        assert!(matches!(
            login.command,
            Some(Command::Login {
                provider: LoginProvider::Xai,
                ..
            })
        ));

        let codex_login =
            PagerArgs::try_parse_from(["grok", "login", "--provider", "openai-codex"])
                .expect("Codex login parses");
        assert!(matches!(
            codex_login.command,
            Some(Command::Login {
                provider: LoginProvider::OpenaiCodex,
                oauth: false,
                device_auth: false,
                devbox: false,
                ..
            })
        ));

        let codex_with_xai_flag =
            PagerArgs::try_parse_from(["grok", "login", "--provider", "openai-codex", "--oauth"])
                .expect("clap preserves the backwards-compatible flag surface");
        let Some(Command::Login {
            provider,
            legacy,
            oauth,
            device_auth,
            devbox,
        }) = codex_with_xai_flag.command
        else {
            panic!("expected login command")
        };
        assert!(
            provider
                .validate_login_flags(legacy, oauth, device_auth, devbox)
                .is_err(),
            "Codex selection rejects xAI-only login flags"
        );

        let status = PagerArgs::try_parse_from([
            "grok",
            "auth",
            "status",
            "--provider",
            "openai-codex",
            "--json",
        ])
        .expect("Codex status parses");
        assert!(matches!(
            status.command,
            Some(Command::Auth(AuthArgs {
                command: AuthCommand::Status {
                    provider: LoginProvider::OpenaiCodex,
                    json: true,
                }
            }))
        ));

        let logout = PagerArgs::try_parse_from(["grok", "logout", "--provider", "openai-codex"])
            .expect("Codex logout parses");
        assert!(matches!(
            logout.command,
            Some(Command::Logout {
                provider: LoginProvider::OpenaiCodex
            })
        ));
    }
    #[test]
    fn positional_prompt_conflicts_with_headless_single() {
        let err = PagerArgs::try_parse_from(["grok", "-p", "headless", "interactive"])
            .expect_err("positional prompt + --single must conflict");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }
    #[test]
    fn worktree_flag_and_initial_prompt_combine() {
        let a = PagerArgs::try_parse_from(["grok", "do the thing", "-w"])
            .expect("prompt then bare -w parses");
        assert_eq!(a.initial_prompt(), Some("do the thing"));
        assert_eq!(a.worktree.as_deref(), Some(""));
        let b = PagerArgs::try_parse_from(["grok", "--worktree=feat", "do the thing"])
            .expect("--worktree=name + positional parses");
        assert_eq!(b.initial_prompt(), Some("do the thing"));
        assert_eq!(b.worktree.as_deref(), Some("feat"));
        let c = PagerArgs::try_parse_from(["grok", "-w", "x"]).expect("-w x parses");
        assert_eq!(c.worktree.as_deref(), Some("x"));
        assert_eq!(c.initial_prompt(), None);
    }
    #[test]
    fn trust_flag_parses_on_pager_and_alias() {
        let bare = PagerArgs::try_parse_from(["grok"]).expect("bare grok parses");
        assert!(!bare.trust);
        let long = PagerArgs::try_parse_from(["grok", "--trust"]).expect("--trust parses");
        assert!(long.trust);
        let alias =
            PagerArgs::try_parse_from(["grok", "--trust-folder"]).expect("--trust-folder parses");
        assert!(alias.trust);
    }
    #[test]
    fn reasoning_effort_and_effort_alias_parse_same_field() {
        let long = PagerArgs::try_parse_from(["grok", "--reasoning-effort", "high"])
            .expect("--reasoning-effort parses");
        assert_eq!(long.reasoning_effort.as_deref(), Some("high"));
        let alias =
            PagerArgs::try_parse_from(["grok", "--effort", "high"]).expect("--effort alias parses");
        assert_eq!(alias.reasoning_effort.as_deref(), Some("high"));
    }
    #[test]
    fn reasoning_effort_accepts_max_and_remapped_ids() {
        let max = PagerArgs::try_parse_from(["grok", "--effort", "max"]).expect("max parses");
        assert_eq!(max.reasoning_effort.as_deref(), Some("max"));
        let deep =
            PagerArgs::try_parse_from(["grok", "--reasoning-effort", "deep"]).expect("deep parses");
        assert_eq!(deep.reasoning_effort.as_deref(), Some("deep"));
    }
    #[test]
    fn reasoning_effort_last_flag_wins_when_both_names_set() {
        let args =
            PagerArgs::try_parse_from(["grok", "--reasoning-effort", "low", "--effort", "high"])
                .expect("both effort flag names parse");
        assert_eq!(args.reasoning_effort.as_deref(), Some("high"));
        let reverse =
            PagerArgs::try_parse_from(["grok", "--effort", "high", "--reasoning-effort", "low"])
                .expect("both effort flag names parse (reverse order)");
        assert_eq!(reverse.reasoning_effort.as_deref(), Some("low"));
    }
    #[test]
    fn agent_args_effort_alias_parses() {
        let args = PagerArgs::try_parse_from(["grok", "agent", "--effort", "max", "stdio"])
            .expect("agent --effort parses");
        let Command::Agent(agent) = args.command.expect("agent subcommand") else {
            panic!("expected agent subcommand");
        };
        assert_eq!(agent.reasoning_effort.as_deref(), Some("max"));
    }
}

#[cfg(test)]
mod state_dir_help_tests {
    /// A `///` comment on a clap field or variant *becomes the help text*, and
    /// cannot interpolate — so a path written in one is frozen at whatever it
    /// said when it was typed. That is how `~/.grok` survived the move to
    /// `~/.medley` in seven places, telling users to look somewhere the
    /// program does not write.
    ///
    /// Checked against the source rather than the rendered help, because
    /// rendering resolves the *developer's* state directory: on a machine that
    /// still has `~/.grok` and no `~/.medley`, correct output contains
    /// `~/.grok` and the test would fail for being right.
    ///
    /// Doc comments on `fn` items are exempt: those are developer
    /// documentation, and some of them name the old path precisely because
    /// they are explaining this.
    #[test]
    fn no_clap_doc_comment_hardcodes_the_state_directory() {
        for (file, src) in [
            ("app/cli.rs", include_str!("cli.rs")),
            ("mcp_cmd.rs", include_str!("../mcp_cmd.rs")),
        ] {
            let lines: Vec<&str> = src.lines().collect();
            for (idx, line) in lines.iter().enumerate() {
                let trimmed = line.trim_start();
                if !trimmed.starts_with("///") || !trimmed.contains("~/.grok") {
                    continue;
                }
                if documents_a_function(&lines, idx) {
                    continue;
                }
                panic!(
                    "{file}:{} is a clap doc comment naming ~/.grok, which becomes \
                     help text verbatim. Move it to #[arg(long_help = ...)] or \
                     #[command(long_about = ...)] and resolve the path with \
                     crate::util::display_user_grok_path.\n  {}",
                    idx + 1,
                    trimmed
                );
            }
        }
    }

    /// Whether the doc block containing `idx` is attached to a `fn`.
    fn documents_a_function(lines: &[&str], idx: usize) -> bool {
        for line in lines.iter().skip(idx + 1) {
            let trimmed = line.trim_start();
            if trimmed.starts_with("///") || trimmed.starts_with("#[") || trimmed.is_empty() {
                continue;
            }
            return trimmed.starts_with("fn ") || trimmed.starts_with("pub fn ");
        }
        false
    }
}

#[cfg(test)]
mod auth_instruction_guard_tests {
    /// Scans this crate's own `src/` — a guard in one crate cannot see another's.
    #[test]
    fn no_hardcoded_auth_instructions() {
        xai_grok_config::auth_instruction_guard::assert_no_hardcoded_auth_instructions(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src"
        ));
    }
}
