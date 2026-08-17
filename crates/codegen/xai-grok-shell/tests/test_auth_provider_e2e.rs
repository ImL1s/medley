//! End-to-end test for `[auth_provider.<name>]` per-model credential helpers.
//!
//! Runs the built grok binary headless against the mock inference server with
//! a config-defined BYOK model whose bearer comes from a mock auth binary (a
//! script the test writes to disk). The harness's `XAI_API_KEY` stands in for
//! the session-tier credential.
//!
//! `#[ignore]` (needs a built binary). CI builds the production entry point
//! and supplies it through `GROK_BINARY`; run locally (auto-builds the pager):
//! ```bash
//! cargo test -p xai-grok-shell --test test_auth_provider_e2e -- --ignored
//! ```
//!
//! Unix-only: the mock helpers are `sh` scripts run via `sh -c`.
#![cfg(unix)]

use xai_grok_test_support::*;

const UNPUBLISHED_SESSION_MARKER: &str = ".unpublished";

fn published_session_dirs(grok_home: &std::path::Path) -> Vec<std::path::PathBuf> {
    let sessions_root = grok_home.join("sessions");
    let mut sessions = Vec::new();
    for cwd_dir in std::fs::read_dir(&sessions_root)
        .unwrap_or_else(|e| panic!("read sessions root {}: {e}", sessions_root.display()))
        .flatten()
    {
        if !cwd_dir.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        for session_dir in std::fs::read_dir(cwd_dir.path())
            .unwrap_or_else(|e| panic!("read cwd session dir {}: {e}", cwd_dir.path().display()))
            .flatten()
        {
            if session_dir.file_type().is_ok_and(|kind| kind.is_dir()) {
                sessions.push(session_dir.path());
            }
        }
    }
    sessions
}

/// Regression for fresh-session publication through the production binary:
/// creating a Codex-harness session, resolving its external provider token,
/// and completing the first Responses request must leave one complete public
/// tree rather than exposing a partial or marker-bearing session directory.
#[tokio::test]
#[ignore] // requires pre-built binary
async fn codex_provider_prompt_publishes_one_complete_session_tree() {
    const FIXTURE_TOKEN: &str = "fixture-token";
    const PROMPT: &str = "publication-auth-fixture-prompt";

    let server = MockInferenceServer::start_with_required_auth(
        vec![
            MockModelEntry::with_agent_type("mock-codex-model", "codex")
                .with_api_backend("responses"),
        ],
        FIXTURE_TOKEN,
    )
    .await
    .expect("start authenticated mock server");
    let mut sandbox = TestSandbox::builder().git().mock_url(server.url()).build();
    sandbox.remove_env("GROK_LEADER_SOCKET");

    let grok_home = sandbox.grok_home().to_path_buf();
    let helper = grok_home.join("fixture-auth.sh");
    std::fs::write(
        &helper,
        format!("#!/bin/sh\nprintf '%s' '{FIXTURE_TOKEN}'\n"),
    )
    .expect("write fixture auth provider");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755))
            .expect("make fixture auth provider executable");
    }

    std::fs::write(
        grok_home.join("config.toml"),
        format!(
            r#"[auth_provider.fixture]
command = "{helper}"
token_ttl_secs = 3600

[model.fixture-codex]
model = "mock-codex-model"
base_url = "{base}"
context_window = 200000
agent_type = "codex"
api_backend = "responses"
auth_provider = "fixture"

[models]
default = "fixture-codex"
"#,
            helper = helper.display(),
            base = server.url(),
        ),
    )
    .expect("write fixture model config");

    let mut cmd = tokio::process::Command::new(grok_binary());
    cmd.args([
        "-p",
        PROMPT,
        "--yolo",
        "--model",
        "fixture-codex",
        "--max-turns",
        "1",
        "--output-format",
        "json",
    ])
    .arg("--cwd")
    .arg(sandbox.workspace())
    .current_dir(sandbox.workspace())
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .kill_on_drop(true);

    let result = run_headless_in_sandbox_borrowed(cmd, &sandbox).await;
    assert_headless_success(
        &result,
        "Codex provider prompt publication e2e",
        Some(&server),
    );
    assert_no_crashes(&result.stderr);

    let responses: Vec<_> = server
        .requests()
        .into_iter()
        .filter(|request| request.method == "POST" && request.path == "/v1/responses")
        .collect();
    assert!(
        !responses.is_empty(),
        "expected at least one Responses sampling request:\n{}",
        server.request_log_summary()
    );
    let prompt_requests: Vec<_> = responses
        .iter()
        .filter(|request| {
            request
                .body
                .as_ref()
                .is_some_and(|body| body.to_string().contains(PROMPT))
        })
        .collect();
    assert!(
        !prompt_requests.is_empty(),
        "a Responses request must carry the fixture prompt"
    );
    assert!(
        prompt_requests
            .iter()
            .any(|request| request.authorization.as_deref() == Some("Bearer fixture-token")),
        "at least one prompt-bearing Responses request must use the fixture provider token; requests: {:?}",
        responses
            .iter()
            .map(|request| (
                request.header("x-grok-turn-idx"),
                request.header("x-grok-req-id"),
                request.authorization.as_deref(),
                request
                    .body
                    .as_ref()
                    .is_some_and(|body| body.to_string().contains(PROMPT)),
            ))
            .collect::<Vec<_>>()
    );

    let session_dirs = published_session_dirs(&grok_home);
    assert_eq!(
        session_dirs.len(),
        1,
        "expected one published session tree under {}; got {session_dirs:?}",
        grok_home.join("sessions").display()
    );
    let session_dir = &session_dirs[0];
    assert!(
        !session_dir.join(UNPUBLISHED_SESSION_MARKER).exists(),
        "published session must not retain {UNPUBLISHED_SESSION_MARKER}: {}",
        session_dir.display()
    );

    let summary: serde_json::Value = serde_json::from_slice(
        &std::fs::read(session_dir.join("summary.json")).expect("read summary.json"),
    )
    .expect("summary.json must contain valid JSON");
    assert!(summary.is_object(), "summary.json must be a JSON object");

    let prompt_context: serde_json::Value = serde_json::from_slice(
        &std::fs::read(session_dir.join("prompt_context.json")).expect("read prompt_context.json"),
    )
    .expect("prompt_context.json must contain valid JSON");
    assert!(
        prompt_context.is_object(),
        "prompt_context.json must be a JSON object"
    );

    let system_prompt = std::fs::read_to_string(session_dir.join("system_prompt.txt"))
        .expect("read system_prompt.txt");
    assert!(
        !system_prompt.trim().is_empty(),
        "system_prompt.txt must not be empty"
    );

    let history = std::fs::read_to_string(session_dir.join("chat_history.jsonl"))
        .expect("read chat_history.jsonl");
    assert!(
        history.contains(PROMPT),
        "chat history must contain the submitted prompt"
    );
    let history_entries: Vec<serde_json::Value> = history
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("chat history line must be valid JSON"))
        .collect();
    assert!(
        !history_entries.is_empty(),
        "chat_history.jsonl must contain at least one JSON entry"
    );
}

#[tokio::test]
#[ignore]
async fn provider_backed_model_sends_minted_token_on_the_wire() {
    let server = MockInferenceServer::start()
        .await
        .expect("start mock server");
    let mut sandbox = TestSandbox::builder().git().mock_url(server.url()).build();
    // The baseline already omits the leader socket; keep the test's explicit
    // fresh-process intent at the typed sandbox layer that survives env_clear().
    sandbox.remove_env("GROK_LEADER_SOCKET");

    let grok_home = sandbox.grok_home().to_path_buf();
    std::fs::create_dir_all(&grok_home).expect("create .grok home");

    let counter = grok_home.join("mint-count");
    let helper = grok_home.join("mock-auth.sh");
    std::fs::write(
        &helper,
        format!(
            "#!/bin/sh\necho run >> {}\nprintf 'gateway-tok-1'\n",
            counter.display()
        ),
    )
    .expect("write mock auth binary");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    std::fs::write(
        grok_home.join("config.toml"),
        format!(
            r#"[auth_provider.gateway]
command = "{helper}"
token_ttl_secs = 3600

[model.proxied-gateway]
model = "mock-gateway-model"
base_url = "{base}"
context_window = 200000
auth_provider = "gateway"
"#,
            helper = helper.display(),
            // Already ends in `/v1`.
            base = server.url(),
        ),
    )
    .expect("write config.toml");

    let mut cmd = tokio::process::Command::new(grok_binary());
    cmd.args([
        "-p",
        "say hi",
        "--yolo",
        "--model",
        "proxied-gateway",
        "--max-turns",
        "1",
        "--output-format",
        "json",
    ])
    .arg("--cwd")
    .arg(sandbox.workspace())
    .current_dir(sandbox.workspace())
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .kill_on_drop(true);

    let result = run_headless_in_sandbox_borrowed(cmd, &sandbox).await;
    assert_headless_success(&result, "auth provider e2e", Some(&server));

    let runs = std::fs::read_to_string(&counter)
        .expect("helper must have run")
        .lines()
        .count();
    assert_eq!(runs, 1, "one turn mints exactly once");

    let requests = server.requests();
    // The mock server plays both the first-party and the provider role on
    // one host, so the leak assertions are scoped to the inference path.
    let chat = requests
        .iter()
        .find(|e| e.method == "POST" && e.path.contains("chat/completions"))
        .unwrap_or_else(|| {
            panic!(
                "no POST /v1/chat/completions request logged; requests:\n{}",
                server.request_log_summary()
            )
        });
    assert_eq!(
        chat.authorization.as_deref(),
        Some("Bearer gateway-tok-1"),
        "the inference request must carry the minted provider token; requests:\n{}",
        server.request_log_summary()
    );
    // `test-key-for-ci` is the harness's XAI_API_KEY; it stands in for the
    // session credential, which resolves below the provider arm.
    assert!(
        !requests.iter().any(|e| {
            e.path.contains("chat/completions")
                && e.authorization
                    .as_deref()
                    .is_some_and(|a| a.contains("test-key-for-ci"))
        }),
        "the session credential must never reach the provider-backed endpoint; requests:\n{}",
        server.request_log_summary()
    );
}

/// A model that references an undefined `[auth_provider.<name>]` must fail
/// closed end to end: the request goes out without a bearer (which the mock
/// rejects), and the session credential is never substituted onto the wire.
#[tokio::test]
#[ignore]
async fn undefined_provider_fails_closed_and_never_leaks_session_key() {
    // Reject any bearer: nothing legitimate can satisfy this, since the model
    // references a provider that is never defined.
    let server = MockInferenceServer::start_with_required_auth(
        vec![MockModelEntry::new("mock-gateway-model")],
        "never-issued-token",
    )
    .await
    .expect("start mock server");
    let mut sandbox = TestSandbox::builder().git().mock_url(server.url()).build();
    // The baseline already omits the leader socket; keep the test's explicit
    // fresh-process intent at the typed sandbox layer that survives env_clear().
    sandbox.remove_env("GROK_LEADER_SOCKET");

    let grok_home = sandbox.grok_home().to_path_buf();
    std::fs::create_dir_all(&grok_home).expect("create .grok home");

    // Model references `gateway`, but no `[auth_provider.gateway]` table exists.
    std::fs::write(
        grok_home.join("config.toml"),
        format!(
            r#"[model.proxied-gateway]
model = "mock-gateway-model"
base_url = "{base}"
context_window = 200000
auth_provider = "gateway"
"#,
            base = server.url(),
        ),
    )
    .expect("write config.toml");

    let mut cmd = tokio::process::Command::new(grok_binary());
    cmd.args([
        "-p",
        "say hi",
        "--yolo",
        "--model",
        "proxied-gateway",
        "--max-turns",
        "1",
        "--output-format",
        "json",
    ])
    .arg("--cwd")
    .arg(sandbox.workspace())
    .current_dir(sandbox.workspace())
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .kill_on_drop(true);

    // The turn is expected to fail (the mock 401s the unauthenticated request);
    // we assert on the wire, not the exit code.
    let _ = run_headless_in_sandbox(cmd, sandbox).await;

    let requests = server.requests();
    // Non-vacuity: the model was actually exercised.
    assert!(
        requests
            .iter()
            .any(|e| e.method == "POST" && e.path.contains("chat/completions")),
        "no chat request attempted; requests:\n{}",
        server.request_log_summary()
    );
    assert!(
        !requests.iter().any(|e| {
            e.path.contains("chat/completions")
                && e.authorization
                    .as_deref()
                    .is_some_and(|a| a.contains("test-key-for-ci"))
        }),
        "an undefined provider must fail closed, never sending the session \
         credential; requests:\n{}",
        server.request_log_summary()
    );
}

/// The documented `args` + JSON-output shape, end to end: a provider with
/// `args = [...]` runs the helper directly (no shell) and parses a JSON
/// `{access_token, expires_in}` payload, and the minted token reaches the wire.
#[tokio::test]
#[ignore]
async fn provider_with_args_and_json_output_sends_minted_token() {
    let server = MockInferenceServer::start()
        .await
        .expect("start mock server");
    let mut sandbox = TestSandbox::builder().git().mock_url(server.url()).build();
    // The baseline already omits the leader socket; keep the test's explicit
    // fresh-process intent at the typed sandbox layer that survives env_clear().
    sandbox.remove_env("GROK_LEADER_SOCKET");

    let grok_home = sandbox.grok_home().to_path_buf();
    std::fs::create_dir_all(&grok_home).expect("create .grok home");

    // The helper records the args it was invoked with (proving direct exec, no
    // shell) and prints a JSON token payload.
    let seen_args = grok_home.join("seen-args");
    let helper = grok_home.join("mock-auth-json.sh");
    std::fs::write(
        &helper,
        format!(
            "#!/bin/sh\nprintf '%s' \"$*\" > {}\n\
             printf '{{\"access_token\":\"gateway-tok-json\",\"expires_in\":3600}}'\n",
            seen_args.display()
        ),
    )
    .expect("write mock auth binary");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    std::fs::write(
        grok_home.join("config.toml"),
        format!(
            r#"[auth_provider.gateway]
command = "{helper}"
args = ["--profile", "corp"]
token_ttl_secs = 3600
timeout_secs = 10

[model.proxied-gateway]
model = "mock-gateway-model"
base_url = "{base}"
context_window = 200000
auth_provider = "gateway"
"#,
            helper = helper.display(),
            base = server.url(),
        ),
    )
    .expect("write config.toml");

    let mut cmd = tokio::process::Command::new(grok_binary());
    cmd.args([
        "-p",
        "say hi",
        "--yolo",
        "--model",
        "proxied-gateway",
        "--max-turns",
        "1",
        "--output-format",
        "json",
    ])
    .arg("--cwd")
    .arg(sandbox.workspace())
    .current_dir(sandbox.workspace())
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .kill_on_drop(true);

    let result = run_headless_in_sandbox_borrowed(cmd, &sandbox).await;
    assert_headless_success(&result, "auth provider args/json e2e", Some(&server));

    let args = std::fs::read_to_string(&seen_args).expect("helper must have run");
    assert_eq!(
        args, "--profile corp",
        "args must be passed directly to the helper with no shell"
    );

    let requests = server.requests();
    let chat = requests
        .iter()
        .find(|e| e.method == "POST" && e.path.contains("chat/completions"))
        .unwrap_or_else(|| {
            panic!(
                "no POST /v1/chat/completions request logged; requests:\n{}",
                server.request_log_summary()
            )
        });
    assert_eq!(
        chat.authorization.as_deref(),
        Some("Bearer gateway-tok-json"),
        "the JSON access_token must reach the wire; requests:\n{}",
        server.request_log_summary()
    );
}
