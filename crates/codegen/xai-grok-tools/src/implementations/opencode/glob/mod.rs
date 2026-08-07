//! `glob` tool — OpenCode architecture (`Tool` trait).
//!
//! File pattern matching using ripgrep's `--files` mode with glob filters.
//! Returns matching file paths sorted by modification time (most recent first),
//! capped at 100 results.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::implementations::grok_build::grep::ripgrep::rg_path;
use crate::types::output::ToolOutput;
#[allow(unused_imports)]
use crate::types::resources::{
    Cwd, DisplayCwd, SharedResources, display_cwd_or_cwd, resolve_model_path,
};
use crate::types::tool::{ToolKind, ToolNamespace};
use crate::types::tool_io::ToolInput;
#[cfg(feature = "pi")]
use crate::util::fd_path;

// ─── Constants ──────────────────────────────────────────────────────

const RESULT_LIMIT: usize = 100;

/// Hard cap on bytes read from ripgrep's stdout (5 MB).
const MAX_STDOUT_BYTES: usize = 5_000_000;

struct BackendRun {
    stdout_buf: Vec<u8>,
    truncated_by_bytes: bool,
    #[cfg(feature = "pi")]
    status_success: bool,
}

// ─── Description ────────────────────────────────────────────────────

const DESCRIPTION: &str = r#"Fast file pattern matching tool that works with any codebase size.

- Supports glob patterns like "**/*.js" or "src/**/*.ts" via the required ${{ params.list.pattern }} parameter
- Optionally set ${{ params.list.path }} to pick the directory to search in (defaults to the current working directory)
- Returns matching file paths sorted by modification time (most recent first), capped at 100 results
- Hidden (dot) files are included; .gitignore patterns are respected for paths the pattern does not explicitly match
- PI builds (`--features pi`) use `fd` first and automatically fall back to ripgrep on backend errors
- Use this tool when you need to find files by name patterns
- You can call multiple tools in a single response. It is always better to speculatively perform multiple searches as a batch that are potentially useful."#;

// ─── Input ──────────────────────────────────────────────────────────

/// Input for the `glob` tool.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct GlobInput {
    /// Glob pattern to match files against (e.g. "**/*.ts", "src/**/*.tsx").
    pub pattern: String,

    /// Directory to search in. Defaults to the current working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl TryFrom<ToolInput> for GlobInput {
    type Error = String;
    fn try_from(value: ToolInput) -> Result<Self, Self::Error> {
        match value {
            ToolInput::Dynamic(v) => {
                serde_json::from_value(v).map_err(|e| format!("GlobInput: {e}"))
            }
            _ => Err("expected Dynamic variant for GlobInput".into()),
        }
    }
}

impl From<GlobInput> for ToolInput {
    fn from(value: GlobInput) -> Self {
        ToolInput::Dynamic(serde_json::to_value(value).expect("GlobInput serializes to JSON"))
    }
}

// ─── Output ─────────────────────────────────────────────────────────

/// Structured output for the `glob` tool.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GlobOutput {
    /// Pre-formatted text for the model prompt.
    pub tool_output_for_prompt: String,
    /// Number of files included in `entries` (capped at `RESULT_LIMIT`).
    pub count: usize,
    /// Total files matched by ripgrep before the cap. May exceed `count`.
    /// When `truncated_by_bytes` was hit this is a lower bound.
    pub total_count: usize,
    /// Whether results were truncated at the limit.
    pub truncated: bool,
    /// Absolute paths of matched files included in `count`, sorted by mtime
    /// descending. Empty when `count == 0`.
    pub entries: Vec<String>,
    /// The model-facing workspace root used to resolve `path` -- equal to
    /// `display_cwd_or_cwd(cwd, display_cwd)`. Adapters that re-format the
    /// output use this as the relativization base when
    /// the model omits `path`, instead of re-resolving cwd themselves.
    pub cwd_for_display: String,
}

impl xai_tool_runtime::ToolOutput for GlobOutput {}

impl From<GlobOutput> for ToolOutput {
    fn from(output: GlobOutput) -> Self {
        ToolOutput::Text(output.tool_output_for_prompt.into())
    }
}

// ─── Tool ───────────────────────────────────────────────────────────

/// Glob tool — lists files matching a glob pattern, sorted by mtime.
#[derive(Debug, Default)]
pub struct GlobTool;

impl crate::types::tool_metadata::ToolMetadata for GlobTool {
    fn kind(&self) -> ToolKind {
        ToolKind::List
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::OpenCode
    }

    fn description_template(&self) -> &str {
        DESCRIPTION
    }
}

fn build_rg_command(search_dir: &Path, pattern: &str) -> Command {
    let rg_exec = rg_path();
    let mut cmd = Command::new(rg_exec);
    cmd.arg("--files")
        .arg("--glob=!.git/*")
        .arg("--hidden")
        .arg("--glob")
        .arg(pattern)
        .arg(search_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::util::detach_command(&mut cmd);
    cmd.stdin(Stdio::null());
    cmd
}

#[cfg(feature = "pi")]
fn build_fd_command(fd_exec: &Path, search_dir: &Path, pattern: &str) -> Command {
    let mut cmd = Command::new(fd_exec);
    cmd.arg("--type")
        .arg("f")
        .arg("--hidden")
        .arg("--exclude")
        .arg(".git")
        .arg("--strip-cwd-prefix")
        .arg("--glob")
        .arg(pattern)
        .arg("--search-path")
        .arg(search_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::util::detach_command(&mut cmd);
    cmd.stdin(Stdio::null());
    cmd
}

async fn run_files_command(mut cmd: Command) -> Result<BackendRun, std::io::Error> {
    #[allow(clippy::disallowed_methods)] // search helper, waited on below
    let mut child = cmd.spawn()?;

    let mut stdout_buf = Vec::with_capacity(MAX_STDOUT_BYTES.min(65_536));
    let mut truncated_by_bytes = false;
    if let Some(mut stdout_pipe) = child.stdout.take() {
        let mut tmp = [0u8; 8192];
        loop {
            match stdout_pipe.read(&mut tmp).await {
                Ok(0) => break,
                Ok(n) => {
                    if stdout_buf.len() + n <= MAX_STDOUT_BYTES {
                        stdout_buf.extend_from_slice(&tmp[..n]);
                    } else {
                        let remaining = MAX_STDOUT_BYTES.saturating_sub(stdout_buf.len());
                        if remaining > 0 {
                            stdout_buf.extend_from_slice(&tmp[..remaining]);
                        }
                        truncated_by_bytes = true;
                        let _ = child.start_kill();
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    }

    // Consume stderr to avoid deadlocks.
    if let Some(stderr_pipe) = child.stderr.take() {
        let _ = stderr_pipe
            .take(1_000_000)
            .read_to_end(&mut Vec::new())
            .await;
    }

    #[cfg(feature = "pi")]
    let status_success = child.wait().await?.success();
    #[cfg(not(feature = "pi"))]
    let _ = child.wait().await?;
    Ok(BackendRun {
        stdout_buf,
        truncated_by_bytes,
        #[cfg(feature = "pi")]
        status_success,
    })
}

async fn run_glob_backend(search_dir: &Path, pattern: &str) -> Result<BackendRun, std::io::Error> {
    #[cfg(feature = "pi")]
    {
        let fd_exec = fd_path();
        match run_files_command(build_fd_command(&fd_exec, search_dir, pattern)).await {
            Ok(out) if out.status_success || out.truncated_by_bytes => return Ok(out),
            Ok(_) => {
                tracing::debug!(
                    fd_path = %fd_exec.display(),
                    "fd glob backend exited non-zero; falling back to ripgrep"
                );
            }
            Err(err) => {
                tracing::debug!(
                    fd_path = %fd_exec.display(),
                    error = %err,
                    "fd glob backend failed to start; falling back to ripgrep"
                );
            }
        }
    }
    run_files_command(build_rg_command(search_dir, pattern)).await
}

impl xai_tool_runtime::Tool for GlobTool {
    type Args = GlobInput;
    type Output = GlobOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new("glob").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            "glob",
            crate::types::tool_metadata::ToolMetadata::sanitized_description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: true,
            tool_scope: Some(xai_tool_protocol::ToolScope::Read),
            ..Default::default()
        }
    }

    #[tracing::instrument(name = "tool.glob", skip_all)]
    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: GlobInput,
    ) -> Result<GlobOutput, xai_tool_runtime::ToolError> {
        use crate::types::tool_metadata::shared_resources;
        let resources = shared_resources(&ctx)?;

        let cwd = crate::types::tool_metadata::resolve_cwd(&ctx, &resources).await?;
        let display_cwd = resources
            .lock()
            .await
            .get::<DisplayCwd>()
            .map(|d| d.0.clone());

        // ── Resolve search directory ────────────────────────────
        let search_dir = resolve_model_path(
            &cwd,
            display_cwd.as_deref(),
            &input.path.clone().unwrap_or_default(),
        );

        // ── Run backend (PI fd with rg fallback, or rg-only) ────
        let backend = match run_glob_backend(&search_dir, &input.pattern).await {
            Ok(out) => out,
            Err(e) => {
                return Ok(GlobOutput {
                    tool_output_for_prompt: format!("Error running glob: {e}"),
                    count: 0,
                    total_count: 0,
                    truncated: false,
                    entries: Vec::new(),
                    cwd_for_display: display_cwd_or_cwd(&cwd, display_cwd.as_deref())
                        .display()
                        .to_string(),
                });
            }
        };

        // ── Parse file paths from stdout ────────────────────────
        let stdout = String::from_utf8_lossy(&backend.stdout_buf);
        let mut truncated = backend.truncated_by_bytes;

        struct FileEntry {
            path: PathBuf,
            mtime_ms: i64,
        }

        // Collect every match so total_count is accurate. Cap stat()s and
        // the returned entry list at RESULT_LIMIT so we don't pay the syscall
        // cost on huge result sets, but keep counting lines past the cap so
        // the truncation marker can report the real overflow.
        let mut entries: Vec<FileEntry> = Vec::new();
        let mut total_count: usize = 0;
        for line in stdout.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            total_count += 1;

            if entries.len() >= RESULT_LIMIT {
                truncated = true;
                continue;
            }

            let relative = line.strip_prefix("./").unwrap_or(line);
            let path = PathBuf::from(relative);
            let full_path = if path.is_absolute() {
                path
            } else {
                search_dir.join(path)
            };
            let mtime_ms = std::fs::metadata(&full_path)
                .ok()
                .and_then(|m| m.modified().ok())
                .map(|t| {
                    t.duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0)
                })
                .unwrap_or(0);

            entries.push(FileEntry {
                path: full_path,
                mtime_ms,
            });
        }

        // ── Sort by mtime descending (most recent first) ────────
        entries.sort_by(|a, b| b.mtime_ms.cmp(&a.mtime_ms));

        // ── Format output ───────────────────────────────────────
        let count = entries.len();
        let cwd_for_display = display_cwd_or_cwd(&cwd, display_cwd.as_deref());
        let entry_paths: Vec<String> = entries
            .iter()
            .map(|e| e.path.display().to_string())
            .collect();

        let tool_output_for_prompt = if entry_paths.is_empty() {
            "No files found".to_string()
        } else {
            let mut lines: Vec<String> = entry_paths.clone();

            if truncated {
                lines.push(String::new());
                lines.push(format!(
                    "(Results are truncated: showing first {} results out of more. \
                     Use a more specific path or pattern to narrow results.)",
                    RESULT_LIMIT
                ));
            }

            // Wrap in workspace_result so the model sees the search context.
            format!(
                "<workspace_result workspace_path=\"{}\">\n{}\n</workspace_result>",
                cwd_for_display.display(),
                lines.join("\n"),
            )
        };

        Ok(GlobOutput {
            tool_output_for_prompt,
            count,
            total_count,
            truncated,
            entries: entry_paths,
            cwd_for_display: cwd_for_display.display().to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::tool_metadata::test_ctx;

    use crate::types::resources::Resources;
    #[cfg(all(feature = "pi", unix))]
    use std::ffi::OsString;
    #[cfg(all(feature = "pi", unix))]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(all(feature = "pi", unix))]
    use std::sync::{Mutex, OnceLock};
    use tempfile::TempDir;

    fn test_resources(cwd: &std::path::Path) -> Resources {
        let mut resources = Resources::new();
        resources.insert(Cwd(cwd.to_path_buf()));
        resources
    }

    #[test]
    fn description_template_tracks_renamed_pattern_and_path() {
        use crate::types::template_renderer::TemplateRenderer;
        use crate::types::tool_metadata::ToolMetadata;
        use std::collections::HashMap;

        let tools = HashMap::from([(ToolKind::List, "glob".to_string())]);
        let params = HashMap::from([(
            ToolKind::List,
            HashMap::from([
                ("pattern".to_string(), "file_pattern".to_string()),
                ("path".to_string(), "search_dir".to_string()),
            ]),
        )]);
        let rendered = TemplateRenderer::new(tools, params)
            .render(ToolMetadata::description_template(&GlobTool))
            .unwrap();
        assert!(
            rendered.contains("required file_pattern parameter")
                && rendered.contains("set search_dir"),
            "renamed pattern/path params must appear:\n{rendered}"
        );
        assert!(
            !rendered.contains("extension breakdowns") && !rendered.contains("dot-directories"),
            "stale list_dir-style claims must not remain:\n{rendered}"
        );
    }

    #[tokio::test]
    async fn glob_finds_matching_files() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("hello.ts"), "console.log('hi');\n").unwrap();
        std::fs::write(tmp.path().join("world.ts"), "export {};\n").unwrap();
        std::fs::write(tmp.path().join("readme.md"), "# readme\n").unwrap();

        let tool = GlobTool;
        let resources = test_resources(tmp.path());

        let output = xai_tool_runtime::Tool::run(
            &tool,
            test_ctx(resources.into_shared()),
            GlobInput {
                pattern: "*.ts".to_string(),
                path: None,
            },
        )
        .await
        .unwrap();

        assert_eq!(output.count, 2);
        assert!(!output.truncated);
        assert!(output.tool_output_for_prompt.contains(".ts"));
        assert!(!output.tool_output_for_prompt.contains("readme.md"));
    }

    #[tokio::test]
    async fn glob_no_matches_returns_empty() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("readme.md"), "# readme\n").unwrap();

        let tool = GlobTool;
        let resources = test_resources(tmp.path());

        let output = xai_tool_runtime::Tool::run(
            &tool,
            test_ctx(resources.into_shared()),
            GlobInput {
                pattern: "*.xyz_nonexistent".to_string(),
                path: None,
            },
        )
        .await
        .unwrap();

        assert_eq!(output.count, 0);
        assert!(!output.truncated);
        assert!(output.tool_output_for_prompt.contains("No files found"));
    }

    #[tokio::test]
    async fn glob_with_subdirectory_path() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("src");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(tmp.path().join("root.txt"), "root\n").unwrap();

        let tool = GlobTool;
        let resources = test_resources(tmp.path());

        let output = xai_tool_runtime::Tool::run(
            &tool,
            test_ctx(resources.into_shared()),
            GlobInput {
                pattern: "*.rs".to_string(),
                path: Some("src".to_string()),
            },
        )
        .await
        .unwrap();

        assert_eq!(output.count, 1);
        assert!(output.tool_output_for_prompt.contains("main.rs"));
        assert!(!output.tool_output_for_prompt.contains("root.txt"));
    }

    #[tokio::test]
    async fn glob_sorts_by_mtime_most_recent_first() {
        let tmp = TempDir::new().unwrap();

        // Create files with slight time gaps so mtime differs.
        std::fs::write(tmp.path().join("old.txt"), "old\n").unwrap();
        // Touch a second file after a small delay.
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(tmp.path().join("new.txt"), "new\n").unwrap();

        let tool = GlobTool;
        let resources = test_resources(tmp.path());

        let output = xai_tool_runtime::Tool::run(
            &tool,
            test_ctx(resources.into_shared()),
            GlobInput {
                pattern: "*.txt".to_string(),
                path: None,
            },
        )
        .await
        .unwrap();

        assert_eq!(output.count, 2);
        // "new.txt" should appear before "old.txt" in the output.
        let new_pos = output.tool_output_for_prompt.find("new.txt").unwrap();
        let old_pos = output.tool_output_for_prompt.find("old.txt").unwrap();
        assert!(
            new_pos < old_pos,
            "new.txt should appear before old.txt (mtime sort), got new@{} old@{}",
            new_pos,
            old_pos
        );
    }

    #[tokio::test]
    async fn glob_recursive_pattern() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(tmp.path().join("top.rs"), "top\n").unwrap();
        std::fs::write(nested.join("deep.rs"), "deep\n").unwrap();

        let tool = GlobTool;
        let resources = test_resources(tmp.path());

        let output = xai_tool_runtime::Tool::run(
            &tool,
            test_ctx(resources.into_shared()),
            GlobInput {
                pattern: "**/*.rs".to_string(),
                path: None,
            },
        )
        .await
        .unwrap();

        assert_eq!(output.count, 2);
        assert!(output.tool_output_for_prompt.contains("top.rs"));
        assert!(output.tool_output_for_prompt.contains("deep.rs"));
    }

    #[test]
    fn tool_metadata() {
        use crate::types::tool_metadata::ToolMetadata;
        let tool = GlobTool;
        assert_eq!(xai_tool_runtime::Tool::id(&tool).as_str(), "glob");
        assert!(matches!(tool.kind(), ToolKind::List));
        assert!(matches!(tool.tool_namespace(), ToolNamespace::OpenCode));
    }

    #[test]
    fn serde_roundtrip() {
        let json = r#"{"pattern":"**/*.ts","path":"src"}"#;
        let input: GlobInput = serde_json::from_str(json).unwrap();
        assert_eq!(input.pattern, "**/*.ts");
        assert_eq!(input.path.as_deref(), Some("src"));

        // Minimal — path omitted
        let json_min = r#"{"pattern":"*.rs"}"#;
        let input_min: GlobInput = serde_json::from_str(json_min).unwrap();
        assert_eq!(input_min.pattern, "*.rs");
        assert!(input_min.path.is_none());

        // Round-trip through serde_json::Value
        let value = serde_json::to_value(&input).unwrap();
        let back: GlobInput = serde_json::from_value(value).unwrap();
        assert_eq!(back.pattern, "**/*.ts");
        assert_eq!(back.path.as_deref(), Some("src"));
    }

    #[tokio::test]
    async fn absolute_path_parameter() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("abs_target");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("found.txt"), "data\n").unwrap();

        let tool = GlobTool;
        // cwd is the tmp root, but we pass the absolute path to the sub dir
        let resources = test_resources(tmp.path());

        let output = xai_tool_runtime::Tool::run(
            &tool,
            test_ctx(resources.into_shared()),
            GlobInput {
                pattern: "*.txt".to_string(),
                path: Some(sub.to_string_lossy().to_string()),
            },
        )
        .await
        .unwrap();

        assert_eq!(output.count, 1);
        assert!(output.tool_output_for_prompt.contains("found.txt"));
    }

    #[tokio::test]
    async fn empty_directory() {
        let tmp = TempDir::new().unwrap();
        // Directory exists but contains no files.

        let tool = GlobTool;
        let resources = test_resources(tmp.path());

        let output = xai_tool_runtime::Tool::run(
            &tool,
            test_ctx(resources.into_shared()),
            GlobInput {
                pattern: "*".to_string(),
                path: None,
            },
        )
        .await
        .unwrap();

        assert_eq!(output.count, 0);
        assert!(!output.truncated);
        assert!(output.tool_output_for_prompt.contains("No files found"));
    }

    #[tokio::test]
    async fn path_empty_string_defaults_to_cwd() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("root.rs"), "fn main() {}\n").unwrap();

        let tool = GlobTool;
        let resources = test_resources(tmp.path());

        let output = xai_tool_runtime::Tool::run(
            &tool,
            test_ctx(resources.into_shared()),
            GlobInput {
                pattern: "*.rs".to_string(),
                path: Some(String::new()),
            },
        )
        .await
        .unwrap();

        assert_eq!(output.count, 1);
        assert!(output.tool_output_for_prompt.contains("root.rs"));
    }

    #[tokio::test]
    async fn missing_cwd_resource() {
        let tool = GlobTool;
        let resources = Resources::new(); // No Cwd inserted

        let result = xai_tool_runtime::Tool::run(
            &tool,
            test_ctx(resources.into_shared()),
            GlobInput {
                pattern: "*.rs".to_string(),
                path: None,
            },
        )
        .await;

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Cwd not available")
        );
    }

    #[tokio::test]
    async fn result_cap_100() {
        let tmp = TempDir::new().unwrap();
        for i in 0..110 {
            std::fs::write(tmp.path().join(format!("file_{:03}.txt", i)), "data\n").unwrap();
        }

        let tool = GlobTool;
        let resources = test_resources(tmp.path());

        let output = xai_tool_runtime::Tool::run(
            &tool,
            test_ctx(resources.into_shared()),
            GlobInput {
                pattern: "*.txt".to_string(),
                path: None,
            },
        )
        .await
        .unwrap();

        assert_eq!(output.count, 100);
        assert!(output.truncated);
        assert!(
            output.tool_output_for_prompt.contains("showing first 100"),
            "expected truncation message, got: {}",
            output.tool_output_for_prompt
        );
    }

    #[tokio::test]
    async fn hidden_files_included() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".hidden.ts"), "hidden\n").unwrap();
        std::fs::write(tmp.path().join("visible.ts"), "visible\n").unwrap();

        let tool = GlobTool;
        let resources = test_resources(tmp.path());

        let output = xai_tool_runtime::Tool::run(
            &tool,
            test_ctx(resources.into_shared()),
            GlobInput {
                pattern: "*.ts".to_string(),
                path: None,
            },
        )
        .await
        .unwrap();

        assert!(
            output.count >= 2,
            "expected at least 2 files, got {}",
            output.count
        );
        assert!(
            output.tool_output_for_prompt.contains(".hidden.ts"),
            "hidden file should appear in output: {}",
            output.tool_output_for_prompt
        );
    }

    #[tokio::test]
    async fn gitignore_respected() {
        // ripgrep's positive --glob overrides .gitignore, so we test the
        // underlying ignore behavior by using a pattern that doesn't match
        // the ignored file. Without .gitignore, `rg --files --hidden`
        // *would* list ignored_dir/ contents, but with .gitignore they are
        // excluded from results that don't glob-override them.
        let tmp = TempDir::new().unwrap();

        // Initialize a git repo so ripgrep respects .gitignore.
        xai_test_utils::git::ensure_hermetic_git_on_path();
        let status = std::process::Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("git must be available");
        assert!(status.success(), "git init failed");

        // Ignore an entire directory via .gitignore.
        std::fs::write(tmp.path().join(".gitignore"), "ignored_dir/\n").unwrap();

        // Create an ignored directory with a .txt file inside it.
        let ignored = tmp.path().join("ignored_dir");
        std::fs::create_dir(&ignored).unwrap();
        std::fs::write(ignored.join("secret.txt"), "should be ignored\n").unwrap();

        // Create a non-ignored .txt file.
        std::fs::write(tmp.path().join("visible.txt"), "should be found\n").unwrap();

        let tool = GlobTool;
        let resources = test_resources(tmp.path());

        // Pattern **/*.txt would match both files if .gitignore were not in
        // effect. Because ripgrep processes the ignore stack *before* applying
        // the glob whitelist for directory ignores, ignored_dir/ is pruned.
        let output = xai_tool_runtime::Tool::run(
            &tool,
            test_ctx(resources.into_shared()),
            GlobInput {
                pattern: "**/*.txt".to_string(),
                path: None,
            },
        )
        .await
        .unwrap();

        assert!(
            output.tool_output_for_prompt.contains("visible.txt"),
            "visible.txt should be in output: {}",
            output.tool_output_for_prompt
        );
        assert!(
            !output.tool_output_for_prompt.contains("secret.txt"),
            "secret.txt inside ignored_dir/ should be excluded by .gitignore: {}",
            output.tool_output_for_prompt
        );
    }

    #[tokio::test]
    async fn output_format_workspace_result() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("example.rs"), "fn main() {}\n").unwrap();

        let tool = GlobTool;
        let resources = test_resources(tmp.path());

        let output = xai_tool_runtime::Tool::run(
            &tool,
            test_ctx(resources.into_shared()),
            GlobInput {
                pattern: "*.rs".to_string(),
                path: None,
            },
        )
        .await
        .unwrap();

        assert!(
            output
                .tool_output_for_prompt
                .starts_with("<workspace_result"),
            "output should start with <workspace_result tag, got: {}",
            output.tool_output_for_prompt
        );
        assert!(
            output
                .tool_output_for_prompt
                .ends_with("</workspace_result>"),
            "output should end with </workspace_result>, got: {}",
            output.tool_output_for_prompt
        );
        assert!(
            output.tool_output_for_prompt.contains("workspace_path="),
            "output should contain workspace_path attribute, got: {}",
            output.tool_output_for_prompt
        );
    }

    #[cfg(all(feature = "pi", unix))]
    fn fd_test_env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[cfg(all(feature = "pi", unix))]
    struct ScopedEnvVar {
        key: &'static str,
        old: Option<OsString>,
    }

    #[cfg(all(feature = "pi", unix))]
    impl ScopedEnvVar {
        fn set(key: &'static str, value: &std::ffi::OsStr) -> Self {
            let old = std::env::var_os(key);
            // SAFETY: tests mutate process env only while holding `fd_test_env_lock`.
            unsafe { std::env::set_var(key, value) };
            Self { key, old }
        }
    }

    #[cfg(all(feature = "pi", unix))]
    impl Drop for ScopedEnvVar {
        fn drop(&mut self) {
            match &self.old {
                Some(value) => {
                    // SAFETY: tests mutate process env only while holding `fd_test_env_lock`.
                    unsafe { std::env::set_var(self.key, value) };
                }
                None => {
                    // SAFETY: tests mutate process env only while holding `fd_test_env_lock`.
                    unsafe { std::env::remove_var(self.key) };
                }
            }
        }
    }

    #[cfg(all(feature = "pi", unix))]
    fn write_executable_script(path: &std::path::Path, body: &str) {
        std::fs::write(path, body).unwrap();
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    #[cfg(all(feature = "pi", unix))]
    #[tokio::test]
    async fn pi_glob_fd_contract_preserves_public_glob_semantics() {
        let _env_lock = fd_test_env_lock().lock().unwrap();
        let tmp = TempDir::new().unwrap();

        xai_test_utils::git::ensure_hermetic_git_on_path();
        let status = std::process::Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("git must be available");
        assert!(status.success(), "git init failed");

        std::fs::write(tmp.path().join(".gitignore"), "ignored_dir/\n").unwrap();
        let ignored = tmp.path().join("ignored_dir");
        std::fs::create_dir(&ignored).unwrap();
        std::fs::write(ignored.join("secret.txt"), "ignored\n").unwrap();
        std::fs::write(tmp.path().join(".hidden.txt"), "hidden\n").unwrap();
        std::fs::write(tmp.path().join("old.txt"), "old\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(40));
        std::fs::write(tmp.path().join("new.txt"), "new\n").unwrap();

        let fd_script = tmp.path().join("fd-glob-proxy.sh");
        let log = tmp.path().join("fd-glob-invocation.log");
        write_executable_script(
            &fd_script,
            r#"#!/bin/sh
set -eu

pattern=""
search_path=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --glob)
      shift
      pattern="${1-}"
      ;;
    --search-path)
      shift
      search_path="${1-}"
      ;;
  esac
  shift || true
done

if [ -n "${FD_TEST_INVOCATION_LOG-}" ]; then
  printf 'pattern=%s\nsearch_path=%s\n' "$pattern" "$search_path" > "$FD_TEST_INVOCATION_LOG"
fi

if [ -z "$pattern" ] || [ -z "$search_path" ]; then
  echo "missing --glob/--search-path" >&2
  exit 64
fi

rg --files --glob='!.git/*' --hidden --glob "$pattern" "$search_path"
"#,
        );

        let _fd_path = ScopedEnvVar::set("GROK_TOOLS_FD_PATH", fd_script.as_os_str());
        let _fd_log = ScopedEnvVar::set("FD_TEST_INVOCATION_LOG", log.as_os_str());

        let tool = GlobTool;
        let resources = test_resources(tmp.path());
        let output = xai_tool_runtime::Tool::run(
            &tool,
            test_ctx(resources.into_shared()),
            GlobInput {
                pattern: "**/*.txt".to_string(),
                path: None,
            },
        )
        .await
        .unwrap();

        assert!(
            output.tool_output_for_prompt.contains("new.txt"),
            "new file should be present: {}",
            output.tool_output_for_prompt
        );
        assert!(
            output.tool_output_for_prompt.contains("old.txt"),
            "old file should be present: {}",
            output.tool_output_for_prompt
        );
        assert!(
            output.tool_output_for_prompt.contains(".hidden.txt"),
            "hidden file should be present: {}",
            output.tool_output_for_prompt
        );
        assert!(
            !output.tool_output_for_prompt.contains("secret.txt"),
            "gitignored file should stay excluded: {}",
            output.tool_output_for_prompt
        );

        let new_pos = output.tool_output_for_prompt.find("new.txt").unwrap();
        let old_pos = output.tool_output_for_prompt.find("old.txt").unwrap();
        assert!(
            new_pos < old_pos,
            "mtime ordering should keep new before old (new@{new_pos}, old@{old_pos})"
        );

        let invocation = std::fs::read_to_string(&log).expect("fd proxy should log invocation");
        assert!(
            invocation.contains("pattern=**/*.txt"),
            "fd glob backend should receive the requested pattern: {invocation}"
        );
        assert!(
            invocation.contains("search_path="),
            "fd glob backend should receive a search path: {invocation}"
        );
    }

    #[cfg(all(feature = "pi", unix))]
    #[tokio::test]
    async fn pi_glob_fd_error_falls_back_to_ripgrep() {
        let _env_lock = fd_test_env_lock().lock().unwrap();
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("fallback.txt"), "ok\n").unwrap();

        let fd_script = tmp.path().join("fd-fail.sh");
        let log = tmp.path().join("fd-fail.log");
        write_executable_script(
            &fd_script,
            r#"#!/bin/sh
set -eu
if [ -n "${FD_TEST_INVOCATION_LOG-}" ]; then
  printf 'failed=1\n' > "$FD_TEST_INVOCATION_LOG"
fi
echo "intentional fd failure" >&2
exit 70
"#,
        );

        let _fd_path = ScopedEnvVar::set("GROK_TOOLS_FD_PATH", fd_script.as_os_str());
        let _fd_log = ScopedEnvVar::set("FD_TEST_INVOCATION_LOG", log.as_os_str());

        let tool = GlobTool;
        let resources = test_resources(tmp.path());
        let output = xai_tool_runtime::Tool::run(
            &tool,
            test_ctx(resources.into_shared()),
            GlobInput {
                pattern: "*.txt".to_string(),
                path: None,
            },
        )
        .await
        .unwrap();

        assert!(
            output.tool_output_for_prompt.contains("fallback.txt"),
            "rg fallback should still return matches: {}",
            output.tool_output_for_prompt
        );
        assert!(
            !output.tool_output_for_prompt.contains("Error running glob"),
            "fd failure should not leak as a hard glob error: {}",
            output.tool_output_for_prompt
        );
        let invocation =
            std::fs::read_to_string(&log).expect("fd failure path should be attempted");
        assert!(
            invocation.contains("failed=1"),
            "fd fallback test should prove the fd backend was attempted: {invocation}"
        );
    }
}
