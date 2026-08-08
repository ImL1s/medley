use super::config::{self, Config};
use super::config_model_override_parse::{ConfigWarningKind, WarningTarget};
use crate::sampling::ApiBackend;
use regex::Regex;
use serial_test::serial;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use xai_grok_test_support::EnvGuard;

// Local check:
//   cargo test -p xai-grok-shell --lib docs_examples_ -- --nocapture

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkerKind {
    Toml,
    TomlNegative,
    ShellOffline,
}

#[derive(Debug, Clone)]
struct DocExample {
    id: String,
    kind: MarkerKind,
    language: String,
    code: String,
    source: PathBuf,
    marker_line: usize,
}

fn custom_models_doc_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../xai-grok-pager/docs/user-guide/11-custom-models.md")
}

fn root_readme_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../README.md")
}

fn negative_fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../tests/fixtures/docs_examples/negative-provider-examples.md")
}

fn parse_marker(line: &str) -> Option<(MarkerKind, String)> {
    let trimmed = line.trim();
    let payload = trimmed
        .strip_prefix("<!-- medley-doc-test:")?
        .strip_suffix("-->")?
        .trim();
    let (kind, id) = payload.split_once(':')?;
    let kind = match kind {
        "toml" => MarkerKind::Toml,
        "toml-negative" => MarkerKind::TomlNegative,
        "shell-offline" => MarkerKind::ShellOffline,
        _ => return None,
    };
    Some((kind, id.trim().to_owned()))
}

fn extract_marked_fences(markdown: &str, source: &Path) -> Result<Vec<DocExample>, String> {
    let lines: Vec<&str> = markdown.lines().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        if let Some((kind, id)) = parse_marker(lines[i]) {
            let marker_line = i + 1;
            let mut j = i + 1;
            while j < lines.len() && lines[j].trim().is_empty() {
                j += 1;
            }
            if j >= lines.len() || !lines[j].trim_start().starts_with("```") {
                return Err(format!(
                    "{}:{} marker '{}' must be followed by a fenced block",
                    source.display(),
                    marker_line,
                    id
                ));
            }
            let fence_header = lines[j].trim();
            let language = fence_header
                .strip_prefix("```")
                .unwrap_or_default()
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_owned();
            let mut body = Vec::new();
            let mut k = j + 1;
            while k < lines.len() {
                if lines[k].trim_start().starts_with("```") {
                    break;
                }
                body.push(lines[k]);
                k += 1;
            }
            if k >= lines.len() {
                return Err(format!(
                    "{}:{} marker '{}' has an unclosed fenced block",
                    source.display(),
                    marker_line,
                    id
                ));
            }
            out.push(DocExample {
                id,
                kind,
                language,
                code: body.join("\n"),
                source: source.to_path_buf(),
                marker_line,
            });
            i = k + 1;
            continue;
        }
        i += 1;
    }
    Ok(out)
}

fn expected_toml_ids() -> BTreeSet<&'static str> {
    [
        "models-default",
        "catalog-key-wire-slug-default-refs",
        "provider-anthropic-claude",
        "provider-openai-chat",
        "provider-openai-responses",
        "provider-openai-codex-secondary",
        "provider-gemini",
        "provider-openrouter",
        "provider-together",
        "provider-hosted-generic",
        "provider-local-ollama",
        "provider-local-lmstudio",
        "provider-local-llamacpp",
        "provider-local-vllm",
        "models-catalog-auth",
    ]
    .into_iter()
    .collect()
}

fn expected_shell_ids() -> BTreeSet<&'static str> {
    ["offline-list-models"].into_iter().collect()
}

fn unset_provider_env() -> Vec<EnvGuard> {
    vec![
        EnvGuard::unset("OPENAI_API_KEY"),
        EnvGuard::unset("ANTHROPIC_API_KEY"),
        EnvGuard::unset("GEMINI_API_KEY"),
        EnvGuard::unset("OPENROUTER_API_KEY"),
        EnvGuard::unset("TOGETHER_API_KEY"),
        EnvGuard::unset("PROVIDER_API_KEY"),
        EnvGuard::unset("XAI_API_KEY"),
        EnvGuard::unset("GROK_CODE_XAI_API_KEY"),
    ]
}

fn heading_slug(raw: &str) -> String {
    let mut slug = String::new();
    let mut prev_dash = false;
    for ch in raw.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            slug.push(lower);
            prev_dash = false;
            continue;
        }
        if lower == ' ' || lower == '-' {
            if !prev_dash && !slug.is_empty() {
                slug.push('-');
                prev_dash = true;
            }
            continue;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    slug
}

fn collect_headings(markdown: &str) -> BTreeSet<String> {
    markdown
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim_start();
            let hashes = trimmed.chars().take_while(|c| *c == '#').count();
            if !(1..=6).contains(&hashes) {
                return None;
            }
            let rest = trimmed[hashes..].trim();
            if rest.is_empty() {
                return None;
            }
            Some(heading_slug(rest))
        })
        .collect()
}

fn secret_and_credential_violations(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let secret_patterns = [
        Regex::new(r"sk-[A-Za-z0-9-]{20,}").expect("secret regex must compile"),
        Regex::new(r"xai-[A-Za-z0-9-]{20,}").expect("secret regex must compile"),
        Regex::new(r"ghp_[A-Za-z0-9]{20,}").expect("secret regex must compile"),
    ];
    for pat in &secret_patterns {
        if pat.is_match(text) {
            out.push(format!("realistic secret pattern '{}'", pat.as_str()));
        }
    }
    let url_pat = Regex::new(r#"https?://[^\s)>"']+"#).expect("credential URL regex must compile");
    for hit in url_pat.find_iter(text) {
        if let Ok(url) = url::Url::parse(hit.as_str()) {
            if !url.username().is_empty() || url.password().is_some() {
                out.push(format!("userinfo URL '{}'", hit.as_str()));
            }
            let has_credential_query = url.query_pairs().any(|(key, value)| {
                let key = key.to_ascii_lowercase();
                let credential_key = key.contains("key")
                    || key.contains("token")
                    || key.contains("secret")
                    || key.contains("password")
                    || key == "auth";
                credential_key && !value.trim().is_empty()
            });
            if has_credential_query {
                out.push(format!("credential query URL '{}'", hit.as_str()));
            }
        }
    }
    out
}

fn validate_offline_shell_block(code: &str) -> Result<(), String> {
    for (idx, line) in code.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed.starts_with("export ") {
            continue;
        }
        if trimmed == "grok" || trimmed.starts_with("grok ") {
            let mut parts = trimmed.split_whitespace();
            let _program = parts.next();
            if matches!(parts.next(), Some("login")) {
                return Err(format!(
                    "line {} must stay offline, found '{}'",
                    idx + 1,
                    trimmed
                ));
            }
            continue;
        }
        return Err(format!(
            "line {} uses unsupported offline command '{}'",
            idx + 1,
            trimmed
        ));
    }
    Ok(())
}

fn validate_markdown_links(doc_path: &Path, markdown: &str) -> Result<(), String> {
    let same_file_headings = collect_headings(markdown);
    let link_re = Regex::new(r#"\[[^\]]+\]\(([^)]+)\)"#).expect("link regex must compile");
    for caps in link_re.captures_iter(markdown) {
        let target = caps
            .get(1)
            .expect("capture 1 exists")
            .as_str()
            .trim()
            .to_owned();
        if target.starts_with("http://")
            || target.starts_with("https://")
            || target.starts_with("mailto:")
            || target.starts_with("file://")
        {
            continue;
        }
        if let Some(anchor) = target.strip_prefix('#') {
            if !same_file_headings.contains(anchor) {
                return Err(format!(
                    "{}: missing heading anchor '#{}'",
                    doc_path.display(),
                    anchor
                ));
            }
            continue;
        }
        let (file_target, anchor) = match target.split_once('#') {
            Some((path, anchor)) => (path, Some(anchor)),
            None => (target.as_str(), None),
        };
        let resolved = doc_path
            .parent()
            .expect("doc file has parent")
            .join(file_target);
        if !resolved.exists() {
            return Err(format!(
                "{}: missing linked file '{}'",
                doc_path.display(),
                resolved.display()
            ));
        }
        if let Some(anchor) = anchor {
            let linked_markdown = std::fs::read_to_string(&resolved).map_err(|error| {
                format!(
                    "{}: failed reading linked file '{}' for anchor check: {error}",
                    doc_path.display(),
                    resolved.display()
                )
            })?;
            let headings = collect_headings(&linked_markdown);
            if !headings.contains(anchor) {
                return Err(format!(
                    "{}: linked heading '#{}' not found in '{}'",
                    doc_path.display(),
                    anchor,
                    resolved.display()
                ));
            }
        }
    }
    Ok(())
}

fn extract_language_fences(markdown: &str, language: &str) -> Vec<(usize, String)> {
    let lines: Vec<&str> = markdown.lines().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        if let Some(rest) = trimmed.strip_prefix("```") {
            let fence_lang = rest.split_whitespace().next().unwrap_or_default();
            if fence_lang == language {
                let fence_line = i + 1;
                let mut body = Vec::new();
                let mut j = i + 1;
                while j < lines.len() {
                    if lines[j].trim_start().starts_with("```") {
                        break;
                    }
                    body.push(lines[j]);
                    j += 1;
                }
                if j < lines.len() {
                    out.push((fence_line, body.join("\n")));
                    i = j + 1;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

#[test]
fn docs_examples_extractor_handles_markers_ignores_unmarked_and_rejects_malformed() {
    let source = Path::new("inline.md");
    let markdown = r#"
<!-- medley-doc-test:toml:one -->
```toml
[model.one]
model = "one"
base_url = "https://api.example.com/v1"
context_window = 200000
```

```toml
[model.ignored]
model = "ignored"
base_url = "https://api.example.com/v1"
context_window = 200000
```
"#;
    let parsed = extract_marked_fences(markdown, source).expect("valid marker parse");
    assert_eq!(parsed.len(), 1, "only marked fences are executable");
    assert_eq!(parsed[0].id, "one");
    assert_eq!(parsed[0].language, "toml");

    let malformed = r#"
<!-- medley-doc-test:toml:broken -->
[model.not-a-fence]
model = "broken"
"#;
    assert!(
        extract_marked_fences(malformed, source).is_err(),
        "malformed markers must fail loud so coverage cannot silently skip"
    );

    let unclosed = r#"
<!-- medley-doc-test:toml:unclosed -->
```toml
[model.unclosed]
model = "x"
"#;
    assert!(
        extract_marked_fences(unclosed, source).is_err(),
        "unclosed fenced blocks must fail extraction"
    );
}

#[test]
#[serial]
fn docs_examples_parse_and_resolve_provider_toml_examples() {
    let _env = unset_provider_env();
    let doc_path = custom_models_doc_path();
    let markdown = std::fs::read_to_string(&doc_path).expect("read custom-model docs");
    let examples = extract_marked_fences(&markdown, &doc_path).expect("extract docs examples");

    let toml_examples: Vec<&DocExample> = examples
        .iter()
        .filter(|ex| ex.kind == MarkerKind::Toml)
        .collect();
    let ids: BTreeSet<&str> = toml_examples.iter().map(|ex| ex.id.as_str()).collect();
    assert_eq!(
        ids,
        expected_toml_ids(),
        "canonical TOML example ids drifted; update markers and expectations together"
    );

    for ex in toml_examples {
        let raw = toml::from_str::<toml::Value>(&ex.code).unwrap_or_else(|error| {
            panic!(
                "{}:{} id='{}' TOML parse failed: {error}",
                ex.source.display(),
                ex.marker_line,
                ex.id
            )
        });
        let cfg = Config::new_from_toml_cfg(&raw).unwrap_or_else(|error| {
            panic!(
                "{}:{} id='{}' production config parse failed: {error}",
                ex.source.display(),
                ex.marker_line,
                ex.id
            )
        });
        let catalog = config::resolve_model_list(&cfg, None);
        match ex.id.as_str() {
            "models-default" => {
                assert_eq!(cfg.models.default.as_deref(), Some("grok-4.5"));
                assert!(
                    catalog.contains_key("grok-4.5"),
                    "default model id must resolve in the catalog"
                );
            }
            "catalog-key-wire-slug-default-refs" => {
                assert!(catalog.contains_key("prod-grok-build"));
                assert!(catalog.contains_key("canary-grok-build"));
                assert_eq!(catalog["prod-grok-build"].info.model, "grok-4.5");
                assert_eq!(catalog["canary-grok-build"].info.model, "grok-4.5");
                assert_eq!(cfg.models.default.as_deref(), Some("canary-grok-build"));
                assert_eq!(cfg.models.web_search.as_deref(), Some("prod-grok-build"));
                assert_eq!(
                    cfg.models.session_summary.as_deref(),
                    Some("canary-grok-build")
                );
            }
            "provider-anthropic-claude" => assert!(catalog.contains_key("claude-opus")),
            "provider-openai-chat" => assert!(catalog.contains_key("gpt-4o")),
            "provider-openai-responses" => assert!(catalog.contains_key("gpt-4o-responses")),
            "provider-openai-codex-secondary" => {
                assert!(catalog.contains_key("my-other-codex-model"))
            }
            "provider-gemini" => assert!(catalog.contains_key("gemini-flash")),
            "provider-openrouter" => assert!(catalog.contains_key("openrouter-llama")),
            "provider-together" => assert!(catalog.contains_key("together-mixtral")),
            "provider-hosted-generic" => assert!(catalog.contains_key("hosted-custom")),
            "provider-local-ollama" => assert!(catalog.contains_key("ollama-codellama")),
            "provider-local-lmstudio" => assert!(catalog.contains_key("lmstudio-local")),
            "provider-local-llamacpp" => assert!(catalog.contains_key("llamacpp")),
            "provider-local-vllm" => assert!(catalog.contains_key("vllm-local")),
            "models-catalog-auth" => {
                let auth_cfg = cfg.models.catalog_auth_config().unwrap();
                assert_eq!(auth_cfg.endpoint.as_deref(), Some("https://api.acme.com/v1/models"));
                assert_eq!(auth_cfg.auth_scheme, Some(xai_grok_sampler::AuthScheme::Bearer));
                assert_eq!(auth_cfg.timeout_secs, Some(15));
                assert_eq!(auth_cfg.headers.get("X-Organization").map(|s| s.as_str()), Some("Acme"));
            }
            other => panic!("unhandled canonical TOML id '{other}'"),
        }
    }
}

#[test]
#[serial]
fn docs_examples_enforce_readiness_and_secret_policy() {
    let _env = unset_provider_env();
    let doc_path = custom_models_doc_path();
    let markdown = std::fs::read_to_string(&doc_path).expect("read custom-model docs");
    let examples = extract_marked_fences(&markdown, &doc_path).expect("extract docs examples");

    for ex in examples
        .iter()
        .filter(|ex| ex.kind == MarkerKind::Toml || ex.kind == MarkerKind::ShellOffline)
    {
        let violations = secret_and_credential_violations(&ex.code);
        assert!(
            violations.is_empty(),
            "{}:{} id='{}' has credential-like literals: {:?}",
            ex.source.display(),
            ex.marker_line,
            ex.id,
            violations
        );
    }

    let by_id = |id: &str| -> &DocExample {
        examples
            .iter()
            .find(|ex| ex.id == id)
            .unwrap_or_else(|| panic!("missing marker id '{id}'"))
    };

    let openai_cfg = Config::new_from_toml_cfg(
        &toml::from_str::<toml::Value>(&by_id("provider-openai-chat").code)
            .expect("provider-openai-chat TOML"),
    )
    .expect("provider-openai-chat config parse");
    let openai_catalog = config::resolve_model_list(&openai_cfg, None);
    let openai_entry = openai_catalog.get("gpt-4o").expect("gpt-4o in catalog");
    let (openai_ready, openai_reason) = config::model_readiness(openai_entry);
    assert!(!openai_ready, "missing OPENAI_API_KEY must be unready");
    assert!(
        openai_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("OPENAI_API_KEY")),
        "unexpected OpenAI readiness reason: {openai_reason:?}"
    );
    assert_eq!(
        openai_entry.info.auth_scheme,
        xai_grok_sampler::AuthScheme::Bearer
    );

    let anthropic_cfg = Config::new_from_toml_cfg(
        &toml::from_str::<toml::Value>(&by_id("provider-anthropic-claude").code)
            .expect("provider-anthropic-claude TOML"),
    )
    .expect("provider-anthropic-claude config parse");
    let anthropic_catalog = config::resolve_model_list(&anthropic_cfg, None);
    let anthropic_entry = anthropic_catalog
        .get("claude-opus")
        .expect("claude-opus in catalog");
    let (anthropic_ready, anthropic_reason) = config::model_readiness(anthropic_entry);
    assert!(
        !anthropic_ready,
        "missing ANTHROPIC_API_KEY must be unready"
    );
    assert!(
        anthropic_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("ANTHROPIC_API_KEY")),
        "unexpected Anthropic readiness reason: {anthropic_reason:?}"
    );
    assert_eq!(
        anthropic_entry.info.auth_scheme,
        xai_grok_sampler::AuthScheme::XApiKey
    );
    assert_eq!(anthropic_entry.info.api_backend, ApiBackend::Messages);

    let local_cfg = Config::new_from_toml_cfg(
        &toml::from_str::<toml::Value>(&by_id("provider-local-ollama").code)
            .expect("provider-local-ollama TOML"),
    )
    .expect("provider-local-ollama config parse");
    let local_catalog = config::resolve_model_list(&local_cfg, None);
    let local_entry = local_catalog
        .get("ollama-codellama")
        .expect("ollama-codellama in catalog");
    let (local_ready, local_reason) = config::model_readiness(local_entry);
    assert!(
        local_ready,
        "auth_scheme=none must be ready for local servers"
    );
    assert_eq!(local_reason, None);
    assert_eq!(
        local_entry.info.auth_scheme,
        xai_grok_sampler::AuthScheme::None
    );

    let fixture_path = negative_fixture_path();
    let fixture_markdown =
        std::fs::read_to_string(&fixture_path).expect("read negative docs fixture markdown");
    let negatives =
        extract_marked_fences(&fixture_markdown, &fixture_path).expect("extract negative examples");
    let negative_ids: BTreeSet<&str> = negatives
        .iter()
        .filter(|ex| ex.kind == MarkerKind::TomlNegative)
        .map(|ex| ex.id.as_str())
        .collect();
    let expected_negative: BTreeSet<&str> = [
        "legacy-local-missing-auth-scheme",
        "invalid-env-key-warning",
        "unsafe-literal-secret-and-userinfo",
    ]
    .into_iter()
    .collect();
    assert_eq!(negative_ids, expected_negative);

    for ex in negatives
        .iter()
        .filter(|ex| ex.kind == MarkerKind::TomlNegative)
    {
        let raw = toml::from_str::<toml::Value>(&ex.code).expect("negative fixture TOML parse");
        let cfg = Config::new_from_toml_cfg(&raw).expect("negative fixture config parse");
        match ex.id.as_str() {
            "legacy-local-missing-auth-scheme" => {
                let catalog = config::resolve_model_list(&cfg, None);
                let entry = catalog
                    .get("legacy-ollama")
                    .expect("legacy local model must still parse into catalog");
                let (ready, reason) = config::model_readiness(entry);
                assert!(
                    !ready,
                    "legacy local model without auth_scheme must be unready"
                );
                assert!(
                    reason.is_some(),
                    "legacy local model should explain why it's unready"
                );
            }
            "invalid-env-key-warning" => {
                let has_path_specific_warning = cfg.config_warnings.iter().any(|warning| {
                    warning.kind == ConfigWarningKind::InvalidValue
                        && matches!(
                            &warning.target,
                            WarningTarget::Model { key, field: Some(field) }
                                if key == "bad-env-key" && field == "env_key"
                        )
                });
                assert!(
                    has_path_specific_warning,
                    "invalid env_key fixture must emit a path-specific warning"
                );
            }
            "unsafe-literal-secret-and-userinfo" => {
                let violations = secret_and_credential_violations(&ex.code);
                assert!(
                    violations
                        .iter()
                        .any(|v| v.contains("realistic secret pattern")),
                    "negative fixture must contain a realistic secret pattern: {violations:?}"
                );
                assert!(
                    violations.iter().any(|v| v.contains("userinfo URL")),
                    "negative fixture must contain a userinfo URL: {violations:?}"
                );
            }
            other => panic!("unhandled negative fixture id '{other}'"),
        }
    }
}

#[test]
fn docs_examples_validate_offline_commands_and_links() {
    let doc_path = custom_models_doc_path();
    let markdown = std::fs::read_to_string(&doc_path).expect("read custom-model docs");
    let examples = extract_marked_fences(&markdown, &doc_path).expect("extract docs examples");

    let shell_examples: Vec<&DocExample> = examples
        .iter()
        .filter(|ex| ex.kind == MarkerKind::ShellOffline)
        .collect();
    let shell_ids: BTreeSet<&str> = shell_examples.iter().map(|ex| ex.id.as_str()).collect();
    assert_eq!(
        shell_ids,
        expected_shell_ids(),
        "offline shell marker ids drifted; update markers and expectations together"
    );
    for ex in shell_examples {
        validate_offline_shell_block(&ex.code).unwrap_or_else(|error| {
            panic!(
                "{}:{} id='{}' offline shell validation failed: {error}",
                ex.source.display(),
                ex.marker_line,
                ex.id
            )
        });
    }

    validate_markdown_links(&doc_path, &markdown).unwrap_or_else(|error| panic!("{error}"));

    let readme_path = root_readme_path();
    let readme = std::fs::read_to_string(&readme_path).expect("read root README");
    validate_markdown_links(&readme_path, &readme).unwrap_or_else(|error| panic!("{error}"));

    let readme_violations = secret_and_credential_violations(&readme);
    assert!(
        readme_violations.is_empty(),
        "{} has credential-like literals: {:?}",
        readme_path.display(),
        readme_violations
    );

    for (line, block) in extract_language_fences(&readme, "toml") {
        let raw = toml::from_str::<toml::Value>(&block).unwrap_or_else(|error| {
            panic!(
                "{}:{} README TOML fence parse failed: {error}",
                readme_path.display(),
                line
            )
        });
        Config::new_from_toml_cfg(&raw).unwrap_or_else(|error| {
            panic!(
                "{}:{} README TOML fence production parse failed: {error}",
                readme_path.display(),
                line
            )
        });
    }
}
