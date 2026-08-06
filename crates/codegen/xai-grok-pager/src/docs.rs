//! In-app how-to documentation data (embedded markdown).
//!
//! Single source of truth: two static arrays (`USER_GUIDE`, `REFERENCE_DOCS`)
//! hold every doc. All lookups are zero-allocation; `DocEntry` exists only for
//! backward compatibility with the TUI doc picker.

/// A compile-time document entry. All fields are `&'static str`.
#[derive(Debug)]
pub struct Doc {
    pub filename: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    /// Deliberately not `pub`. Every reader must go through [`doc_content`],
    /// which applies the invoked-program rename; a direct read ships the
    /// upstream `grok …` commands. Five separate readers had to be found and
    /// fixed by hand before this was closed off — the compiler is a better
    /// guard than a checklist.
    content: &'static str,
}

/// Owned variant for the TUI doc picker (backward compat).
#[derive(Debug, Clone)]
pub struct DocEntry {
    pub title: String,
    pub description: String,
    /// Embedded markdown content.
    pub content: &'static str,
}

impl From<&Doc> for DocEntry {
    fn from(d: &Doc) -> Self {
        Self {
            title: d.title.into(),
            description: d.description.into(),
            // Not `d.content`: the picker must show the same text the on-disk
            // copy has. See [`doc_content`].
            content: doc_content(d),
        }
    }
}

// ── Static doc tables ────────────────────────────────────────────────────────

macro_rules! guide {
    ($file:literal, $title:literal, $desc:literal) => {
        Doc {
            filename: $file,
            title: $title,
            description: $desc,
            content: include_str!(concat!("../docs/user-guide/", $file)),
        }
    };
}

pub static USER_GUIDE: &[Doc] = &[
    guide!(
        "01-getting-started.md",
        "Getting Started",
        "Installation, first launch, and basic interaction"
    ),
    guide!(
        "02-authentication.md",
        "Authentication",
        "Browser login, API keys, OIDC, external auth providers"
    ),
    guide!(
        "03-keyboard-shortcuts.md",
        "Keyboard Shortcuts",
        "Complete reference for all TUI key bindings"
    ),
    guide!(
        "04-slash-commands.md",
        "Slash Commands",
        "All / commands, including goals, research, and workflow management"
    ),
    guide!(
        "05-configuration.md",
        "Configuration",
        "config.toml, pager.toml, environment variables, file locations"
    ),
    guide!(
        "06-theming.md",
        "Theming and Appearance",
        "Themes, color support, pager.toml customization"
    ),
    guide!(
        "07-mcp-servers.md",
        "MCP Servers",
        "Setting up external tool integrations via MCP"
    ),
    guide!(
        "08-skills.md",
        "Skills",
        "Creating and using reusable prompt packages"
    ),
    guide!(
        "09-plugins.md",
        "Plugins and Marketplace",
        "Installing, managing, and creating plugin packages"
    ),
    guide!(
        "10-hooks.md",
        "Hooks",
        "Project lifecycle scripts for pre/post tool-use events"
    ),
    guide!(
        "11-custom-models.md",
        "Custom Models",
        "BYOK, Ollama, OpenAI-compatible endpoints"
    ),
    guide!(
        "12-project-rules.md",
        "Project Rules (AGENTS.md)",
        "Per-directory instructions and precedence rules"
    ),
    guide!(
        "13-memory.md",
        "Memory",
        "Cross-session knowledge persistence and search"
    ),
    guide!(
        "14-headless-mode.md",
        "Headless Mode and Scripting",
        "Non-interactive CLI for automation and CI/CD"
    ),
    guide!(
        "15-agent-mode.md",
        "Agent Mode and IDE Integration",
        "ACP stdio transport, WebSocket relay, SDK integration"
    ),
    guide!(
        "16-subagents.md",
        "Subagents and Personas",
        "Spawning parallel child agents with specialized roles"
    ),
    guide!(
        "17-sessions.md",
        "Session Management",
        "Save, load, resume, rewind, and compact sessions"
    ),
    guide!(
        "18-sandbox.md",
        "Sandbox Mode",
        "OS-level filesystem and network isolation"
    ),
    guide!(
        "19-plan-mode.md",
        "Plan Mode",
        "Structured planning with approval dialogs"
    ),
    guide!(
        "20-background-tasks.md",
        "Background Tasks and Monitoring",
        "Background commands, /loop, monitor, scheduler"
    ),
    guide!(
        "21-terminal-support.md",
        "Terminal Support and Troubleshooting",
        "tmux, Byobu, Zellij, SSH, truecolor, clipboard, and diagnostics"
    ),
    guide!(
        "22-permissions-and-safety.md",
        "Permissions and Safety",
        "Modes, authorization order, allow/ask/deny rules, matching, and hooks"
    ),
    guide!(
        "23-dashboard.md",
        "Agent Dashboard",
        "Live multi-session roster: peek, dispatch, pin, stop, and search"
    ),
    guide!(
        "24-monitoring-usage.md",
        "Monitoring Usage (External OpenTelemetry)",
        "Export usage metrics to a customer OpenTelemetry collector"
    ),
];

/// Non-user-guide reference docs. Separate from USER_GUIDE because they
/// live under `docs/` (not `docs/user-guide/`), are not extracted to disk,
/// and do not follow the NN-*.md managed naming pattern. Bundled via
/// `include_str!` so they are available at runtime without a docs path.
static REFERENCE_DOCS: &[Doc] = &[
    Doc {
        filename: "hooks-and-plugins.md",
        title: "Hooks & Plugins Guide",
        description: "Using hooks, plugins, and marketplace",
        content: include_str!("../docs/hooks-and-plugins.md"),
    },
    Doc {
        filename: "custom-hooks.md",
        title: "Creating Custom Hooks",
        description: "Writing your own hooks and matchers",
        content: include_str!("../docs/custom-hooks.md"),
    },
];

// ── Public API ───────────────────────────────────────────────────────────────

/// Find a doc by title (case-insensitive). Returns the static entry.
pub fn find_doc(title: &str) -> Option<&'static Doc> {
    USER_GUIDE
        .iter()
        .chain(REFERENCE_DOCS.iter())
        .find(|d| d.title.eq_ignore_ascii_case(title))
}

/// All doc titles, zero allocation.
pub fn all_titles() -> impl Iterator<Item = &'static str> {
    USER_GUIDE
        .iter()
        .chain(REFERENCE_DOCS.iter())
        .map(|d| d.title)
}

/// Returns the content of a how-to document by exact title match (case-insensitive).
pub fn get_howto_doc(title: &str) -> Option<&'static str> {
    find_doc(title).map(doc_content)
}

/// A doc's body as the reader should see it, with command examples renamed.
///
/// Every path that puts a doc in front of a user or the model goes through
/// here: the copies written to `<grok_home>/docs/`, the TUI doc picker, the
/// tutorial's "go deeper" view, and [`get_howto_doc`]. Renaming only at write
/// time — which is what the first version did — left the in-app viewer showing
/// `grok login` for the very file whose on-disk copy said `medley login`.
///
/// Computed once. `Box::leak` is deliberate: the set is fixed and small, the
/// strings live as long as the process, and every reader needs `&'static str`.
pub fn doc_content(doc: &Doc) -> &'static str {
    static RENAMED: std::sync::OnceLock<std::collections::HashMap<&'static str, &'static str>> =
        std::sync::OnceLock::new();
    RENAMED
        .get_or_init(|| {
            USER_GUIDE
                .iter()
                .chain(REFERENCE_DOCS.iter())
                .filter_map(|d| match rename_commands_to_invoked_program(d.content) {
                    // Borrowed means nothing was rewritten; the static is fine.
                    std::borrow::Cow::Borrowed(_) => None,
                    std::borrow::Cow::Owned(renamed) => {
                        Some((d.filename, &*Box::leak(renamed.into_boxed_str())))
                    }
                })
                .collect()
        })
        .get(doc.filename)
        .copied()
        .unwrap_or(doc.content)
}

/// Returns a list of available how-to titles for the model to choose from.
pub fn list_howto_titles() -> Vec<String> {
    all_titles().map(String::from).collect()
}

/// Returns all docs as owned `DocEntry` values for the TUI doc picker.
pub fn default_howto_entries() -> Vec<DocEntry> {
    USER_GUIDE
        .iter()
        .chain(REFERENCE_DOCS.iter())
        .map(DocEntry::from)
        .collect()
}

/// Rewrite `` `grok …` `` command examples to name the program the user actually
/// invoked.
///
/// These docs are compiled in as static markdown and then written to disk **for
/// the model to read**, so an instruction here reaches both the user and the
/// agent. #117 made the binary stop calling itself `grok` in its own messages;
/// without this the shipped documentation still tells both of them to run a
/// command that, on a machine with the official build installed, drives the
/// *other* program — the exact failure that issue is about.
///
/// Two positions count as a command, and only one of them is a backtick span:
///
/// * the leading token of a line inside a fenced code block, optionally behind
///   a `$ ` or `> ` prompt. This is the copy-pasteable form, and in this guide
///   it is the *majority* of the references — an earlier version of this
///   function required a literal backtick before the name and so left every
///   fenced block untouched, which put `grok login` in a bash block four lines
///   above `medley login` in the prose of `02-authentication.md`.
/// * `grok` at a word boundary inside a backtick span, followed by a space or
///   by the closing backtick. Not only as the span's first token: the guide has
///   `` `sign in with grok login --provider openai-codex` ``, which the binary
///   itself prints with the invoked name.
///
/// The word-boundary rule is what keeps prose about the upstream product ("a
/// fork of Grok Build"), model ids (`grok-4.5`), `grok.com`, `xai-grok-pager`,
/// `GROK_HOME` and `~/.grok` untouched. Returns `Cow::Borrowed` when the
/// invoked name is already `grok`, so the common upstream case allocates
/// nothing.
fn rename_commands_to_invoked_program(content: &'static str) -> std::borrow::Cow<'static, str> {
    let program = xai_grok_config::program_name::program_name();
    if program == "grok" || !content.contains("grok") {
        return std::borrow::Cow::Borrowed(content);
    }
    let mut out = String::with_capacity(content.len());
    let mut in_fence = false;
    // A code span may wrap across lines, and the guide has one that does:
    // ``the reason `sign in with`` / ``grok login --provider openai-codex` ``.
    // Reset at a blank line and at a fence, so one unbalanced backtick can
    // never poison the rest of the document.
    let mut in_span = false;
    let mut fence_is_data = false;
    let mut changed = false;
    for line in content.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let is_fence = trimmed.starts_with("```") || trimmed.starts_with("~~~");
        if is_fence {
            // A data fence holds values, not commands. `{"vendor": "grok
            // build"}` in a json block is a string the reader must not see
            // rewritten. Bare fences stay command text: they are overwhelmingly
            // shell here, and treating them as data would reopen the gap this
            // whole function exists to close.
            fence_is_data = matches!(
                trimmed
                    .trim_start_matches(['`', '~'])
                    .trim()
                    .split(|c: char| !c.is_ascii_alphanumeric())
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase()
                    .as_str(),
                "json" | "toml" | "yaml" | "yml" | "ini" | "xml" | "csv" | "jsonc"
            );
        }
        if is_fence || trimmed.trim_end().is_empty() {
            in_span = false;
        }
        // A fence delimiter carries an info string and prose carries backtick
        // spans; the span scanner handles both. Only the interior of a fence is
        // bare command text.
        let rewritten = if is_fence || !in_fence {
            rename_in_code_spans(line, program, &mut in_span)
        } else if fence_is_data {
            // A data fence holds values, but its comments still cite commands:
            // `09-plugins.md` has a toml comment saying "(from `grok plugin
            // list`)". Backticks are the reader's own mark for "this is a
            // command", so honour them and leave bare words alone.
            let mut fence_span = false;
            rename_in_code_spans(line, program, &mut fence_span)
        } else {
            rename_words_in_code(line, program)
        };
        if is_fence {
            in_fence = !in_fence;
            in_span = false;
        }
        changed |= rewritten != line;
        out.push_str(&rewritten);
    }
    if changed {
        std::borrow::Cow::Owned(out)
    } else {
        std::borrow::Cow::Borrowed(content)
    }
}

/// Bytes that, immediately beside `grok`, mean it is part of a longer name
/// rather than the command: `grok-4.5`, `xai-grok-pager`, `.grok/config.toml`,
/// `grok.com`, `groknight`.
fn is_name_byte(b: u8) -> bool {
    // `$` is here for `$grok` -- a shell variable, not the program. `$ grok`
    // (the prompt form) is unaffected: the byte before the name is the space.
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/' | b'$')
}

/// Whether `grok` starts at `i` as a command: a word on its own, followed by a
/// space (a subcommand or flag follows) or by the end of its span.
///
/// `GROK_HOME` is excluded by the comparison being case-sensitive, and no URL
/// can match because a URL has no space in it and its `grok` is always preceded
/// by `/` or `.`.
fn grok_is_command_at(bytes: &[u8], i: usize) -> bool {
    if bytes.get(i..i + 4) != Some(b"grok".as_slice()) {
        return false;
    }
    if i > 0 && is_name_byte(bytes[i - 1]) {
        return false;
    }
    // Shell delimiters count, not just whitespace: `grok;`, `grok | jq`,
    // `grok && echo done`, `$(grok)` and a `grok\` line continuation are all
    // invocations. None appear in the guide today, and both this predicate and
    // the corpus test's detector shared the narrower boundary -- so the first
    // doc to use one would have shipped an un-renamed command with nothing to
    // catch it.
    match bytes.get(i + 4) {
        None => true,
        Some(&b) => matches!(
            b,
            b' ' | b'\t' | b'`' | b'\n' | b'\r' | b';' | b'|' | b'&' | b')' | b'\\'
        ),
    }
}

/// Rewrite command positions inside the backtick spans of one line.
///
/// `in_span` carries across lines because a code span may wrap; the caller
/// resets it at paragraph and fence boundaries.
fn rename_in_code_spans(line: &str, program: &str, in_span: &mut bool) -> String {
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            // Consume the whole run, so a ``double-backtick span`` toggles once
            // and keeps its delimiters balanced.
            let start = i;
            while i < bytes.len() && bytes[i] == b'`' {
                i += 1;
            }
            out.push_str(&line[start..i]);
            *in_span = !*in_span;
            continue;
        }
        if *in_span && grok_is_command_at(bytes, i) {
            out.push_str(program);
            i += 4;
            continue;
        }
        let len = line[i..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&line[i..i + len]);
        i += len;
    }
    out
}

/// Rewrite every command position on a line inside a fenced code block.
///
/// Not just the leading token. In the shipped guide the name also appears
/// behind an env-var prefix (`GROK_AGENT_SECRET='…' grok agent serve`), inside
/// a substitution (`RESULT=$(grok -p …)`), in a `#` comment, and in an ASCII
/// table cell — all of which a reader copies and a model imitates.
///
/// Inside a code block a standalone `grok` is the program, so the word
/// boundaries carry the whole burden here; they are what keep `/tmp/grok.log`
/// and `GROK_AGENT_SECRET` intact.
fn rename_words_in_code(line: &str, program: &str) -> String {
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < bytes.len() {
        if grok_is_command_at(bytes, i) {
            out.push_str(program);
            i += 4;
            continue;
        }
        let len = line[i..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&line[i..i + len]);
        i += len;
    }
    out
}

/// Extract user-guide docs to `<grok_home>/docs/user-guide/`.
///
/// Called from the pager binary startup so the model can read them from disk.
pub fn extract_user_guide_docs(grok_home: &std::path::Path) {
    let docs_dir = grok_home.join("docs").join("user-guide");
    if let Err(e) = std::fs::create_dir_all(&docs_dir) {
        tracing::warn!(error = %e, "Failed to create user-guide docs directory");
        return;
    }
    for doc in USER_GUIDE {
        let content = doc_content(doc);
        if let Err(e) = std::fs::write(docs_dir.join(doc.filename), content) {
            tracing::debug!(error = %e, filename = doc.filename, "Failed to extract user-guide doc");
        }
    }
    // Clean up stale managed docs (files removed from USER_GUIDE since last run).
    // Only remove files matching the managed naming pattern (NN-*.md).
    if let Ok(entries) = std::fs::read_dir(&docs_dir) {
        let valid: std::collections::HashSet<&str> =
            USER_GUIDE.iter().map(|d| d.filename).collect();
        for dir_entry in entries.flatten() {
            if let Some(name) = dir_entry.file_name().to_str() {
                let is_managed = name.len() > 3
                    && name.as_bytes()[0].is_ascii_digit()
                    && name.as_bytes()[1].is_ascii_digit()
                    && name.as_bytes()[2] == b'-'
                    && name.ends_with(".md");
                if is_managed
                    && !valid.contains(name)
                    && let Err(e) = std::fs::remove_file(dir_entry.path())
                {
                    tracing::debug!(error = %e, filename = name, "Failed to remove stale user-guide doc");
                }
            }
        }
    }
}

#[cfg(test)]
mod invoked_program_rename_tests {
    use super::rename_commands_to_invoked_program;

    /// The docs are written to disk for the model to read, so a command example
    /// here is an instruction to both the user and the agent (#117).
    #[test]
    fn backticked_commands_are_renamed_and_prose_is_not() {
        // Fixture for the rewriter under test, not an instruction this program
        // emits. auth-instruction-guard: exempt
        let src: &'static str = "Run `grok login` to sign in. \
             A fork of Grok Build. Model `grok-4.5` lives in ~/.grok. \
             See `grok mcp add --help`. The word grok alone is prose.";
        let out = rename_commands_to_invoked_program(src);

        // Under a unit test the invoked name is the test binary, which is
        // exactly the property being tested: whatever we were invoked as.
        let program = xai_grok_config::program_name::program_name();
        if program == "grok" {
            // Upstream-named build: borrowed, untouched, no allocation.
            assert!(matches!(out, std::borrow::Cow::Borrowed(_)));
            return;
        }

        assert!(
            out.contains(&format!("`{program} login`")),
            "a backticked command must name the invoked program: {out}"
        );
        assert!(
            out.contains(&format!("`{program} mcp add --help`")),
            "every backticked command, not just the first: {out}"
        );
        assert!(
            out.contains("A fork of Grok Build"),
            "prose about the upstream product must survive: {out}"
        );
        assert!(
            out.contains("`grok-4.5`"),
            "a model id is not a command: {out}"
        );
        assert!(out.contains("~/.grok"), "a path is not a command: {out}");
        assert!(
            out.contains("word grok alone"),
            "unbackticked prose is not a command: {out}"
        );
    }

    /// No `grok` commands at all must not allocate.
    #[test]
    fn content_without_commands_is_borrowed() {
        let out = rename_commands_to_invoked_program("nothing to rewrite here");
        assert!(matches!(out, std::borrow::Cow::Borrowed(_)));
    }

    /// Subcommands this CLI actually has. Used by the corpus scan below to tell
    /// a command from the word "grok" in a sentence.
    const SUBCOMMANDS: &[&str] = &[
        "login", "logout", "auth", "inspect", "plugin", "doctor", "wrap", "update", "sessions",
        "mcp", "agent", "models", "init", "setup", "config", "export", "worktree", "review",
    ];

    /// Does this line still invoke `grok` as a program?
    ///
    /// Written independently of `grok_is_command_at` on purpose: a detector
    /// that shares the implementation's predicate passes by construction and
    /// would have missed exactly the gap this test exists for.
    pub(super) fn still_invokes_grok(line: &str) -> bool {
        let bytes = line.as_bytes();
        line.match_indices("grok").any(|(i, _)| {
            let left_ok = i == 0
                || !(bytes[i - 1].is_ascii_alphanumeric()
                    || matches!(bytes[i - 1], b'-' | b'_' | b'.' | b'/'));
            if !left_ok {
                return false;
            }
            match bytes.get(i + 4) {
                // A shell delimiter right after the name is an invocation on
                // its own; there is no subcommand to inspect.
                Some(b';' | b'|' | b'&' | b')' | b'\\') => true,
                Some(b' ' | b'\t') => {
                    let next = line[i + 5..]
                        .split_whitespace()
                        .next()
                        .unwrap_or_default()
                        .trim_matches('`');
                    next.starts_with('-') || SUBCOMMANDS.contains(&next)
                }
                _ => false,
            }
        })
    }

    /// A fence is not automatically command text.
    ///
    /// Treating every fenced line as a command rewrote data: `"grok build"` as
    /// a JSON string value became `"medley build"`, and the shell variable
    /// `$grok` became `$medley`. But a data fence's *comments* still cite
    /// commands — `09-plugins.md` has a toml comment reading "(from `grok
    /// plugin list`)" — so a blanket skip loses those. Backticks are the
    /// reader's own mark for "this is a command"; inside a data fence they are
    /// the only mark honoured.
    #[test]
    fn data_fences_rename_only_what_is_backticked() {
        // Fixtures for the rewriter under test, not instructions this program
        // emits.
        let src: &'static str = concat!(
            "```json\n",
            "{\"vendor\": \"grok build\", \"note\": \"see `grok doctor`\"}\n",
            "```\n\n```bash\n",
            // auth-instruction-guard: exempt
            "$grok login\n",
            // auth-instruction-guard: exempt
            "$ grok login\n```\n",
        );
        let out = rename_commands_to_invoked_program(src);
        let program = xai_grok_config::program_name::program_name();
        if program == "grok" {
            assert!(matches!(out, std::borrow::Cow::Borrowed(_)));
            return;
        }

        assert!(
            out.contains("\"grok build\""),
            "a bare value in a data fence is data, not a command: {out}"
        );
        assert!(
            out.contains(&format!("`{program} doctor`")),
            "a backticked command in a data fence's comment is still a \
             command: {out}"
        );
        assert!(
            // auth-instruction-guard: exempt
            out.contains("$grok login"),
            "`$grok` is a shell variable, not the program: {out}"
        );
        assert!(
            out.contains(&format!("$ {program} login")),
            "`$ grok` is the prompt form and must still be renamed: {out}"
        );
    }

    /// The regression test for the fenced-block miss.
    ///
    /// The first version required a literal backtick before the name, so it
    /// rewrote 88 spans and left 137 — including every fenced code block, which
    /// is the form a reader copies and a model imitates. `02-authentication.md`
    /// shipped `grok login` inside a bash fence four lines above `medley login`
    /// in its own prose.
    ///
    /// This walks the real shipped guide, not a fixture. The previous tests all
    /// used hand-written strings, which is why none of them noticed.
    #[test]
    fn no_command_in_the_shipped_guide_still_invokes_grok() {
        let program = xai_grok_config::program_name::program_name();
        if program == "grok" {
            return;
        }
        let mut offenders = Vec::new();
        for doc in super::USER_GUIDE.iter().chain(super::REFERENCE_DOCS.iter()) {
            for (n, line) in super::doc_content(doc).lines().enumerate() {
                if still_invokes_grok(line) {
                    offenders.push(format!("{}:{}: {}", doc.filename, n + 1, line.trim()));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "{} command reference(s) still name `grok` after the rename, so the \
             docs written for the model to read tell it to run a different \
             program:\n{}",
            offenders.len(),
            offenders.join("\n")
        );

        // The other direction, so this test cannot be satisfied by renaming
        // more aggressively. These are the things in the corpus that contain
        // "grok" and are *not* the command; every one must survive untouched,
        // with the same count it has in the source.
        let renamed: String = super::USER_GUIDE
            .iter()
            .chain(super::REFERENCE_DOCS.iter())
            .map(|doc| super::doc_content(doc))
            .collect();
        let source: String = super::USER_GUIDE
            .iter()
            .chain(super::REFERENCE_DOCS.iter())
            .map(|doc| doc.content)
            .collect();
        for needle in [
            "grok-4.5",
            "grok.com",
            ".grok/",
            "~/.grok",
            "GROK_HOME",
            "xai-grok-pager",
            "Grok Build",
        ] {
            assert_eq!(
                renamed.matches(needle).count(),
                source.matches(needle).count(),
                "{needle:?} is not a command and must survive the rename intact"
            );
        }
    }

    /// The fenced form specifically, since that is the whole of the gap.
    #[test]
    fn fenced_blocks_and_bare_spans_are_renamed() {
        // Fixtures for the rewriter under test, not instructions this program
        // emits. The guard reads physical lines, so each one that names a
        // command carries its own marker.
        let src: &'static str = concat!(
            "Intro.\n\n```bash\n",
            // auth-instruction-guard: exempt
            "grok login\n",
            "$ grok mcp add x\n```\n\n",
            // auth-instruction-guard: exempt
            "Then run `grok`. The reason `sign in with grok login --provider x`.\n",
            // auth-instruction-guard: exempt
            "Not a command: grok login in plain prose, `grok-4.5`, `grok.com`.\n",
        );
        let out = rename_commands_to_invoked_program(src);
        let program = xai_grok_config::program_name::program_name();
        if program == "grok" {
            assert!(matches!(out, std::borrow::Cow::Borrowed(_)));
            return;
        }

        assert!(
            out.contains(&format!("\n{program} login\n")),
            "a fenced command line must be renamed: {out}"
        );
        assert!(
            out.contains(&format!("$ {program} mcp add x")),
            "a fenced command behind a shell prompt must be renamed: {out}"
        );
        assert!(
            out.contains(&format!("run `{program}`.")),
            "a bare backticked name is still an invocation: {out}"
        );
        assert!(
            out.contains(&format!("sign in with {program} login")),
            "the name need not be the span's first token — the binary prints \
             this exact sentence with the invoked name: {out}"
        );
        assert!(
            out.contains("```bash\n"),
            "the fence delimiter and its info string must survive: {out}"
        );
        assert!(
            // auth-instruction-guard: exempt
            out.contains("grok login in plain prose"),
            "outside a span or a fence it is prose, not a command: {out}"
        );
        assert!(
            out.contains("`grok-4.5`") && out.contains("`grok.com`"),
            "a model id and a domain are not commands: {out}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_guide_entries_are_valid() {
        for doc in USER_GUIDE {
            assert!(!doc.content.is_empty(), "Doc {} is empty", doc.filename);
            assert!(
                !doc.title.is_empty(),
                "Doc {} has empty title",
                doc.filename
            );
            assert!(
                !doc.description.is_empty(),
                "Doc {} has empty description",
                doc.filename
            );
            assert!(
                doc.content.starts_with('#'),
                "Doc {} should start with a markdown header",
                doc.filename
            );
        }
    }

    /// #122: the Codex preset's context bar total is a local default. The
    /// custom-models guide is where a user (and the model reading
    /// `<grok_home>/docs/`) looks when that number looks wrong — keep the
    /// metadata-only override discoverable there.
    #[test]
    fn custom_models_guide_documents_codex_context_window_override() {
        let doc = USER_GUIDE
            .iter()
            .find(|d| d.filename == "11-custom-models.md")
            .expect("custom-models guide must be in USER_GUIDE");
        let content = doc.content;
        assert!(
            content.contains("local default") && content.contains("context bar"),
            "custom-models guide must say the Codex context-bar total is a local default, \
             not a provider-reported figure"
        );
        // Every `400000` must carry an example marker on the same line or in
        // the sentence that follows. This page is written to `<grok_home>/docs/`
        // for the model to read, so an unqualified number is an instruction to
        // use that number -- and #122 is explicit that the real capacity must
        // not be guessed.
        //
        // Asserting the *disclaimers* rather than the override block: the block
        // and the value both predate this change (there was already a
        // `[model."gpt-5.6-sol"]` example tagged "(issue #122)"), so asserting
        // their presence guards nothing. And with several occurrences, a
        // whole-file `contains` is satisfied by any one survivor.
        for (idx, _) in content.match_indices("400000") {
            let line_start = content[..idx].rfind('\n').map_or(0, |n| n + 1);
            let window_end = content[idx..]
                .match_indices('\n')
                .nth(3)
                .map_or(content.len(), |(off, _)| idx + off);
            let window = &content[line_start..window_end];
            assert!(
                window.contains("example"),
                "every `400000` in the guide must be marked as an example; this one is not:\n{window}"
            );
        }
    }

    #[test]
    fn user_guide_entries_have_no_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for doc in USER_GUIDE {
            assert!(
                seen.insert(doc.filename),
                "Duplicate doc in list: {}",
                doc.filename
            );
        }
    }

    #[test]
    fn default_howto_entries_includes_all_user_guide_docs() {
        let entries = default_howto_entries();
        assert_eq!(entries.len(), USER_GUIDE.len() + REFERENCE_DOCS.len());
        for (i, doc) in USER_GUIDE.iter().enumerate() {
            assert_eq!(entries[i].title, doc.title, "Entry {} title mismatch", i);
        }
    }

    #[test]
    fn find_doc_is_case_insensitive() {
        let doc = find_doc("getting started").expect("should find Getting Started");
        assert_eq!(doc.title, "Getting Started");
        assert!(find_doc("nonexistent guide").is_none());
    }

    #[test]
    fn all_titles_covers_both_tables() {
        let titles: Vec<_> = all_titles().collect();
        assert_eq!(titles.len(), USER_GUIDE.len() + REFERENCE_DOCS.len());
    }

    #[test]
    fn get_howto_doc_delegates_to_find_doc() {
        assert!(get_howto_doc("Getting Started").is_some());
        assert!(get_howto_doc("Hooks & Plugins Guide").is_some());
        assert!(get_howto_doc("no such doc").is_none());
    }

    #[test]
    fn list_howto_titles_returns_all() {
        let titles = list_howto_titles();
        assert_eq!(titles.len(), USER_GUIDE.len() + REFERENCE_DOCS.len());
    }

    #[test]
    fn extract_writes_docs_and_cleans_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let docs_dir = tmp.path().join("docs").join("user-guide");

        std::fs::create_dir_all(&docs_dir).unwrap();
        std::fs::write(docs_dir.join("99-removed.md"), "stale").unwrap();
        std::fs::write(docs_dir.join("notes.md"), "user notes").unwrap();

        extract_user_guide_docs(tmp.path());

        let program = xai_grok_config::program_name::program_name();
        for doc in USER_GUIDE {
            let path = docs_dir.join(doc.filename);
            assert!(path.exists(), "Expected doc {} to exist", doc.filename);
            let on_disk = std::fs::read_to_string(&path).unwrap();

            // What lands on disk is what every other reader gets. Asserting
            // against `doc_content` rather than against the rewriter directly
            // is the point: the bug was the write path and the TUI path
            // disagreeing, and only this comparison can see that.
            assert_eq!(
                on_disk,
                doc_content(doc),
                "Content mismatch for {}",
                doc.filename
            );

            // Independent of the rewriter: whatever it did, nothing on disk may
            // still tell the model to run a program the user does not have.
            // Comparing against `rename_commands_to_invoked_program(...)` alone
            // -- which this test used to do -- passes with the function body
            // replaced by the identity.
            if program != "grok" {
                let left = on_disk
                    .lines()
                    .filter(|l| super::invoked_program_rename_tests::still_invokes_grok(l))
                    .collect::<Vec<_>>();
                assert!(
                    left.is_empty(),
                    "{} still ships {} `grok` command(s) on disk:\n{}",
                    doc.filename,
                    left.len(),
                    left.join("\n")
                );
            }
        }
        assert!(
            !docs_dir.join("99-removed.md").exists(),
            "Stale doc should be cleaned up"
        );
        assert!(
            docs_dir.join("notes.md").exists(),
            "User file should not be deleted"
        );
    }
}
