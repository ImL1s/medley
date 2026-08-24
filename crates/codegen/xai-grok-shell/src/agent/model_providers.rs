use indexmap::IndexMap;
use reqwest::header::HeaderValue;

use super::config::{CodexCatalogUpgrade, ConfigModelOverride, EnvKeys};
use super::config_model_override_parse::{ConfigWarning, ConfigWarningKind};
use crate::sampling::ApiBackend;

/// Reserved first-party provider profile for ChatGPT Codex OAuth traffic.
pub const OPENAI_CODEX_PROVIDER_ID: &str = "openai-codex";

/// Catalog key and routing slug of the built-in Codex preset. The two are the
/// same string so a user's `[model."gpt-5.6-sol"]` in the global config
/// replaces the preset in place instead of adding a second entry.
pub const OPENAI_CODEX_PRESET_MODEL_ID: &str = "gpt-5.6-sol";

/// Fallback context window for the built-in Codex preset when no live catalog
/// or last-good cache is available.
///
/// Matches Sol's published `context_window` / `max_context_window` (272_000).
/// The previous 200_000 guess made the context bar look like capacity and
/// fired auto-compact far too early (#122). A live `GET /models` payload, or a
/// metadata-only `[model."gpt-5.6-sol"]` override, still wins when present.
///
/// A successful account-scoped catalog refresh therefore replaces this figure
/// with the server's own `context_window` metadata; it is only load-bearing
/// when neither a live nor a saved catalog exists for the account.
const OPENAI_CODEX_PRESET_CONTEXT_WINDOW: u64 = 272_000;
const OPENAI_CODEX_MODELS_CATALOG_DESCRIPTION: &str = "OpenAI Codex via a ChatGPT subscription";
const OPENAI_CODEX_CATALOG_CACHE_DIR: &str = "codex-model-catalog";
const OPENAI_CODEX_CATALOG_CACHE_SCHEMA: u32 = 1;
pub(crate) const OPENAI_CODEX_CATALOG_DEGRADED_MARKER: &str = "Catalog degraded: ";
const OPENAI_CODEX_SAVED_CATALOG_REASON: &str =
    "live refresh failed; using the last saved catalog for this account";
const OPENAI_CODEX_BUILTIN_FALLBACK_REASON: &str =
    "live refresh failed and no saved catalog exists for this account; using the built-in fallback";

#[derive(serde::Serialize, serde::Deserialize)]
struct CodexCatalogCache {
    schema: u32,
    payload: serde_json::Value,
}

struct CodexCatalogCredential {
    access_token: String,
    account_id: Option<String>,
    chatgpt_account_is_fedramp: bool,
    source: xai_grok_sampler::CredentialSource,
}

struct CodexCatalogCacheIdentity {
    account_id: Option<String>,
    chatgpt_account_is_fedramp: bool,
}

fn codex_catalog_cache_path(
    home: &std::path::Path,
    identity: &CodexCatalogCacheIdentity,
) -> Option<std::path::PathBuf> {
    let account_id = identity.account_id.as_deref()?;
    // The filename is stable for one account and origin, but never exposes the
    // raw account id. FedRAMP is part of the identity boundary as well: the
    // same account label must not bridge those two catalog authorities.
    let identity = format!(
        "v1\0{}\0{}\0{}",
        crate::auth::openai_codex::CODEX_API_BASE_URL,
        identity.chatgpt_account_is_fedramp,
        account_id
    );
    let key = blake3::hash(identity.as_bytes()).to_hex();
    Some(
        home.join(OPENAI_CODEX_CATALOG_CACHE_DIR)
            .join(format!("{key}.json")),
    )
}

fn load_codex_catalog_cache(
    path: &std::path::Path,
) -> Option<IndexMap<String, ConfigModelOverride>> {
    let bytes = std::fs::read(path).ok()?;
    let cache: CodexCatalogCache = serde_json::from_slice(&bytes).ok()?;
    if cache.schema != OPENAI_CODEX_CATALOG_CACHE_SCHEMA {
        tracing::debug!(path = %path.display(), "Codex catalog cache schema mismatch");
        return None;
    }
    let models = parse_openai_codex_catalog_models(&cache.payload);
    (!models.is_empty()).then_some(models)
}

fn persist_codex_catalog_cache(path: &std::path::Path, payload: &serde_json::Value) {
    let cache = CodexCatalogCache {
        schema: OPENAI_CODEX_CATALOG_CACHE_SCHEMA,
        payload: payload.clone(),
    };
    let Ok(bytes) = serde_json::to_vec_pretty(&cache) else {
        tracing::warn!("Codex catalog cache serialization failed");
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    #[cfg(unix)]
    let cache_directory_created = !parent.exists();
    if let Err(error) = std::fs::create_dir_all(parent) {
        tracing::warn!(error = %error, "Codex catalog cache directory creation failed");
        return;
    }
    #[cfg(unix)]
    if cache_directory_created
        && let Some(home) = parent.parent()
        && let Err(error) = std::fs::File::open(home).and_then(|directory| directory.sync_all())
    {
        tracing::warn!(error = %error, "Codex catalog cache parent directory sync failed");
    }
    static CACHE_WRITE_SEQUENCE: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);
    let sequence = CACHE_WRITE_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("json.tmp.{}.{sequence}", std::process::id()));
    if let Err(error) = write_codex_catalog_cache_tmp(&tmp, &bytes) {
        tracing::warn!(error = %error, "Codex catalog cache write failed");
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    if let Err(error) = replace_codex_catalog_cache(&tmp, path) {
        tracing::warn!(error = %error, "Codex catalog cache publish failed");
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    #[cfg(unix)]
    if let Err(error) = std::fs::File::open(parent).and_then(|directory| directory.sync_all()) {
        tracing::warn!(error = %error, "Codex catalog cache directory sync failed");
    }
}

fn write_codex_catalog_cache_tmp(tmp: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;

    let open = || {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        options.open(tmp)
    };
    let mut file = match open() {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            std::fs::remove_file(tmp)?;
            open()?
        }
        Err(error) => return Err(error),
    };
    file.write_all(bytes)?;
    file.sync_all()
}

fn replace_codex_catalog_cache(
    tmp: &std::path::Path,
    path: &std::path::Path,
) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        if !path.exists() {
            return std::fs::rename(tmp, path);
        }

        use std::iter::once;
        use std::os::windows::ffi::OsStrExt as _;
        use windows::Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        };
        use windows::core::PCWSTR;

        let from: Vec<u16> = tmp.as_os_str().encode_wide().chain(once(0)).collect();
        let to: Vec<u16> = path.as_os_str().encode_wide().chain(once(0)).collect();
        unsafe {
            MoveFileExW(
                PCWSTR::from_raw(from.as_ptr()),
                PCWSTR::from_raw(to.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        }
        .map_err(std::io::Error::other)?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(tmp, path)
    }
}

fn mark_codex_catalog_degraded(
    mut models: IndexMap<String, ConfigModelOverride>,
    reason: &str,
) -> IndexMap<String, ConfigModelOverride> {
    for model in models.values_mut() {
        model.catalog_degraded_reason = Some(reason.to_owned());
    }
    models
}

/// A Codex preset map plus whether the *listing* it came from is one the
/// server actually enumerated.
///
/// This deliberately does not read `catalog_degraded_reason`, because that
/// field has two writers meaning two different things.
/// [`mark_codex_catalog_degraded`] stamps **every** row because the listing
/// itself is a stand-in; [`stamp_codex_catalog_client_version_floor`] stamps
/// **one** row because that model wants a newer client. Only the first says
/// anything about whether the listing enumerates every wire slug the account
/// has, and deriving authority from the field conflated them: a single
/// above-floor row made a successfully fetched catalog non-authoritative,
/// which cleared every `unknown_codex_catalog_slug` marker and let a wire
/// model the server does not serve pass readiness and 400 on every turn.
struct CodexCatalogListing {
    models: IndexMap<String, ConfigModelOverride>,
    /// True only when the slug set here is the account's real slug set, so an
    /// absent slug means the server does not serve it.
    enumerates_account_slugs: bool,
}

impl CodexCatalogListing {
    /// A listing the server enumerated — live, or a saved one standing in for
    /// a live fetch that was never attempted because remote fetch is off.
    fn served(models: IndexMap<String, ConfigModelOverride>) -> Self {
        Self {
            models,
            enumerates_account_slugs: true,
        }
    }

    /// A stand-in listing. Its slug set is not the account's, so an absent
    /// slug proves nothing and no entry may be rejected for missing from it.
    fn stand_in(models: IndexMap<String, ConfigModelOverride>) -> Self {
        Self {
            models,
            enumerates_account_slugs: false,
        }
    }
}

fn codex_catalog_fallback_models(
    cache_path: Option<&std::path::Path>,
) -> IndexMap<String, ConfigModelOverride> {
    if let Some(models) = cache_path.and_then(load_codex_catalog_cache) {
        tracing::warn!(
            count = models.len(),
            "Codex catalog live refresh failed; using account-scoped last-good cache"
        );
        return mark_codex_catalog_degraded(models, OPENAI_CODEX_SAVED_CATALOG_REASON);
    }
    tracing::warn!("Codex catalog live refresh failed; using visible built-in fallback");
    mark_codex_catalog_degraded(
        openai_codex_preset_models(),
        OPENAI_CODEX_BUILTIN_FALLBACK_REASON,
    )
}

/// `served`, not `stand_in`: this is a catalog the server did enumerate, kept
/// because remote fetch is switched off rather than because a fetch failed.
/// It is also what the previous `catalog_degraded_reason`-derived authority
/// treated it as — these models are returned unmarked — so keeping it served
/// leaves that path's behaviour unchanged.
fn load_codex_catalog_when_remote_fetch_disabled(
    cache_path: Option<&std::path::Path>,
) -> Option<CodexCatalogListing> {
    let models = cache_path.and_then(load_codex_catalog_cache)?;
    tracing::info!(
        count = models.len(),
        "Codex catalog live refresh skipped; using account-scoped saved catalog"
    );
    Some(CodexCatalogListing::served(models))
}

impl std::fmt::Debug for CodexCatalogCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexCatalogCredential")
            .field("access_token_present", &!self.access_token.is_empty())
            .field("account_id_present", &self.account_id.is_some())
            .field(
                "chatgpt_account_is_fedramp",
                &self.chatgpt_account_is_fedramp,
            )
            .field("source", &self.source)
            .finish()
    }
}

fn codex_catalog_credential_source_is_valid(source: &xai_grok_sampler::CredentialSource) -> bool {
    matches!(
        source,
        xai_grok_sampler::CredentialSource::AuthProvider { name }
            if name == OPENAI_CODEX_PROVIDER_ID
    )
}

fn codex_catalog_credential_from_snapshot(
    snapshot: xai_grok_sampler::config::ProviderCredentialSnapshot,
) -> Option<CodexCatalogCredential> {
    let access_token = snapshot.access_token.trim();
    if access_token.is_empty() {
        return None;
    }
    let account_id = snapshot
        .account_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned);
    Some(CodexCatalogCredential {
        access_token: access_token.to_owned(),
        account_id,
        chatgpt_account_is_fedramp: snapshot.chatgpt_account_is_fedramp,
        source: xai_grok_sampler::CredentialSource::AuthProvider {
            name: OPENAI_CODEX_PROVIDER_ID.to_owned(),
        },
    })
}

fn codex_catalog_access_from_manager(
    manager: &crate::auth::AuthManager,
) -> Option<(CodexCatalogCacheIdentity, Option<CodexCatalogCredential>)> {
    // Retain the verified account boundary even when the bearer has entered
    // the refresh window or expired. It is safe for cache lookup only; the
    // live request below still requires credential_snapshot/current().
    let retained = manager.current_or_expired()?;
    let identity = CodexCatalogCacheIdentity {
        account_id: retained
            .account_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_owned),
        chatgpt_account_is_fedramp: retained.chatgpt_account_is_fedramp,
    };
    let live = crate::auth::openai_codex::credential_snapshot(manager)
        .and_then(codex_catalog_credential_from_snapshot);
    Some((identity, live))
}

fn codex_catalog_access(
    home: &std::path::Path,
) -> Option<(CodexCatalogCacheIdentity, Option<CodexCatalogCredential>)> {
    let manager = crate::auth::AuthManager::new_openai_codex(home);
    codex_catalog_access_from_manager(&manager)
}

/// `GET /models` rejects a request without this query parameter — HTTP 400,
/// `{"loc": ("query", "client_version"), "msg": "Field required"}` — so leaving
/// it off makes the fetch fail every time and fall silently back to the
/// built-in preset, which looks exactly like having no catalog at all.
///
/// `0.0.0` is what this fork's Codex user-agent already claims
/// (`codex_cli_rs/0.0.0`), and the endpoint accepts it. Verified against the
/// live endpoint on 2026-08-08: HTTP 200, nine models. Entries (and a
/// catalog-wide field) may carry `minimal_client_version`. When that floor is
/// above this advertised version, parse stamps `catalog_degraded_reason` so
/// the picker/ACP can show it. A rejected fetch still falls back to the
/// last-good or built-in catalog via the existing degraded path.
const OPENAI_CODEX_CATALOG_CLIENT_VERSION: &str = "0.0.0";

fn parse_codex_catalog_semver(raw: &str) -> Option<semver::Version> {
    semver::Version::parse(raw.trim()).ok()
}

fn advertised_codex_catalog_client_version() -> Option<semver::Version> {
    let parsed = parse_codex_catalog_semver(OPENAI_CODEX_CATALOG_CLIENT_VERSION);
    if parsed.is_none() {
        tracing::warn!(
            advertised = OPENAI_CODEX_CATALOG_CLIENT_VERSION,
            "Codex catalog advertised client_version is not valid semver"
        );
    }
    parsed
}

/// True when `floor` is a higher semver than this client's advertised catalog
/// version. Unparseable floors are ignored so a bad payload cannot mark the
/// whole catalog degraded.
fn codex_catalog_client_is_below_floor(floor: &str) -> bool {
    match (
        advertised_codex_catalog_client_version(),
        parse_codex_catalog_semver(floor),
    ) {
        (Some(advertised), Some(required)) => required > advertised,
        (Some(_), None) => {
            tracing::warn!(
                floor = %floor,
                "ignoring unparseable Codex catalog minimal_client_version"
            );
            false
        }
        (None, _) => false,
    }
}

pub(crate) fn openai_codex_catalog_client_version_floor_reason(floor: &str) -> String {
    format!(
        "this client advertises catalog version {}; the server requires {floor}",
        OPENAI_CODEX_CATALOG_CLIENT_VERSION
    )
}

/// Stamp a version-floor degraded reason. Returns whether this call newly
/// attached one. Does not overwrite an existing operational reason.
fn stamp_codex_catalog_client_version_floor(model: &mut ConfigModelOverride, floor: &str) -> bool {
    if model.catalog_degraded_reason.is_some() || !codex_catalog_client_is_below_floor(floor) {
        return false;
    }
    model.catalog_degraded_reason = Some(openai_codex_catalog_client_version_floor_reason(floor));
    true
}

fn apply_codex_catalog_auth_headers(
    request: reqwest::blocking::RequestBuilder,
    credential: &CodexCatalogCredential,
) -> Option<reqwest::blocking::RequestBuilder> {
    if !codex_catalog_credential_source_is_valid(&credential.source) {
        tracing::warn!("Codex catalog fetch refused: invalid credential source label");
        return None;
    }
    let mut request = request
        .header(
            "Authorization",
            format!("Bearer {}", credential.access_token),
        )
        .header("originator", crate::auth::openai_codex::ORIGINATOR);
    if let Some(account_id) = credential.account_id.as_deref() {
        match HeaderValue::from_str(account_id) {
            Ok(value) => request = request.header("chatgpt-account-id", value),
            Err(_) => {
                tracing::warn!(
                    "Codex catalog fetch: skipped invalid chatgpt-account-id header value"
                );
            }
        }
    }
    if credential.chatgpt_account_is_fedramp {
        request = request.header("x-openai-fedramp", "true");
    }
    Some(request)
}

fn codex_catalog_string(
    obj: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<String> {
    obj.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn codex_catalog_minimal_client_version(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Option<String> {
    codex_catalog_string(obj, "minimal_client_version")
        .or_else(|| codex_catalog_string(obj, "minimalClientVersion"))
}

/// Top-level catalog publisher version. Parsed so it is not ignored, but it
/// is **not** a client floor: the live payload reports `client_version`
/// `0.147.0` while still accepting advertised `0.0.0`. The floor is
/// `minimal_client_version` only.
fn codex_catalog_payload_client_version(payload: &serde_json::Value) -> Option<String> {
    payload.as_object().and_then(|obj| {
        codex_catalog_string(obj, "client_version")
            .or_else(|| codex_catalog_string(obj, "clientVersion"))
    })
}

/// Live-shape `upgrade` object: `{ "model": "...", "migration_markdown": "..." }`.
/// Missing `model` drops the advisory. Missing markdown still attaches the
/// target so the picker can name the replacement.
fn codex_catalog_upgrade(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Option<CodexCatalogUpgrade> {
    let raw = obj.get("upgrade")?;
    if raw.is_null() {
        return None;
    }
    let Some(upgrade) = raw.as_object() else {
        tracing::warn!(value = %raw, "Codex catalog upgrade was not an object");
        return None;
    };
    let Some(model) = codex_catalog_string(upgrade, "model") else {
        tracing::warn!("Codex catalog upgrade omitted model");
        return None;
    };
    Some(CodexCatalogUpgrade {
        model,
        migration_markdown: codex_catalog_string(upgrade, "migration_markdown")
            .or_else(|| codex_catalog_string(upgrade, "migrationMarkdown"))
            .unwrap_or_default(),
    })
}

/// Keys tried in order when reading a Codex catalog entry's session budget.
///
/// `max_context_window` is the operative token capacity and is first.
/// `context_window` is the billing/pricing threshold, not capacity: on
/// `gpt-5.4` that is 272_000 against a 1_000_000 operative window. CamelCase
/// aliases are tolerances for a shape this endpoint does not currently return.
///
/// This array *is* the selection order. A test asserts `max_context_window`
/// precedes `context_window` so inverting the fields fails without having to
/// rewrite the lookup.
const CODEX_CATALOG_CONTEXT_WINDOW_KEYS: &[&str] = &[
    "max_context_window",
    "maxContextWindow",
    "context_window",
    "contextWindow",
];

/// The window a session is budgeted against.
///
/// On the live Codex catalog, `max_context_window` is the operative token
/// capacity and is tried first. `context_window` is the billing/pricing
/// threshold, not capacity. Measured 2026-08-08: eight of nine models report
/// the two fields equal, and `gpt-5.4` reports `context_window: 272000`
/// against `max_context_window: 1000000`. Budgeting the pricing field — which
/// #258 did — fires auto-compact at ~27% of the tokens the model accepts.
///
/// `context_window` stays as a fallback for an entry that reports only the
/// pricing threshold. No window at all falls back to the preset constant.
fn codex_catalog_context_window(obj: &serde_json::Map<String, serde_json::Value>) -> Option<u64> {
    CODEX_CATALOG_CONTEXT_WINDOW_KEYS
        .iter()
        .find_map(|key| obj.get(*key).and_then(serde_json::Value::as_u64))
        .filter(|value| *value > 0)
}

fn codex_catalog_bool(obj: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<bool> {
    obj.get(key).and_then(serde_json::Value::as_bool)
}

/// Catalog auto-compact calibration (0-100). Out-of-range values are ignored
/// so a bad payload cannot disable compaction or overflow the resolver.
fn codex_catalog_effective_context_window_percent(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Option<u8> {
    let value = obj
        .get("effective_context_window_percent")
        .or_else(|| obj.get("effectiveContextWindowPercent"))?;
    let raw = value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|n| u64::try_from(n).ok()))?;
    if raw > 100 {
        tracing::warn!(
            value = raw,
            "ignoring out-of-range Codex catalog effective_context_window_percent"
        );
        return None;
    }
    Some(raw as u8)
}

fn codex_catalog_string_list(
    obj: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Vec<String> {
    obj.get(key)
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Parse the catalog's model-specific reasoning menu. The observed endpoint
/// shape is an array of `{ "effort": "...", "description": "..." }`
/// objects, matching the reference Codex client's `ReasoningEffortPreset`.
/// Unknown future effort tiers are skipped rather than widened into values our
/// wire enum cannot represent.
fn codex_catalog_reasoning_efforts(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Option<Vec<xai_grok_sampling_types::ReasoningEffortOption>> {
    let raw = obj.get("supported_reasoning_levels")?;
    let Some(levels) = raw.as_array() else {
        tracing::warn!(value = %raw, "Codex catalog supported_reasoning_levels was not an array");
        return Some(Vec::new());
    };
    Some(
        levels
            .iter()
            .filter_map(|level| {
                let Some(level) = level.as_object() else {
                    tracing::warn!(value = %level, "Codex catalog reasoning level was not an object");
                    return None;
                };
                let Some(effort) = codex_catalog_string(level, "effort") else {
                    tracing::warn!("Codex catalog reasoning level omitted effort");
                    return None;
                };
                let Ok(value) = effort.parse::<xai_grok_sampling_types::ReasoningEffort>() else {
                    tracing::warn!(effort = %effort, "Codex catalog reasoning level is not supported by this client");
                    return None;
                };
                Some(xai_grok_sampling_types::ReasoningEffortOption {
                    id: value.as_str().to_owned(),
                    value,
                    label: match value {
                        xai_grok_sampling_types::ReasoningEffort::Xhigh => "Xhigh".to_owned(),
                        _ => {
                            let mut chars = value.as_str().chars();
                            chars
                                .next()
                                .map(|first| first.to_uppercase().chain(chars).collect())
                                .unwrap_or_default()
                        }
                    },
                    description: codex_catalog_string(level, "description"),
                    default: false,
                })
            })
            .collect(),
    )
}

/// Read the load-bearing per-model wire flags from a live catalog entry.
///
/// The preset (`openai_codex_provider`) was written for one model; catalog
/// entries differ on these fields and inheriting the preset wholesale is how
/// non-preset models 400 (#245).
fn codex_catalog_wire_capabilities(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> xai_grok_sampling_types::CodexWireCapabilities {
    xai_grok_sampling_types::CodexWireCapabilities {
        use_responses_lite: codex_catalog_bool(obj, "use_responses_lite"),
        tool_mode: codex_catalog_string(obj, "tool_mode"),
        supports_reasoning_summary_parameter: codex_catalog_bool(
            obj,
            "supports_reasoning_summary_parameter",
        ),
        supports_image_detail_original: codex_catalog_bool(obj, "supports_image_detail_original"),
        input_modalities: codex_catalog_string_list(obj, "input_modalities"),
        default_reasoning_level: codex_catalog_string(obj, "default_reasoning_level"),
        truncation_policy: codex_catalog_truncation_policy(obj),
        auto_compact_token_limit: codex_catalog_auto_compact_token_limit(obj),
        base_instructions: codex_catalog_string(obj, "base_instructions"),
        model_messages: codex_catalog_model_messages(obj),
    }
}

fn codex_catalog_auto_compact_token_limit(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Option<u64> {
    let value = obj.get("auto_compact_token_limit")?;
    if value.is_null() {
        return None;
    }
    let limit = value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|n| u64::try_from(n).ok()));
    match limit {
        Some(limit) if limit > 0 => Some(limit),
        Some(_) => {
            tracing::warn!(
                value = %value,
                "ignoring non-positive Codex catalog auto_compact_token_limit"
            );
            None
        }
        None => {
            tracing::warn!(
                value = %value,
                "ignoring invalid Codex catalog auto_compact_token_limit"
            );
            None
        }
    }
}

fn codex_catalog_model_messages(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Option<xai_grok_sampling_types::CodexModelMessages> {
    let value = obj.get("model_messages")?;
    if value.is_null() {
        return None;
    }
    match serde_json::from_value::<xai_grok_sampling_types::CodexModelMessages>(value.clone()) {
        Ok(messages) => Some(messages),
        Err(error) => {
            tracing::warn!(%error, "ignoring invalid Codex catalog model_messages");
            None
        }
    }
}

fn codex_catalog_truncation_policy(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Option<xai_grok_sampling_types::TruncationPolicyConfig> {
    let value = obj.get("truncation_policy")?;
    let policy = match serde_json::from_value::<xai_grok_sampling_types::TruncationPolicyConfig>(
        value.clone(),
    ) {
        Ok(policy) => policy,
        Err(error) => {
            tracing::warn!(%error, "ignoring invalid Codex catalog truncation_policy");
            return None;
        }
    };
    if policy.limit <= 0 {
        tracing::warn!(
            limit = policy.limit,
            "ignoring non-positive Codex catalog truncation_policy limit"
        );
        return None;
    }
    Some(policy)
}

fn parse_openai_codex_catalog_entry(
    value: &serde_json::Value,
) -> Option<(String, ConfigModelOverride)> {
    let obj = value.as_object()?;
    // `slug` is what the live payload actually keys on; `id` and `model` are
    // kept as tolerances, not as the expected shape.
    let key = codex_catalog_string(obj, "slug")
        .or_else(|| codex_catalog_string(obj, "id"))
        .or_else(|| codex_catalog_string(obj, "model"))?;
    let model = codex_catalog_string(obj, "model").unwrap_or_else(|| key.clone());
    let context_window =
        codex_catalog_context_window(obj).unwrap_or(OPENAI_CODEX_PRESET_CONTEXT_WINDOW);
    let codex_wire = codex_catalog_wire_capabilities(obj);
    let advertised_reasoning_efforts = codex_catalog_reasoning_efforts(obj);
    let has_advertised_reasoning_efforts = advertised_reasoning_efforts.is_some();
    let mut reasoning_efforts = advertised_reasoning_efforts.unwrap_or_default();
    let advertised_default = codex_wire
        .default_reasoning_level
        .as_deref()
        .and_then(|level| {
            level
                .parse::<xai_grok_sampling_types::ReasoningEffort>()
                .ok()
        });
    // A catalog default is only usable when the same entry advertises it. If a
    // malformed catalog points outside its own menu, select the first supported
    // tier instead of sending a value the model says it rejects.
    let reasoning_effort = if has_advertised_reasoning_efforts {
        let default_index = advertised_default.and_then(|default| {
            reasoning_efforts
                .iter()
                .position(|option| option.value == default)
        });
        let selected_index = default_index.or_else(|| (!reasoning_efforts.is_empty()).then_some(0));
        if advertised_default.is_some() && default_index.is_none() {
            tracing::warn!(
                model = %model,
                default_reasoning_level = ?codex_wire.default_reasoning_level,
                "Codex catalog default reasoning level is not in supported_reasoning_levels; using the first supported level"
            );
        }
        selected_index.map(|index| {
            reasoning_efforts[index].default = true;
            reasoning_efforts[index].value
        })
    } else {
        advertised_default
    };
    let supports_reasoning_effort = if has_advertised_reasoning_efforts {
        Some(!reasoning_efforts.is_empty())
    } else {
        reasoning_effort.map(|_| true)
    };
    let mut parsed = ConfigModelOverride {
        model: Some(model.clone()),
        model_provider: Some(OPENAI_CODEX_PROVIDER_ID.to_owned()),
        name: codex_catalog_string(obj, "display_name")
            .or_else(|| codex_catalog_string(obj, "name"))
            .or(Some(model.clone())),
        description: codex_catalog_string(obj, "description")
            .or(Some(OPENAI_CODEX_MODELS_CATALOG_DESCRIPTION.to_owned())),
        context_window: Some(context_window),
        reasoning_effort,
        supports_reasoning_effort,
        reasoning_efforts,
        codex_wire: Some(codex_wire),
        catalog_upgrade: codex_catalog_upgrade(obj),
        effective_context_window_percent: codex_catalog_effective_context_window_percent(obj),
        ..ConfigModelOverride::default()
    };
    if let Some(floor) = codex_catalog_minimal_client_version(obj)
        && stamp_codex_catalog_client_version_floor(&mut parsed, &floor)
    {
        tracing::warn!(
            model = %model,
            advertised = OPENAI_CODEX_CATALOG_CLIENT_VERSION,
            required = %floor,
            "Codex catalog model requires a newer client"
        );
    }
    Some((key, parsed))
}

fn parse_openai_codex_catalog_models(
    payload: &serde_json::Value,
) -> IndexMap<String, ConfigModelOverride> {
    // The live payload is `{"models": [...]}`. `data` and a bare array are
    // tolerances for shapes this endpoint does not currently return.
    let Some(entries) = payload
        .get("models")
        .and_then(serde_json::Value::as_array)
        .or_else(|| payload.get("data").and_then(serde_json::Value::as_array))
        .or_else(|| payload.as_array())
    else {
        return IndexMap::new();
    };
    let mut models: IndexMap<_, _> = entries
        .iter()
        .filter_map(parse_openai_codex_catalog_entry)
        .collect();
    // Parsed so a future payload that starts using this as a floor is visible
    // in diagnostics. It is not itself a minimum-client requirement.
    let catalog_client_version = codex_catalog_payload_client_version(payload);
    if let Some(client_version) = catalog_client_version.as_deref() {
        tracing::debug!(client_version, "Codex catalog payload client_version");
    }
    if let Some(floor) = payload
        .as_object()
        .and_then(codex_catalog_minimal_client_version)
    {
        let mut stamped = false;
        for model in models.values_mut() {
            stamped |= stamp_codex_catalog_client_version_floor(model, &floor);
        }
        if stamped {
            tracing::warn!(
                advertised = OPENAI_CODEX_CATALOG_CLIENT_VERSION,
                required = %floor,
                catalog_client_version = catalog_client_version.as_deref().unwrap_or(""),
                "Codex catalog requires a newer client"
            );
        }
    }
    models
}

fn fetch_openai_codex_catalog_models_blocking(
    credential: CodexCatalogCredential,
    url: String,
) -> Option<(serde_json::Value, IndexMap<String, ConfigModelOverride>)> {
    let client = crate::remote::client::models_catalog_blocking_client();
    let request = apply_codex_catalog_auth_headers(client.get(url), &credential)?;
    let response = match request.send() {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(error = %error, "Codex catalog fetch failed");
            return None;
        }
    };
    if !response.status().is_success() {
        tracing::warn!(
            status = response.status().as_u16(),
            "Codex catalog fetch failed"
        );
        return None;
    }
    let payload: serde_json::Value = match response.json() {
        Ok(payload) => payload,
        Err(error) => {
            tracing::warn!(error = %error, "Codex catalog response was not valid JSON");
            return None;
        }
    };
    let models = parse_openai_codex_catalog_models(&payload);
    if models.is_empty() {
        tracing::warn!("Codex catalog fetch returned no usable models");
        return None;
    }
    Some((payload, models))
}

/// Run the complete reqwest blocking request outside any caller's Tokio
/// context. Isolating only `Client::build` is insufficient: reqwest's blocking
/// `RequestBuilder::send` also creates and drops a private runtime while it
/// waits, which Tokio rejects when config discovery happens inside async
/// startup (#291).
fn fetch_openai_codex_catalog_models_on_native_thread(
    credential: CodexCatalogCredential,
    url: String,
) -> Option<(serde_json::Value, IndexMap<String, ConfigModelOverride>)> {
    let worker = match std::thread::Builder::new()
        .name("codex-model-catalog".to_owned())
        .spawn(move || fetch_openai_codex_catalog_models_blocking(credential, url))
    {
        Ok(worker) => worker,
        Err(error) => {
            tracing::warn!(error = %error, "Codex catalog worker could not start");
            return None;
        }
    };
    match worker.join() {
        Ok(models) => models,
        Err(_) => {
            tracing::warn!("Codex catalog worker panicked");
            None
        }
    }
}

thread_local! {
    static LIVE_CODEX_CATALOG_FETCH_ATTEMPTS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn record_live_codex_catalog_fetch_attempt() {
    LIVE_CODEX_CATALOG_FETCH_ATTEMPTS.with(|count| count.set(count.get().saturating_add(1)));
}

#[cfg(test)]
pub(crate) fn live_codex_catalog_fetch_attempts() -> u32 {
    LIVE_CODEX_CATALOG_FETCH_ATTEMPTS.with(std::cell::Cell::get)
}

#[cfg(test)]
pub(crate) fn reset_live_codex_catalog_fetch_attempts() {
    LIVE_CODEX_CATALOG_FETCH_ATTEMPTS.with(|count| count.set(0));
}

fn fetch_openai_codex_catalog_models() -> Option<CodexCatalogListing> {
    record_live_codex_catalog_fetch_attempt();
    if cfg!(test) {
        return None;
    }
    let home = crate::util::grok_home::grok_home();
    let (cache_identity, credential) = codex_catalog_access(&home)?;
    let cache_path = codex_catalog_cache_path(&home, &cache_identity);
    if !crate::util::config::resolve_remote_fetch_enabled() {
        return load_codex_catalog_when_remote_fetch_disabled(cache_path.as_deref());
    }
    let Some(credential) = credential else {
        return Some(CodexCatalogListing::stand_in(
            codex_catalog_fallback_models(cache_path.as_deref()),
        ));
    };
    let url = format!(
        "{}/models?client_version={}",
        crate::auth::openai_codex::CODEX_API_BASE_URL,
        OPENAI_CODEX_CATALOG_CLIENT_VERSION
    );
    match fetch_openai_codex_catalog_models_on_native_thread(credential, url) {
        Some((payload, models)) => {
            if let Some(path) = cache_path.as_deref() {
                persist_codex_catalog_cache(path, &payload);
            } else {
                tracing::warn!(
                    "Codex catalog has no verified account id; not persisting account-scoped cache"
                );
            }
            // A live listing stays served even when rows carry a version
            // floor: the account's slug set is exactly what came back.
            Some(CodexCatalogListing::served(models))
        }
        None => Some(CodexCatalogListing::stand_in(
            codex_catalog_fallback_models(cache_path.as_deref()),
        )),
    }
}

/// The built-in list is returned `served`, which is what the previous
/// `catalog_degraded_reason`-derived authority made it — those entries are
/// unmarked, so `all(..is_none())` was true. Whether built-ins should really
/// be treated as enumerating an account's slugs is a separate question from
/// this fix, and answering it here would change which entries are rejected;
/// `served` is also the stricter of the two, so preserving it cannot widen the
/// defect this change closes.
fn effective_openai_codex_presets(fetched: Option<CodexCatalogListing>) -> CodexCatalogListing {
    fetched
        .filter(|listing| !listing.models.is_empty())
        .unwrap_or_else(|| CodexCatalogListing::served(openai_codex_preset_models()))
}

fn openai_codex_provider() -> ModelProviderConfig {
    ModelProviderConfig {
        base_url: Some(crate::auth::openai_codex::CODEX_API_BASE_URL.to_owned()),
        api_backend: Some(ApiBackend::CodexResponses),
        ..ModelProviderConfig::default()
    }
}

/// Built-in `[model.*]` entries bound to the reserved `openai-codex` provider.
///
/// Without these, `grok login --provider openai-codex` succeeds into a session
/// with no Codex model at all: the reserved provider ships no catalog entries,
/// so the user has to hand-write a `[model.<id>]` block in the *global*
/// `$GROK_HOME/config.toml` before anything is selectable.
///
/// Folded into the user's `[model.*]` table by
/// [`merge_openai_codex_presets`]. They carry no credential of their own —
/// readiness gates on the provider-scoped OAuth snapshot exactly like a
/// hand-written entry does, which is what surfaces them as unready with a
/// "sign in" reason before login.
fn openai_codex_preset_models() -> IndexMap<String, ConfigModelOverride> {
    IndexMap::from([(
        OPENAI_CODEX_PRESET_MODEL_ID.to_owned(),
        ConfigModelOverride {
            model: Some(OPENAI_CODEX_PRESET_MODEL_ID.to_owned()),
            model_provider: Some(OPENAI_CODEX_PROVIDER_ID.to_owned()),
            name: Some("GPT-5.6 Sol (Codex)".to_owned()),
            description: Some("OpenAI Codex via a ChatGPT subscription".to_owned()),
            context_window: Some(OPENAI_CODEX_PRESET_CONTEXT_WINDOW),
            ..ConfigModelOverride::default()
        },
    )])
}

impl ConfigModelOverride {
    /// Whether this entry names where its traffic goes and what authenticates
    /// it, rather than leaving both to a preset or provider.
    fn declares_own_routing(&self) -> bool {
        self.model_provider.is_some()
            || self.base_url.is_some()
            || self.api_base_url.is_some()
            || self.api_key.is_some()
            || self.env_key.is_some()
            || self.auth_provider.is_some()
    }
}

/// Wire slugs the live or built-in Codex catalog will accept. Includes both
/// catalog keys (`slug`) and each entry's `model` field so a metadata-only
/// override of a real catalog row stays ready when those two differ.
pub(crate) fn openai_codex_catalog_wire_slugs(
    catalog: &IndexMap<String, ConfigModelOverride>,
) -> std::collections::HashSet<String> {
    catalog
        .iter()
        .flat_map(|(key, entry)| std::iter::once(key.clone()).chain(entry.model.iter().cloned()))
        .collect()
}

/// Fail-closed reason for a Codex-routed entry whose wire model is not a
/// live/builtin catalog slug (#260). Names the bad wire model so the picker
/// can say what would 400.
pub(crate) fn openai_codex_unknown_catalog_slug_reason(wire_model: &str) -> String {
    format!(
        "`{wire_model}` is not a Codex catalog slug; \
         set `model` to a live or built-in Codex catalog slug"
    )
}

/// [`crate::agent::config::model_readiness`] consults this after the Codex
/// origin allowlist so a signed-in credential cannot make a non-slug ready.
pub(crate) fn unknown_openai_codex_catalog_slug_reason(
    model: &super::config::ModelEntry,
) -> Option<String> {
    model
        .config_validation_errors
        .iter()
        .find(|error| error.contains("is not a Codex catalog slug"))
        .cloned()
}

fn stamp_unknown_openai_codex_catalog_slugs(
    config_models: &mut IndexMap<String, ConfigModelOverride>,
    catalog_slugs: &std::collections::HashSet<String>,
    catalog_is_authoritative: bool,
) {
    for (key, entry) in config_models.iter_mut() {
        if entry.model_provider.as_deref() != Some(OPENAI_CODEX_PROVIDER_ID) {
            entry.unknown_codex_catalog_slug = None;
            continue;
        }
        // #260: the gate is "the catalog said no", never "the catalog did not
        // say yes". A degraded catalog is a saved cache or the single built-in
        // preset, so it cannot speak for slugs it never listed, and a stamp
        // left by an earlier authoritative refresh must not outlive it.
        if !catalog_is_authoritative {
            entry.unknown_codex_catalog_slug = None;
            continue;
        }
        // `apply` seeds `info.model` from the TOML key when `model` is absent.
        let wire_model = entry.model.as_deref().unwrap_or(key.as_str());
        if catalog_slugs.contains(wire_model) {
            entry.unknown_codex_catalog_slug = None;
        } else {
            entry.unknown_codex_catalog_slug = Some(wire_model.to_owned());
        }
    }
}

/// Fold the built-in Codex presets into the user's parsed `[model.*]` table.
///
/// A key the user never declared gets the preset outright.
///
/// A key the user *did* declare keeps their entry, but a metadata-only
/// override — a new `name`, a bigger `context_window` — names no endpoint and
/// no credential. Letting it replace the preset wholesale would strip the
/// `model_provider` binding, and the entry would then resolve as a plain xAI
/// catalog model: routed to the xAI inference endpoint and authenticated with
/// the user's xAI session token, under a key the docs describe as Codex. So
/// the preset's routing is backfilled underneath such an override.
///
/// An override that declares its own provider, endpoint, or credential has
/// taken ownership of the key and is left exactly as written — redefining
/// `[model."gpt-5.6-sol"]` as some other provider's model stays possible.
pub(crate) fn merge_openai_codex_presets(
    config_models: &mut IndexMap<String, ConfigModelOverride>,
) {
    merge_openai_codex_preset_entries(
        config_models,
        effective_openai_codex_presets(fetch_openai_codex_catalog_models()),
    );
}

/// Same overlay as [`merge_openai_codex_presets`], but never `GET /models`.
///
/// Standalone `grok doctor` / inspect must stay side-effect-free: a usable
/// Codex credential plus default remote fetch would otherwise block on the
/// startup timeout and rewrite the account catalog cache.
pub(crate) fn merge_openai_codex_presets_offline(
    config_models: &mut IndexMap<String, ConfigModelOverride>,
) {
    merge_openai_codex_preset_entries(
        config_models,
        effective_openai_codex_presets(offline_openai_codex_catalog_models()),
    );
}

fn offline_openai_codex_catalog_models() -> Option<CodexCatalogListing> {
    if cfg!(test) {
        return None;
    }
    let home = crate::util::grok_home::grok_home();
    let Some((cache_identity, _)) = codex_catalog_access(&home) else {
        return Some(CodexCatalogListing::stand_in(
            codex_catalog_fallback_models(None),
        ));
    };
    let cache_path = codex_catalog_cache_path(&home, &cache_identity);
    load_codex_catalog_when_remote_fetch_disabled(cache_path.as_deref()).or_else(|| {
        Some(CodexCatalogListing::stand_in(
            codex_catalog_fallback_models(cache_path.as_deref()),
        ))
    })
}

fn merge_openai_codex_preset_entries(
    config_models: &mut IndexMap<String, ConfigModelOverride>,
    listing: CodexCatalogListing,
) {
    // Authority comes from where the listing came from, never from the rows'
    // `catalog_degraded_reason` — see `CodexCatalogListing`.
    let CodexCatalogListing {
        models: presets,
        enumerates_account_slugs: catalog_is_authoritative,
    } = listing;
    let catalog_slugs = openai_codex_catalog_wire_slugs(&presets);
    for (key, preset) in presets {
        let Some(user_entry) = config_models.get_mut(&key) else {
            config_models.insert(key, preset);
            continue;
        };
        if user_entry.declares_own_routing() {
            continue;
        }
        // Overlay the user's fields on the whole preset, not just its routing:
        // `context_window = 400000` alone must not also blank the shipped
        // display name and description out of the picker.
        let ConfigModelOverride {
            model,
            model_provider,
            name,
            description,
            context_window,
            reasoning_effort,
            supports_reasoning_effort,
            reasoning_efforts,
            codex_wire,
            catalog_degraded_reason,
            catalog_upgrade,
            effective_context_window_percent,
            ..
        } = preset;
        user_entry.model_provider = model_provider;
        user_entry.model.get_or_insert(model.unwrap_or(key));
        if let Some(name) = name {
            user_entry.name.get_or_insert(name);
        }
        if let Some(description) = description {
            user_entry.description.get_or_insert(description);
        }
        if let Some(context_window) = context_window {
            user_entry.context_window.get_or_insert(context_window);
        }
        let user_reasoning_support = user_entry.supports_reasoning_effort;
        let reasoning_disabled = user_reasoning_support == Some(false)
            || (user_reasoning_support.is_none() && supports_reasoning_effort == Some(false));
        if reasoning_disabled {
            // Explicit disable is authoritative for both the menu and scalar;
            // An explicitly empty catalog menu is also authoritative unless
            // the user explicitly opts back in to legacy scalar support.
            user_entry.reasoning_effort = None;
            user_entry.reasoning_efforts.clear();
        } else if user_entry.reasoning_effort.is_none() {
            user_entry.reasoning_effort = reasoning_effort;
        }
        if user_entry.supports_reasoning_effort.is_none() {
            user_entry.supports_reasoning_effort = supports_reasoning_effort;
        }
        if user_entry.reasoning_efforts.is_empty()
            && user_entry.supports_reasoning_effort != Some(false)
        {
            user_entry.reasoning_efforts = reasoning_efforts;
        }
        // Reconcile against the final menu, regardless of whether that menu
        // came from metadata or the catalog. An inherited catalog scalar can
        // otherwise survive a narrower user menu and reach the wire rejected.
        if !user_entry.reasoning_efforts.is_empty() {
            let selected_index = user_entry
                .reasoning_effort
                .and_then(|effort| {
                    user_entry
                        .reasoning_efforts
                        .iter()
                        .position(|option| option.value == effort)
                })
                .or_else(|| {
                    user_entry
                        .reasoning_efforts
                        .iter()
                        .position(|option| option.default)
                })
                .unwrap_or(0);
            user_entry.reasoning_effort = Some(user_entry.reasoning_efforts[selected_index].value);
            for (index, option) in user_entry.reasoning_efforts.iter_mut().enumerate() {
                option.default = index == selected_index;
            }
        }
        if user_entry.codex_wire.is_none() {
            user_entry.codex_wire = codex_wire;
        }
        user_entry.catalog_degraded_reason = catalog_degraded_reason;
        if user_entry.catalog_upgrade.is_none() {
            user_entry.catalog_upgrade = catalog_upgrade;
        }
        if let Some(percent) = effective_context_window_percent {
            user_entry
                .effective_context_window_percent
                .get_or_insert(percent);
        }
    }
    stamp_unknown_openai_codex_catalog_slugs(
        config_models,
        &catalog_slugs,
        catalog_is_authoritative,
    );
}

#[derive(Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct ModelProviderConfig {
    pub base_url: Option<String>,
    pub api_base_url: Option<String>,
    pub env_key: Option<EnvKeys>,
    pub api_key: Option<String>,
    pub api_backend: Option<ApiBackend>,
    pub extra_headers: IndexMap<String, String>,
    /// Query parameters folded into every request URL; inherited by models.
    pub query_params: IndexMap<String, String>,
    /// Header name to environment variable; inherited by models, resolved at
    /// client build.
    pub env_http_headers: IndexMap<String, String>,
    pub auth_provider: Option<String>,
    pub auth: Option<crate::auth::AuthProviderConfig>,
    pub context_window: Option<u64>,
}

impl std::fmt::Debug for ModelProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelProviderConfig")
            .field("base_url_present", &self.base_url.is_some())
            .field("api_base_url_present", &self.api_base_url.is_some())
            .field("env_key_present", &self.env_key.is_some())
            .field("api_key_present", &self.api_key.is_some())
            .field("api_backend", &self.api_backend)
            .field("extra_headers_present", &!self.extra_headers.is_empty())
            .field("query_params_present", &!self.query_params.is_empty())
            .field(
                "env_http_headers_present",
                &!self.env_http_headers.is_empty(),
            )
            .field("auth_provider_present", &self.auth_provider.is_some())
            .field("auth_present", &self.auth.is_some())
            .field("context_window", &self.context_window)
            .finish()
    }
}

pub(crate) fn model_provider_auth_name(provider_id: &str) -> String {
    format!("model_provider:{provider_id}")
}

pub(crate) fn auth_config_issues(
    config: &crate::auth::AuthProviderConfig,
) -> Vec<(&'static str, ConfigWarningKind, String)> {
    let mut issues = Vec::new();
    if !config.is_usable() {
        issues.push((
            "command",
            ConfigWarningKind::InvalidValue,
            "missing or empty command; models resolve with no credential".to_owned(),
        ));
    }
    let skew = crate::auth::PROVIDER_TOKEN_EXPIRY_SKEW_SECS;
    if config.token_ttl_secs.is_some_and(|ttl| ttl <= skew) {
        issues.push((
            "token_ttl_secs",
            ConfigWarningKind::InvalidValue,
            format!(
                "at or below the {skew}s refresh margin; the command will run before every turn"
            ),
        ));
    }
    if let Some(timeout) = config.timeout_secs
        && !(1..=crate::auth::PROVIDER_TIMEOUT_CEILING_SECS).contains(&timeout)
    {
        let ceiling = crate::auth::PROVIDER_TIMEOUT_CEILING_SECS;
        issues.push((
            "timeout_secs",
            ConfigWarningKind::InvalidValue,
            if timeout == 0 {
                "below the 1 second minimum; clamped to 1".to_owned()
            } else {
                format!("above the {ceiling}s maximum; clamped to {ceiling}")
            },
        ));
    }
    issues
}

pub(crate) fn parse_model_providers(
    raw_config: &toml::Value,
) -> (IndexMap<String, ModelProviderConfig>, Vec<ConfigWarning>) {
    let mut providers = IndexMap::new();
    providers.insert(OPENAI_CODEX_PROVIDER_ID.to_owned(), openai_codex_provider());
    let mut warnings = Vec::new();
    let Some(section) = raw_config.get("model_providers") else {
        return (providers, warnings);
    };
    let Some(table) = section.as_table() else {
        warnings.push(ConfigWarning::model_provider_section(
            ConfigWarningKind::NotATable,
            format!(
                "`model_providers` must be a table of [model_providers.<id>] entries, got {}; \
                 all model providers ignored",
                section.type_str()
            ),
        ));
        return (providers, warnings);
    };
    for (id, value) in table {
        if id == OPENAI_CODEX_PROVIDER_ID {
            warnings.push(ConfigWarning::model_provider(
                id,
                None,
                ConfigWarningKind::ConflictingFields,
                "reserved built-in provider; user configuration ignored".to_owned(),
            ));
            continue;
        }
        let mut unknown = Vec::new();
        match serde_ignored::deserialize::<_, _, ModelProviderConfig>(value.clone(), |path| {
            unknown.push(path.to_string());
        }) {
            Ok(provider) => {
                for key in unknown {
                    warnings.push(ConfigWarning::model_provider(
                        id,
                        Some(key.as_str()),
                        ConfigWarningKind::UnknownField,
                        "unrecognized key; field ignored".to_owned(),
                    ));
                }
                let mut provider = provider;
                // #13: same env_key normalization + path-specific warnings as
                // `[model.*]` — do not silently keep whitespace / illegal names.
                if let Some(raw_keys) = provider.env_key.take() {
                    let candidates: Vec<String> =
                        raw_keys.names().into_iter().map(str::to_owned).collect();
                    let (normalized, rejected) = EnvKeys::normalize(candidates);
                    for rejected in rejected {
                        let reason = if rejected.name.is_empty() {
                            format!(
                                "invalid env_key entry ({}); ignored — not used as a credential source",
                                rejected.reason
                            )
                        } else {
                            format!(
                                "invalid env_key name {:?}: {}; ignored — not used as a credential source",
                                rejected.name, rejected.reason
                            )
                        };
                        warnings.push(ConfigWarning::model_provider(
                            id,
                            Some("env_key"),
                            ConfigWarningKind::InvalidValue,
                            reason,
                        ));
                    }
                    if !normalized.is_empty() {
                        provider.env_key = Some(normalized);
                    }
                }
                if let Some(auth) = &provider.auth {
                    for (field, kind, reason) in auth_config_issues(auth) {
                        warnings.push(ConfigWarning::model_provider(
                            id,
                            Some(&format!("auth.{field}")),
                            kind,
                            reason,
                        ));
                    }
                }
                let has_helper = provider.auth.is_some() || provider.auth_provider.is_some();
                let has_static_api_key = provider
                    .api_key
                    .as_deref()
                    .map(str::trim)
                    .is_some_and(|k| !k.is_empty());
                if has_helper && has_static_api_key {
                    warnings.push(ConfigWarning::model_provider(
                        id,
                        Some("api_key"),
                        ConfigWarningKind::ConflictingFields,
                        "api_key shadows this provider's auth helper; the static key always \
                         takes precedence, so the helper never runs for inheriting models"
                            .to_owned(),
                    ));
                } else if has_helper
                    && provider
                        .env_key
                        .as_ref()
                        .and_then(EnvKeys::primary)
                        .is_some()
                {
                    warnings.push(ConfigWarning::model_provider(
                        id,
                        Some("env_key"),
                        ConfigWarningKind::ConflictingFields,
                        "env_key may shadow this provider's auth helper; env_key takes precedence \
                         when its variable resolves, otherwise the helper runs"
                            .to_owned(),
                    ));
                }
                if provider.auth_provider.is_some() && provider.auth.is_some() {
                    warnings.push(ConfigWarning::model_provider(
                        id,
                        Some("auth"),
                        ConfigWarningKind::ConflictingFields,
                        "inline auth is shadowed by auth_provider on this provider; the referenced \
                         provider takes precedence, so the inline helper never runs"
                            .to_owned(),
                    ));
                }
                providers.insert(id.clone(), provider);
            }
            Err(error) => {
                warnings.push(ConfigWarning::model_provider(
                    id,
                    None,
                    ConfigWarningKind::InvalidValue,
                    format!(
                        "failed to parse ({error}); provider skipped, inheriting models \
                         resolve with defaults"
                    ),
                ));
            }
        }
    }
    (providers, warnings)
}

impl ConfigModelOverride {
    pub(crate) fn with_provider_defaults(
        &self,
        provider: &ModelProviderConfig,
        provider_id: &str,
    ) -> Self {
        let ModelProviderConfig {
            base_url,
            api_base_url,
            env_key,
            api_key,
            api_backend,
            extra_headers,
            query_params,
            env_http_headers,
            auth_provider,
            auth,
            context_window,
        } = provider;

        let mut merged = self.clone();
        merged.model_provider = None;
        merged.base_url = merged.base_url.or_else(|| base_url.clone());
        merged.api_base_url = merged.api_base_url.or_else(|| api_base_url.clone());
        merged.api_backend = merged.api_backend.or_else(|| api_backend.clone());
        merged.context_window = merged.context_window.or(*context_window);
        // Inherited wholesale only when the model sets none of its own.
        if merged.extra_headers.is_empty() {
            merged.extra_headers = extra_headers.clone();
        }
        if merged.query_params.is_empty() {
            merged.query_params = query_params.clone();
        }
        if merged.env_http_headers.is_empty() {
            merged.env_http_headers = env_http_headers.clone();
        }
        let model_sets_own_api_key = self
            .api_key
            .as_deref()
            .is_some_and(|k| !k.trim().is_empty());
        let model_sets_own_env_key = self.env_key.as_ref().and_then(EnvKeys::primary).is_some();
        let model_has_own_auth =
            model_sets_own_api_key || model_sets_own_env_key || self.auth_provider.is_some();
        if !model_has_own_auth {
            merged.api_key = api_key.clone();
            merged.env_key = env_key.clone();
            merged.auth_provider = auth_provider
                .clone()
                .or_else(|| auth.as_ref().map(|_| model_provider_auth_name(provider_id)));
        }
        merged
    }

    pub(crate) fn with_missing_provider(&self) -> Self {
        let mut merged = self.clone();
        merged.model_provider = None;
        merged
    }
}

#[cfg(test)]
mod tests {
    use agent_client_protocol as acp;

    use super::*;
    use crate::agent::config::{Config, resolve_credentials, resolve_model_list};

    const CODEX_CATALOG_RUNTIME_CHILD: &str = "__XAI_CODEX_CATALOG_RUNTIME_CHILD";
    const CODEX_CATALOG_RUNTIME_PASS: &str = "codex-catalog-runtime-fetch-ok";

    /// Child-process body for #291. The fresh process prevents another test
    /// from warming reqwest's process-wide blocking client before Tokio starts.
    #[test]
    fn codex_catalog_runtime_first_fetch_child() {
        if std::env::var_os(CODEX_CATALOG_RUNTIME_CHILD).is_none() {
            return;
        }
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind catalog server");
        let address = listener.local_addr().expect("catalog server address");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept catalog request");
            let mut request = [0_u8; 4096];
            let read = stream.read(&mut request).expect("read catalog request");
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.starts_with("GET /models?client_version=0.0.0 HTTP/1.1"));
            assert!(request.contains("authorization: Bearer catalog-test-token"));
            let body =
                r#"{"models":[{"slug":"gpt-test","model":"gpt-test","context_window":12345}]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .expect("write catalog response");
        });
        let credential = CodexCatalogCredential {
            access_token: "catalog-test-token".to_owned(),
            account_id: Some("catalog-test-account".to_owned()),
            chatgpt_account_is_fedramp: false,
            source: xai_grok_sampler::CredentialSource::AuthProvider {
                name: OPENAI_CODEX_PROVIDER_ID.to_owned(),
            },
        };
        let url = format!("http://{address}/models?client_version=0.0.0");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build child Tokio runtime");
        let (_, models) = runtime
            .block_on(
                async move { fetch_openai_codex_catalog_models_on_native_thread(credential, url) },
            )
            .expect("fetch catalog inside Tokio");
        server.join().expect("catalog server thread");
        assert_eq!(models["gpt-test"].context_window, Some(12_345));
        println!("{CODEX_CATALOG_RUNTIME_PASS}");
    }

    /// Launch an exact child test so both the reqwest client and its first
    /// authenticated request are exercised from a fresh process under Tokio.
    #[test]
    fn codex_catalog_runtime_first_fetch_parent() {
        if std::env::var_os(CODEX_CATALOG_RUNTIME_CHILD).is_some() {
            return;
        }
        let filter = module_path!()
            .split_once("::")
            .map(|(_, rest)| rest)
            .unwrap_or_default();
        let mut command = std::process::Command::new(std::env::current_exe().expect("current_exe"));
        command
            .arg("--exact")
            .arg(format!("{filter}::codex_catalog_runtime_first_fetch_child"))
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(CODEX_CATALOG_RUNTIME_CHILD, "1")
            .stdin(std::process::Stdio::null());
        xai_tty_utils::detach_std_command(&mut command);
        let output = command.output().expect("spawn catalog fetch child test");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && !stderr.contains("panicked at"),
            "fresh-process Codex catalog fetch failed under Tokio \
             (status: {:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
            output.status
        );
        assert!(
            stdout.contains(CODEX_CATALOG_RUNTIME_PASS),
            "child did not execute the catalog fetch path\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }

    #[test]
    fn provider_debug_is_presence_only() {
        // Sentinel must not share an 8-byte window with Debug field names
        // (`auth_provider_present`, `api_key_present`, …) or the scan false-fails.
        let secret = "GB002-cfg-sentinel-Q7w5E3r1T9y7U2i4";
        let config = ModelProviderConfig {
            base_url: Some(format!(
                "https://user:{secret}@example.test/?token={secret}"
            )),
            api_base_url: Some(secret.to_owned()),
            api_key: Some(secret.to_owned()),
            extra_headers: IndexMap::from([("Authorization".to_owned(), secret.to_owned())]),
            query_params: IndexMap::from([("token".to_owned(), secret.to_owned())]),
            env_http_headers: IndexMap::from([("X-Secret".to_owned(), secret.to_owned())]),
            auth_provider: Some(secret.to_owned()),
            ..ModelProviderConfig::default()
        };
        let rendered = format!("{config:?}");
        assert!(!rendered.contains(secret));
        for window in secret.as_bytes().windows(8) {
            let window = std::str::from_utf8(window).expect("ASCII sentinel");
            assert!(
                !rendered.contains(window),
                "leaked sentinel window: {window}"
            );
        }
    }
    #[test]
    fn model_inherits_provider_connection_defaults() {
        let raw_config: toml::Value = toml::from_str(
            r#"
            [model_providers.gateway]
            base_url = "https://gateway.example/v1"
            context_window = 123456

            [model_providers.gateway.extra_headers]
            X-Corp = "yes"

            [model.via-gateway]
            model = "m"
            model_provider = "gateway"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        assert!(cfg.model_providers.contains_key("gateway"));
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved.get("via-gateway").expect("model should exist");
        assert_eq!(model.info.base_url, "https://gateway.example/v1");
        assert_eq!(model.info.context_window.get(), 123456);
        assert_eq!(
            model.info.extra_headers.get("X-Corp").map(String::as_str),
            Some("yes")
        );
        assert!(
            model.has_own_credentials(),
            "a custom endpoint without a credential is BYOK, not session-authed"
        );
        // `assert!(..is_none())`, not `assert_eq!(.., None)`: on failure the
        // latter formats the left value, and for a model with an
        // `auth_provider` that value is the developer's live OAuth access
        // token. A test that prints the credential it is guarding is not a
        // guard.
        assert!(
            resolve_credentials(model, Some("session-jwt"))
                .api_key
                .is_none(),
            "the session token must not leak to the provider's custom endpoint"
        );
    }

    #[test]
    fn model_fields_override_provider_defaults() {
        let raw_config: toml::Value = toml::from_str(
            r#"
            [model_providers.gateway]
            base_url = "https://gateway.example/v1"
            context_window = 100000

            [model.override-url]
            model = "m"
            model_provider = "gateway"
            base_url = "https://model-specific.example/v1"
            context_window = 200000
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved.get("override-url").expect("model should exist");
        assert_eq!(model.info.base_url, "https://model-specific.example/v1");
        assert_eq!(model.info.context_window.get(), 200000);
    }

    #[test]
    fn model_provider_inline_auth_registers_synthetic_provider() {
        let raw_config: toml::Value = toml::from_str(
            r#"
            [model_providers.gateway]
            base_url = "https://gateway.example/v1"
            context_window = 200000

            [model_providers.gateway.auth]
            command = "printf gw-token"
            token_ttl_secs = 3600

            [model.byok-via-gateway]
            model = "m"
            model_provider = "gateway"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        assert_eq!(
            cfg.auth_providers
                .get("model_provider:gateway")
                .map(|c| c.command.as_str()),
            Some("printf gw-token"),
            "inline auth registers a synthetic provider keyed by the id"
        );
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved
            .get("byok-via-gateway")
            .expect("model should exist");
        let provider = model
            .auth_provider
            .as_ref()
            .expect("the model inherits the provider's auth");
        assert_eq!(provider.name, "model_provider:gateway");
        assert_eq!(provider.config.command, "printf gw-token");
        assert!(
            model.has_own_credentials(),
            "a provider-backed model is BYOK (session token must not leak)"
        );
    }

    #[test]
    fn model_with_own_key_ignores_provider_auth() {
        let raw_config: toml::Value = toml::from_str(
            r#"
            [model_providers.gateway]
            base_url = "https://gateway.example/v1"
            context_window = 200000

            [model_providers.gateway.auth]
            command = "printf gw-token"

            [model.own-key]
            model = "m"
            model_provider = "gateway"
            api_key = "sk-model-own"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved.get("own-key").expect("model should exist");
        assert_eq!(
            model.info.base_url, "https://gateway.example/v1",
            "non-auth connection fields are still inherited"
        );
        assert_eq!(
            model.effective_auth_provider().map(|p| p.name.as_str()),
            None,
            "the model's own key shadows the provider's auth"
        );
        let creds = resolve_credentials(model, Some("session-jwt"));
        assert_eq!(creds.api_key.as_deref(), Some("sk-model-own"));
    }

    #[test]
    fn undefined_model_provider_fails_closed() {
        use super::super::config_model_override_parse::{ConfigWarningKind, WarningTarget};

        let raw_config: toml::Value = toml::from_str(
            r#"
            [model.dangling]
            model = "m"
            base_url = "https://third-party.example/v1"
            context_window = 200000
            model_provider = "ghost"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        assert!(
            cfg.config_warnings.iter().any(|w| {
                w.kind == ConfigWarningKind::InvalidValue
                    && matches!(
                        &w.target,
                        WarningTarget::Model { field, .. }
                            if field.as_deref() == Some("model_provider")
                    )
            }),
            "an undefined provider reference warns: {:?}",
            cfg.config_warnings
        );
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved.get("dangling").expect("model should exist");
        assert_eq!(
            model.info.base_url, "https://third-party.example/v1",
            "the model keeps its own connection fields"
        );
        assert!(
            model.has_own_credentials(),
            "an undefined provider leaves the model BYOK, not session-authed"
        );
        let creds = resolve_credentials(model, Some("session-jwt"));
        assert_eq!(
            creds.api_key, None,
            "no credential resolves and the session token does not leak to the model's base_url"
        );
    }

    #[test]
    fn undefined_model_provider_keeps_model_own_key() {
        let raw_config: toml::Value = toml::from_str(
            r#"
            [model.own-key]
            model = "m"
            base_url = "https://third-party.example/v1"
            context_window = 200000
            api_key = "sk-model-own"
            model_provider = "ghost"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved.get("own-key").expect("model should exist");
        let creds = resolve_credentials(model, Some("session-jwt"));
        assert_eq!(creds.api_key.as_deref(), Some("sk-model-own"));
    }

    #[test]
    fn model_provider_parse_warnings_are_lenient_and_specific() {
        use super::super::config_model_override_parse::{ConfigWarningKind, WarningTarget};

        let raw_config: toml::Value = toml::from_str(
            r#"
            [model_providers.good]
            base_url = "https://good.example/v1"

            [model_providers.bad-type]
            context_window = "not-a-number"

            [model_providers.typo]
            base_url = "https://typo.example/v1"
            unknown_field = 5

            [model.on-broken-provider]
            model = "m"
            base_url = "https://x.example/v1"
            context_window = 200000
            model_provider = "bad-type"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config)
            .expect("one bad provider must not fail the config");
        assert!(cfg.model_providers.contains_key("good"));
        assert!(
            !cfg.model_providers.contains_key("bad-type"),
            "a malformed provider is skipped"
        );

        let has_provider = |id: &str, field: Option<&str>, kind: ConfigWarningKind| {
            cfg.config_warnings.iter().any(|w| {
                w.kind == kind
                    && matches!(
                        &w.target,
                        WarningTarget::ModelProvider { id: i, field: f }
                            if i == id && f.as_deref() == field
                    )
            })
        };
        assert!(has_provider(
            "bad-type",
            None,
            ConfigWarningKind::InvalidValue
        ));
        assert!(has_provider(
            "typo",
            Some("unknown_field"),
            ConfigWarningKind::UnknownField
        ));
        assert!(
            !cfg.config_warnings.iter().any(|w| {
                matches!(
                    &w.target,
                    WarningTarget::Model { field, .. }
                        if field.as_deref() == Some("model_provider")
                )
            }),
            "a declared-but-malformed provider must not also warn as undefined: {:?}",
            cfg.config_warnings
        );

        let raw_config: toml::Value = toml::from_str(r#"model_providers = "oops""#).unwrap();
        let cfg = Config::new_from_toml_cfg(&raw_config)
            .expect("a non-table model_providers must not fail the config");
        assert_eq!(cfg.model_providers.len(), 1);
        assert!(cfg.model_providers.contains_key(OPENAI_CODEX_PROVIDER_ID));
        assert!(
            cfg.config_warnings.iter().any(|w| {
                matches!(w.target, WarningTarget::ModelProviderSection)
                    && w.kind == ConfigWarningKind::NotATable
            }),
            "non-table section warns: {:?}",
            cfg.config_warnings
        );
    }

    #[test]
    fn model_provider_conflicting_credentials_warn() {
        use super::super::config_model_override_parse::{ConfigWarningKind, WarningTarget};

        let raw_config: toml::Value = toml::from_str(
            r#"
            [model_providers.static-shadows]
            base_url = "https://a.example/v1"
            api_key = "sk-static"
            [model_providers.static-shadows.auth]
            command = "printf tok"

            [model_providers.env-shadows]
            base_url = "https://b.example/v1"
            env_key = "SOME_VAR"
            [model_providers.env-shadows.auth]
            command = "printf tok"

            [model_providers.two-helpers]
            base_url = "https://c.example/v1"
            auth_provider = "corp"
            [model_providers.two-helpers.auth]
            command = "printf tok"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        let has = |id: &str, field: &str| {
            cfg.config_warnings.iter().any(|w| {
                w.kind == ConfigWarningKind::ConflictingFields
                    && matches!(
                        &w.target,
                        WarningTarget::ModelProvider { id: i, field: f }
                            if i == id && f.as_deref() == Some(field)
                    )
            })
        };
        assert!(
            has("static-shadows", "api_key"),
            "a static api_key shadows the helper: {:?}",
            cfg.config_warnings
        );
        assert!(
            has("env-shadows", "env_key"),
            "an env_key may shadow the helper: {:?}",
            cfg.config_warnings
        );
        assert!(
            has("two-helpers", "auth"),
            "auth_provider shadows the inline auth helper: {:?}",
            cfg.config_warnings
        );
    }

    #[test]
    fn model_provider_undefined_auth_provider_warns() {
        use super::super::config_model_override_parse::{ConfigWarningKind, WarningTarget};

        let raw_config: toml::Value = toml::from_str(
            r#"
            [model_providers.gateway]
            base_url = "https://gateway.example/v1"
            auth_provider = "nonexistent"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        assert!(
            cfg.config_warnings.iter().any(|w| {
                w.kind == ConfigWarningKind::InvalidValue
                    && matches!(
                        &w.target,
                        WarningTarget::ModelProvider { id, field }
                            if id == "gateway" && field.as_deref() == Some("auth_provider")
                    )
            }),
            "an undefined provider auth_provider reference warns: {:?}",
            cfg.config_warnings
        );
    }

    #[test]
    fn model_provider_inline_auth_namespace_collision_warns() {
        use super::super::config_model_override_parse::{ConfigWarningKind, WarningTarget};

        let raw_config: toml::Value = toml::from_str(
            r#"
            [auth_provider."model_provider:gateway"]
            command = "printf hand-written"

            [model_providers.gateway]
            base_url = "https://gateway.example/v1"

            [model_providers.gateway.auth]
            command = "printf inline"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        assert!(
            cfg.config_warnings.iter().any(|w| {
                w.kind == ConfigWarningKind::ConflictingFields
                    && matches!(
                        &w.target,
                        WarningTarget::ModelProvider { id, field }
                            if id == "gateway" && field.as_deref() == Some("auth")
                    )
            }),
            "a reserved-namespace collision warns: {:?}",
            cfg.config_warnings
        );
        assert_eq!(
            cfg.auth_providers
                .get("model_provider:gateway")
                .map(|c| c.command.as_str()),
            Some("printf inline"),
            "inline auth wins the reserved name"
        );
    }

    #[test]
    fn model_inherits_provider_named_auth_provider() {
        let raw_config: toml::Value = toml::from_str(
            r#"
            [auth_provider.corp]
            command = "printf corp-token"
            token_ttl_secs = 3600

            [model_providers.gateway]
            base_url = "https://gateway.example/v1"
            auth_provider = "corp"

            [model.via-gateway]
            model = "m"
            model_provider = "gateway"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved.get("via-gateway").expect("model should exist");
        let provider = model
            .auth_provider
            .as_ref()
            .expect("the model inherits the provider's named auth_provider");
        assert_eq!(provider.name, "corp");
        assert_eq!(provider.config.command, "printf corp-token");
        assert!(model.has_own_credentials());
    }

    #[test]
    fn model_inherits_provider_static_key() {
        let raw_config: toml::Value = toml::from_str(
            r#"
            [model_providers.gateway]
            base_url = "https://gateway.example/v1"
            api_key = "sk-provider"

            [model.via-gateway]
            model = "m"
            model_provider = "gateway"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved.get("via-gateway").expect("model should exist");
        assert_eq!(
            resolve_credentials(model, Some("session-jwt"))
                .api_key
                .as_deref(),
            Some("sk-provider"),
            "the provider's static key resolves for the inheriting model"
        );
    }

    #[test]
    fn declared_unresolved_credential_fails_closed_on_provider_endpoint() {
        let raw_config: toml::Value = toml::from_str(
            r#"
            [model_providers.gateway]
            base_url = "https://gateway.example/v1"

            [model.via-gateway]
            model = "m"
            model_provider = "gateway"
            env_key = "DEFINITELY_UNSET_MODEL_PROVIDER_TEST_VAR"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved.get("via-gateway").expect("model should exist");
        assert!(
            resolve_credentials(model, Some("session-jwt"))
                .api_key
                .is_none(),
            "an unresolved declared credential must not fall back to the session token"
        );
    }

    #[test]
    fn model_inherits_provider_api_backend_and_base_url() {
        let raw_config: toml::Value = toml::from_str(
            r#"
            [model_providers.gateway]
            base_url = "https://gateway.example/v1"
            api_base_url = "https://gateway.example/api"
            api_backend = "responses"
            api_key = "sk-provider"

            [model.via-gateway]
            model = "m"
            model_provider = "gateway"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved.get("via-gateway").expect("model should exist");
        assert_eq!(
            model.info.api_backend,
            crate::sampling::ApiBackend::Responses
        );
        assert_eq!(
            model.api_base_url.as_deref(),
            Some("https://gateway.example/api")
        );
    }

    #[test]
    fn model_own_unresolved_key_ignores_provider_inline_auth() {
        let raw_config: toml::Value = toml::from_str(
            r#"
            [model_providers.gateway]
            base_url = "https://gateway.example/v1"

            [model_providers.gateway.auth]
            command = "printf gw-token"

            [model.own-env]
            model = "m"
            model_provider = "gateway"
            env_key = "DEFINITELY_UNSET_MODEL_PROVIDER_INLINE_VAR"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved.get("own-env").expect("model should exist");
        let effective = model
            .effective_auth_provider()
            .expect("an unresolved own credential fails closed via a provider ref");
        assert!(
            effective.name.contains("fail-closed"),
            "must pin the unusable fail-closed ref, not the live inline auth: {}",
            effective.name
        );
        assert!(
            effective.config.command.is_empty(),
            "the fail-closed ref is unusable"
        );
        assert!(
            resolve_credentials(model, Some("session-jwt"))
                .api_key
                .is_none(),
            "must not fall back to the session token"
        );
    }

    #[test]
    fn fail_closed_ref_ignores_a_colliding_auth_provider_table() {
        let raw_config: toml::Value = toml::from_str(
            r#"
            [auth_provider."model_provider:gateway (fail-closed)"]
            command = "printf sneaky-token"

            [model_providers.gateway]
            base_url = "https://gateway.example/v1"

            [model.via-gateway]
            model = "m"
            context_window = 200000
            model_provider = "gateway"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved.get("via-gateway").expect("model should exist");
        assert!(
            resolve_credentials(model, Some("session-jwt"))
                .api_key
                .is_none(),
            "a fail-closed ref must never resolve a colliding auth_provider table"
        );
        let effective = model
            .effective_auth_provider()
            .expect("fails closed via a provider ref");
        assert!(
            effective.config.command.is_empty(),
            "the fail-closed ref stays unusable despite the name collision"
        );
    }

    #[test]
    fn model_headers_shadow_provider_headers() {
        let raw_config: toml::Value = toml::from_str(
            r#"
            [model_providers.gateway]
            base_url = "https://gateway.example/v1"
            api_key = "sk-provider"

            [model_providers.gateway.extra_headers]
            X-Corp = "yes"

            [model.via-gateway]
            model = "m"
            context_window = 200000
            model_provider = "gateway"

            [model.via-gateway.extra_headers]
            X-Model = "own"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved.get("via-gateway").expect("model should exist");
        assert_eq!(
            model.info.extra_headers.get("X-Model").map(String::as_str),
            Some("own")
        );
        assert!(
            model.info.extra_headers.get("X-Corp").is_none(),
            "a model that sets any header inherits none of the provider's"
        );
    }

    #[test]
    fn model_provider_inline_auth_ttl_and_timeout_warn() {
        use super::super::config_model_override_parse::{ConfigWarningKind, WarningTarget};

        let raw_config: toml::Value = toml::from_str(
            r#"
            [model_providers.gateway]
            base_url = "https://gateway.example/v1"

            [model_providers.gateway.auth]
            command = "printf tok"
            token_ttl_secs = 5
            timeout_secs = 0
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        let has = |field: &str| {
            cfg.config_warnings.iter().any(|w| {
                w.kind == ConfigWarningKind::InvalidValue
                    && matches!(
                        &w.target,
                        WarningTarget::ModelProvider { id, field: f }
                            if id == "gateway" && f.as_deref() == Some(field)
                    )
            })
        };
        assert!(
            has("auth.token_ttl_secs"),
            "inline auth ttl below the refresh margin warns: {:?}",
            cfg.config_warnings
        );
        assert!(
            has("auth.timeout_secs"),
            "inline auth timeout out of range warns: {:?}",
            cfg.config_warnings
        );
    }

    #[test]
    fn blank_api_key_does_not_shadow_provider_auth() {
        let raw_config: toml::Value = toml::from_str(
            r#"
            [model_providers.gateway]
            base_url = "https://gateway.example/v1"

            [model_providers.gateway.auth]
            command = "printf tok"

            [model.m]
            model = "m"
            model_provider = "gateway"
            api_key = "   "
            "#,
        )
        .unwrap();
        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        let resolved = resolve_model_list(&cfg, None);
        let provider = resolved["m"]
            .auth_provider
            .as_ref()
            .expect("blank api_key must not fail-close a working gateway");
        assert_eq!(provider.name.as_str(), "model_provider:gateway");
        assert!(!provider.is_fail_closed());
    }

    #[test]
    fn model_inherits_provider_query_params_and_env_http_headers() {
        let toml_cfg: toml::Value = toml::from_str(
            r#"
            [model_providers.gateway]
            base_url = "https://gateway.example/v1"
            api_key = "sk-provider"

            [model_providers.gateway.query_params]
            api-version = "2026-07-22"

            [model_providers.gateway.env_http_headers]
            X-Tenant-Token = "GATEWAY_TENANT_TOKEN"

            [model.via-gateway]
            model = "m"
            model_provider = "gateway"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&toml_cfg).expect("config should parse");
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved.get("via-gateway").expect("model should exist");
        assert_eq!(
            model
                .info
                .query_params
                .get("api-version")
                .map(String::as_str),
            Some("2026-07-22"),
            "the model inherits the provider's query params"
        );
        assert_eq!(
            model
                .info
                .env_http_headers
                .get("X-Tenant-Token")
                .map(String::as_str),
            Some("GATEWAY_TENANT_TOKEN"),
            "the model inherits the provider's env_http_headers mapping (unresolved names)"
        );
    }

    #[test]
    fn model_query_params_shadow_provider_query_params() {
        let toml_cfg: toml::Value = toml::from_str(
            r#"
            [model_providers.gateway]
            base_url = "https://gateway.example/v1"
            api_key = "sk-provider"

            [model_providers.gateway.query_params]
            api-version = "provider"

            [model.via-gateway]
            model = "m"
            model_provider = "gateway"

            [model.via-gateway.query_params]
            api-version = "model"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&toml_cfg).expect("config should parse");
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved.get("via-gateway").expect("model should exist");
        assert_eq!(
            model
                .info
                .query_params
                .get("api-version")
                .map(String::as_str),
            Some("model"),
            "a model that sets its own query params inherits none of the provider's"
        );
    }

    #[test]
    fn openai_codex_is_reserved_and_cannot_be_overridden() {
        let toml_cfg: toml::Value = toml::from_str(
            r#"
            [model_providers.openai-codex]
            base_url = "https://attacker.invalid/v1"
            api_key = "must-not-survive"

            [model.codex]
            model = "gpt-5.6-sol"
            model_provider = "openai-codex"
            base_url = "https://attacker.invalid/model"
            api_base_url = "https://api.openai.com/v1"
            api_key = "platform-key-must-not-survive"
            env_key = "OPENAI_API_KEY"
            api_backend = "responses"

            [model.codex.extra_headers]
            X-Grok-User-Id = "must-not-survive"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&toml_cfg).expect("config should parse");
        let provider = cfg
            .model_providers
            .get(OPENAI_CODEX_PROVIDER_ID)
            .expect("built-in provider remains registered");
        assert_eq!(
            provider.base_url.as_deref(),
            Some(crate::auth::openai_codex::CODEX_API_BASE_URL)
        );
        assert_eq!(provider.api_backend, Some(ApiBackend::CodexResponses));
        assert!(provider.api_key.is_none());
        assert!(cfg.config_warnings.iter().any(|warning| {
            matches!(
                &warning.target,
                crate::agent::config_model_override_parse::WarningTarget::ModelProvider {
                    id,
                    field: None,
                } if id == OPENAI_CODEX_PROVIDER_ID
            )
        }));

        let resolved = resolve_model_list(&cfg, None);
        let model = resolved.get("codex").expect("Codex model should exist");
        assert_eq!(
            model.info.base_url,
            crate::auth::openai_codex::CODEX_API_BASE_URL
        );
        assert_eq!(model.info.api_backend, ApiBackend::CodexResponses);
        assert_eq!(model.info.auth_scheme, xai_grok_sampler::AuthScheme::Bearer);
        assert!(model.api_base_url.is_none());
        assert!(model.api_key.is_none());
        assert!(model.env_key.is_none());
        assert!(model.info.extra_headers.is_empty());
        assert_eq!(
            model
                .auth_provider
                .as_ref()
                .map(|provider| provider.name.as_str()),
            Some(OPENAI_CODEX_PROVIDER_ID)
        );
    }

    #[test]
    fn direct_openai_codex_auth_provider_cannot_authenticate_a_custom_origin() {
        let toml_cfg: toml::Value = toml::from_str(
            r#"
            [model.exfiltration]
            model = "attacker-model"
            base_url = "https://attacker.invalid/v1"
            api_backend = "responses"
            auth_provider = "openai-codex"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&toml_cfg).expect("config should parse");
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved
            .get("exfiltration")
            .expect("custom model should remain visible but fail closed");
        assert!(
            !model.config_validation_errors.is_empty(),
            "the reserved native provider must require model_provider provenance"
        );
        assert!(
            model
                .auth_provider
                .as_ref()
                .is_some_and(crate::auth::AuthProviderRef::is_fail_closed)
        );

        let credentials = resolve_credentials(model, Some("ambient-xai-session"));
        assert_eq!(credentials.api_key, None);
        let sampler = crate::agent::config::sampling_config_for_model(
            model,
            credentials,
            None,
            None,
            None,
            None,
            &crate::agent::trusted_origins::TrustedXaiOrigins::default(),
        );
        assert_eq!(sampler.base_url, "https://attacker.invalid/v1");
        assert_eq!(sampler.auth_scheme, xai_grok_sampler::AuthScheme::None);
        assert!(sampler.api_key.is_none());
        assert!(sampler.bearer_resolver.is_none());

        let temp = tempfile::tempdir().expect("temporary auth home");
        let manager = std::sync::Arc::new(crate::auth::AuthManager::new_openai_codex(temp.path()));
        manager.hot_swap(crate::auth::GrokAuth {
            key: "native-codex-secret".to_owned(),
            auth_mode: crate::auth::AuthMode::OpenAiCodex,
            ..crate::auth::GrokAuth::default()
        });
        let live_native_provider = crate::auth::AuthProviderRef::openai_codex(manager);
        assert_eq!(
            live_native_provider.cached_token().as_deref(),
            Some("native-codex-secret")
        );
        let mut custom_entry = model.clone();
        custom_entry.config_validation_errors.clear();
        custom_entry.auth_provider = Some(live_native_provider);
        let prefetched = IndexMap::from([("custom-native-ref".to_owned(), custom_entry)]);
        let isolated = resolve_model_list(&Config::default(), Some(prefetched));
        let isolated = &isolated["custom-native-ref"];
        assert!(
            isolated
                .auth_provider
                .as_ref()
                .is_some_and(crate::auth::AuthProviderRef::is_fail_closed)
        );
        assert!(
            resolve_credentials(isolated, None).api_key.is_none(),
            "even a live native Codex token is detached from a custom-origin entry"
        );
    }

    /// Auth home holding the credential a successful `grok login --provider
    /// openai-codex` writes.
    fn live_codex_auth_home() -> tempfile::TempDir {
        let temp = tempfile::tempdir().expect("temporary auth home");
        let auth = crate::auth::GrokAuth {
            key: "live-access-token".to_owned(),
            auth_mode: crate::auth::AuthMode::OpenAiCodex,
            refresh_token: Some("rotating-refresh-token".to_owned()),
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            oidc_issuer: Some(crate::auth::openai_codex::ISSUER.to_owned()),
            oidc_client_id: Some(crate::auth::openai_codex::CLIENT_ID.to_owned()),
            account_id: Some("account-id".to_owned()),
            ..crate::auth::GrokAuth::default()
        };
        let auth_map = std::collections::HashMap::from([(
            crate::auth::openai_codex::AUTH_SCOPE.to_owned(),
            auth,
        )]);
        std::fs::write(
            temp.path().join("auth.json"),
            serde_json::to_vec(&auth_map).unwrap(),
        )
        .unwrap();
        temp
    }

    /// The preset's readiness must be decided by the credential in `auth_home`,
    /// not by whatever `$GROK_HOME` the test runner happens to have.
    fn preset_entry_with_auth_home(
        cfg: &Config,
        auth_home: &std::path::Path,
    ) -> crate::agent::config::ModelEntry {
        let mut resolved = resolve_model_list(cfg, None);
        let mut model = resolved
            .shift_remove(OPENAI_CODEX_PRESET_MODEL_ID)
            .expect("built-in Codex preset should resolve");
        model.auth_provider = Some(crate::auth::AuthProviderRef::openai_codex(
            crate::auth::openai_codex::manager(auth_home),
        ));
        model
    }

    #[test]
    fn builtin_openai_codex_preset_exists_without_user_config() {
        let cfg = Config::new_from_toml_cfg(&toml::Value::Table(toml::map::Map::new()))
            .expect("an empty config should parse");
        let preset = cfg
            .config_models
            .get(OPENAI_CODEX_PRESET_MODEL_ID)
            .expect("the Codex preset must ship without any user config");
        assert_eq!(preset.model.as_deref(), Some(OPENAI_CODEX_PRESET_MODEL_ID));
        assert_eq!(
            preset.model_provider.as_deref(),
            Some(OPENAI_CODEX_PROVIDER_ID)
        );
        assert!(
            preset.api_key.is_none() && preset.env_key.is_none() && preset.base_url.is_none(),
            "the preset must carry no credential or origin of its own"
        );

        let resolved = resolve_model_list(&cfg, None);
        let model = resolved
            .get(OPENAI_CODEX_PRESET_MODEL_ID)
            .expect("the preset must resolve into the catalog");
        assert_eq!(
            model.info.base_url,
            crate::auth::openai_codex::CODEX_API_BASE_URL
        );
        assert_eq!(model.info.api_backend, ApiBackend::CodexResponses);
        assert_eq!(
            model
                .auth_provider
                .as_ref()
                .map(|provider| provider.name.as_str()),
            Some(OPENAI_CODEX_PROVIDER_ID)
        );
        assert!(
            model.is_openai_codex_profile(),
            "the preset must be excluded from the xAI/BYOK key predicates"
        );
    }

    #[test]
    fn builtin_openai_codex_preset_is_unready_before_login() {
        let cfg = Config::new_from_toml_cfg(&toml::Value::Table(toml::map::Map::new()))
            .expect("an empty config should parse");
        let temp = tempfile::tempdir().expect("temporary auth home");
        let model = preset_entry_with_auth_home(&cfg, temp.path());

        let (ready, reason) = crate::agent::config::model_readiness(&model);
        assert!(!ready, "the preset must not be selectable before login");
        let reason = reason.expect("an unready preset must say why");
        // The program name is whatever we were invoked as -- under a unit test
        // that is the test binary, which is the point: the instruction must
        // name the command the user actually has, not a hardcoded `grok` that
        // may belong to a different installed program (#117).
        let expected = format!(
            "{} login --provider openai-codex",
            xai_grok_config::program_name::program_name()
        );
        assert!(
            reason.contains(&expected),
            "the reason must name the login command as invoked, expected {expected:?}, got: {reason}"
        );
    }

    #[test]
    fn builtin_openai_codex_preset_is_selectable_after_login() {
        let cfg = Config::new_from_toml_cfg(&toml::Value::Table(toml::map::Map::new()))
            .expect("an empty config should parse");
        let temp = live_codex_auth_home();
        let model = preset_entry_with_auth_home(&cfg, temp.path());

        assert_eq!(crate::agent::config::model_readiness(&model), (true, None));
        assert_eq!(
            resolve_credentials(&model, Some("xai-session-token"))
                .api_key
                .as_deref(),
            Some("live-access-token"),
            "the preset must sample with the Codex bearer, never the xAI session token"
        );
    }

    #[test]
    fn user_global_config_overrides_the_builtin_openai_codex_preset() {
        let toml_cfg: toml::Value = toml::from_str(&format!(
            r#"
            [model."{OPENAI_CODEX_PRESET_MODEL_ID}"]
            model = "{OPENAI_CODEX_PRESET_MODEL_ID}"
            model_provider = "{OPENAI_CODEX_PROVIDER_ID}"
            name = "My Codex"
            context_window = 123456
            "#
        ))
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&toml_cfg).expect("config should parse");
        assert_eq!(
            cfg.config_models.len(),
            1,
            "the preset must merge into the user's entry, not sit alongside it"
        );
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved
            .get(OPENAI_CODEX_PRESET_MODEL_ID)
            .expect("the user's entry keeps the preset key");
        assert_eq!(model.info.name.as_deref(), Some("My Codex"));
        assert_eq!(model.info.context_window.get(), 123_456);
    }

    /// The docs tell users to redeclare the preset key to retune its metadata.
    /// Such an override names no endpoint and no credential, so it must not
    /// take the Codex routing down with it: without the preset underneath, the
    /// entry resolves as a plain xAI catalog model and authenticates with the
    /// user's xAI session token under a key documented as Codex.
    #[test]
    fn metadata_only_override_keeps_the_preset_codex_routing() {
        let toml_cfg: toml::Value = toml::from_str(&format!(
            r#"
            [model."{OPENAI_CODEX_PRESET_MODEL_ID}"]
            name = "Codex"
            context_window = 400000
            "#
        ))
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&toml_cfg).expect("config should parse");
        let auth_home = tempfile::tempdir().expect("temporary auth home");
        let model = preset_entry_with_auth_home(&cfg, auth_home.path());

        assert_eq!(model.info.name.as_deref(), Some("Codex"));
        assert_eq!(model.info.context_window.get(), 400_000);
        assert_eq!(
            model.info.base_url,
            crate::auth::openai_codex::CODEX_API_BASE_URL,
            "a metadata-only override must not re-point the key at xAI"
        );
        assert_eq!(model.info.api_backend, ApiBackend::CodexResponses);
        assert_eq!(
            model
                .auth_provider
                .as_ref()
                .map(|provider| provider.name.as_str()),
            Some(OPENAI_CODEX_PROVIDER_ID)
        );
        assert!(
            resolve_credentials(&model, Some("xai-session-token"))
                .api_key
                .is_none(),
            "the xAI session token must never authenticate a Codex-keyed model"
        );
    }

    /// Drift guard: every field the preset ships must survive a metadata-only
    /// override. A preset field that [`merge_openai_codex_presets`] forgets to
    /// overlay would silently disappear from the picker.
    #[test]
    fn every_preset_field_survives_a_metadata_only_override() {
        let preset = openai_codex_preset_models()
            .shift_remove(OPENAI_CODEX_PRESET_MODEL_ID)
            .expect("the preset should be keyed by its model id");
        let preset_value = toml::Value::try_from(&preset).expect("preset serializes");
        let preset_table = preset_value.as_table().expect("preset is a table");

        // Sets one field the preset does not, and declares no routing.
        let mut models = IndexMap::from([(
            OPENAI_CODEX_PRESET_MODEL_ID.to_owned(),
            ConfigModelOverride {
                top_p: Some(0.5),
                ..ConfigModelOverride::default()
            },
        )]);
        merge_openai_codex_presets(&mut models);
        let merged_value = toml::Value::try_from(&models[OPENAI_CODEX_PRESET_MODEL_ID])
            .expect("merged serializes");
        let merged_table = merged_value.as_table().expect("merged is a table");

        for (field, value) in preset_table {
            assert_eq!(
                merged_table.get(field),
                Some(value),
                "preset field `{field}` was dropped by a metadata-only override; \
                 overlay it in merge_openai_codex_presets"
            );
        }
        assert_eq!(
            merged_table.get("top_p").and_then(toml::Value::as_float),
            Some(0.5),
            "the user's own field must survive the overlay"
        );
    }

    /// The flip side: an override that names its own endpoint and credential
    /// has taken the key over, so the preset must not clamp it back onto Codex
    /// (which would silently strip that endpoint and key).
    #[test]
    fn override_declaring_its_own_endpoint_is_left_alone() {
        let toml_cfg: toml::Value = toml::from_str(&format!(
            r#"
            [model."{OPENAI_CODEX_PRESET_MODEL_ID}"]
            model = "gpt-5.6-sol"
            base_url = "https://api.openai.com/v1"
            env_key = "OPENAI_API_KEY"
            context_window = 200000
            "#
        ))
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&toml_cfg).expect("config should parse");
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved
            .get(OPENAI_CODEX_PRESET_MODEL_ID)
            .expect("the user's entry should resolve");

        assert_eq!(model.info.base_url, "https://api.openai.com/v1");
        assert_eq!(
            model.env_key.as_ref().map(|keys| keys.names()),
            Some(vec!["OPENAI_API_KEY"]),
            "the preset must not strip a credential the user declared"
        );
        assert_ne!(model.info.api_backend, ApiBackend::CodexResponses);
    }

    #[test]
    fn openai_codex_model_without_a_live_credential_is_unready() {
        let toml_cfg: toml::Value = toml::from_str(
            r#"
            [model.codex]
            model = "gpt-5.6-sol"
            model_provider = "openai-codex"
            "#,
        )
        .unwrap();
        let cfg = Config::new_from_toml_cfg(&toml_cfg).expect("config should parse");
        let mut resolved = resolve_model_list(&cfg, None);
        let model = resolved.get_mut("codex").expect("Codex model should exist");
        let temp = tempfile::tempdir().expect("temporary auth home");
        model.auth_provider = Some(crate::auth::AuthProviderRef::openai_codex(
            crate::auth::openai_codex::manager(temp.path()),
        ));

        let (ready, reason) = crate::agent::config::model_readiness(model);
        assert!(!ready);
        assert!(
            reason
                .as_deref()
                .is_some_and(|reason| reason.contains("login"))
        );
        assert!(
            resolve_credentials(model, Some("xai-session-token"))
                .api_key
                .is_none()
        );
    }

    #[test]
    fn expired_refreshable_openai_codex_credential_stays_selectable() {
        let toml_cfg: toml::Value = toml::from_str(
            r#"
            [model.codex]
            model = "gpt-5.6-sol"
            model_provider = "openai-codex"
            "#,
        )
        .unwrap();
        let cfg = Config::new_from_toml_cfg(&toml_cfg).expect("config should parse");
        let mut resolved = resolve_model_list(&cfg, None);
        let model = resolved.get_mut("codex").expect("Codex model should exist");

        let temp = tempfile::tempdir().expect("temporary auth home");
        let auth = crate::auth::GrokAuth {
            key: "expired-access-token".to_owned(),
            auth_mode: crate::auth::AuthMode::OpenAiCodex,
            refresh_token: Some("rotating-refresh-token".to_owned()),
            expires_at: Some(chrono::Utc::now() - chrono::Duration::minutes(1)),
            oidc_issuer: Some(crate::auth::openai_codex::ISSUER.to_owned()),
            oidc_client_id: Some(crate::auth::openai_codex::CLIENT_ID.to_owned()),
            account_id: Some("account-id".to_owned()),
            ..crate::auth::GrokAuth::default()
        };
        let auth_map = std::collections::HashMap::from([(
            crate::auth::openai_codex::AUTH_SCOPE.to_owned(),
            auth,
        )]);
        std::fs::write(
            temp.path().join("auth.json"),
            serde_json::to_vec(&auth_map).unwrap(),
        )
        .unwrap();
        model.auth_provider = Some(crate::auth::AuthProviderRef::openai_codex(
            crate::auth::openai_codex::manager(temp.path()),
        ));

        assert_eq!(crate::agent::config::model_readiness(model), (true, None));
        assert!(
            resolve_credentials(model, Some("xai-session-token"))
                .api_key
                .is_none(),
            "an expired Codex bearer must refresh pre-turn, never fall back to xAI"
        );
    }

    /// #260. `ConfigModelOverride::apply` seeds the wire model from the TOML
    /// key, and merge only looks up catalog keys, so a display-name key
    /// silently became a ready Codex row that 400s every turn.
    #[test]
    fn codex_wire_model_catalog_slug_rejects_non_slug_before_request_io() {
        let toml_cfg: toml::Value = toml::from_str(
            r#"
            [model."GPT-5.3-Codex-Spark"]
            model_provider = "openai-codex"
            "#,
        )
        .unwrap();
        let cfg = Config::new_from_toml_cfg(&toml_cfg).expect("config should parse");
        let temp = live_codex_auth_home();
        let mut resolved = resolve_model_list(&cfg, None);
        let mut model = resolved
            .shift_remove("GPT-5.3-Codex-Spark")
            .expect("user Codex entry should resolve");
        model.auth_provider = Some(crate::auth::AuthProviderRef::openai_codex(
            crate::auth::openai_codex::manager(temp.path()),
        ));

        let (ready, reason) = crate::agent::config::model_readiness(&model);
        assert!(
            !ready,
            "a Codex-routed non-catalog wire model must be unready before request I/O"
        );
        let reason = reason.expect("an unready Codex row must say why");
        assert!(
            reason.contains("GPT-5.3-Codex-Spark"),
            "reason must name the bad wire model, got: {reason}"
        );
        assert!(
            reason.contains("not a Codex catalog slug"),
            "reason must say the wire model is not a Codex catalog slug, got: {reason}"
        );
    }

    /// #260. A metadata-only override of a real catalog slug stays ready
    /// when Codex credentials are present.
    #[test]
    fn codex_wire_model_catalog_slug_accepts_metadata_override_when_signed_in() {
        let toml_cfg: toml::Value = toml::from_str(&format!(
            r#"
            [model."{OPENAI_CODEX_PRESET_MODEL_ID}"]
            name = "My Codex"
            "#
        ))
        .unwrap();
        let cfg = Config::new_from_toml_cfg(&toml_cfg).expect("config should parse");
        let temp = live_codex_auth_home();
        let model = preset_entry_with_auth_home(&cfg, temp.path());

        assert_eq!(
            model.info.model.as_str(),
            OPENAI_CODEX_PRESET_MODEL_ID,
            "metadata-only override must keep the catalog wire slug"
        );
        assert_eq!(
            crate::agent::config::model_readiness(&model),
            (true, None),
            "a real Codex catalog slug must stay ready when credentials are present"
        );
    }

    /// #260, second done-criterion: "an unavailable catalog does not make valid
    /// entries unready". The check must be "the catalog said no", not "the
    /// catalog did not say yes".
    ///
    /// `codex_catalog_fallback_models` stamps `catalog_degraded_reason` on
    /// every entry it hands back, whether that is a saved cache or the single
    /// built-in preset. Stamping unknown slugs off a degraded catalog therefore
    /// marks a user's perfectly valid hand-written entry unready whenever the
    /// live refresh fails — reachable through `resolve_remote_fetch_enabled`,
    /// which is a supported setting, not an error path.
    ///
    /// This calls `merge_openai_codex_preset_entries` directly and hands it a
    /// degraded preset set. Going through the normal path cannot reproduce it:
    /// `fetch_openai_codex_catalog_models` returns `None` under `cfg!(test)`,
    /// so the effective catalog in unit tests is always the one built-in slug,
    /// and the two tests above happen to use exactly that slug. The bug is
    /// structurally invisible to them.
    #[test]
    fn codex_wire_model_catalog_slug_stays_ready_when_catalog_is_degraded() {
        let mut config_models = IndexMap::from([(
            "gpt-5.3-codex-spark".to_owned(),
            ConfigModelOverride {
                model_provider: Some(OPENAI_CODEX_PROVIDER_ID.to_owned()),
                ..ConfigModelOverride::default()
            },
        )]);
        let degraded = mark_codex_catalog_degraded(
            openai_codex_preset_models(),
            OPENAI_CODEX_BUILTIN_FALLBACK_REASON,
        );

        merge_openai_codex_preset_entries(
            &mut config_models,
            CodexCatalogListing::stand_in(degraded),
        );

        assert_eq!(
            config_models["gpt-5.3-codex-spark"].unknown_codex_catalog_slug, None,
            "a degraded catalog must not deny a slug it simply never listed"
        );
    }

    /// The counterweight the pair above was missing: "the catalog did not say
    /// yes" must not be reached by a route other than degradation. A catalog
    /// the server *did* serve keeps speaking for every slug even when one of
    /// its rows wants a newer client.
    ///
    /// Deriving authority from `catalog_degraded_reason` conflated the two
    /// writers of that field — a per-model version floor and a stand-in
    /// listing — so a single above-floor row cleared every
    /// `unknown_codex_catalog_slug` marker and let a wire model the account
    /// cannot use pass readiness and 400 on every turn.
    #[test]
    fn codex_wire_model_catalog_slug_is_rejected_when_a_served_catalog_has_a_version_floor() {
        let served = parse_openai_codex_catalog_models(&serde_json::json!({
            "models": [
                {
                    "slug": "gpt-5.6-sol",
                    "display_name": "GPT-5.6 Sol",
                    "minimal_client_version": "0.100.0",
                    "context_window": 272000
                }
            ]
        }));
        assert!(
            served["gpt-5.6-sol"].catalog_degraded_reason.is_some(),
            "the floor must stamp the row, or this test proves nothing"
        );

        let mut config_models = IndexMap::from([(
            "gpt-5.3-codex-spark".to_owned(),
            ConfigModelOverride {
                model_provider: Some(OPENAI_CODEX_PROVIDER_ID.to_owned()),
                ..ConfigModelOverride::default()
            },
        )]);

        merge_openai_codex_preset_entries(&mut config_models, CodexCatalogListing::served(served));

        assert_eq!(
            config_models["gpt-5.3-codex-spark"]
                .unknown_codex_catalog_slug
                .as_deref(),
            Some("gpt-5.3-codex-spark"),
            "a served catalog enumerates every slug; one row wanting a newer \
             client says nothing about the others"
        );
    }

    /// The other half: a stamp left by an earlier authoritative refresh must
    /// not survive into a degraded one. Skipping the stamping step alone would
    /// leave the stale `Some(..)` in place and keep the row unready for the
    /// rest of the session, which is the same user-visible bug arriving by a
    /// slower route.
    #[test]
    fn codex_wire_model_catalog_slug_clears_stale_stamp_on_degraded_refresh() {
        let mut config_models = IndexMap::from([(
            "gpt-5.3-codex-spark".to_owned(),
            ConfigModelOverride {
                model_provider: Some(OPENAI_CODEX_PROVIDER_ID.to_owned()),
                unknown_codex_catalog_slug: Some("gpt-5.3-codex-spark".to_owned()),
                ..ConfigModelOverride::default()
            },
        )]);
        let degraded = mark_codex_catalog_degraded(
            openai_codex_preset_models(),
            OPENAI_CODEX_BUILTIN_FALLBACK_REASON,
        );

        merge_openai_codex_preset_entries(
            &mut config_models,
            CodexCatalogListing::stand_in(degraded),
        );

        assert_eq!(
            config_models["gpt-5.3-codex-spark"].unknown_codex_catalog_slug, None,
            "a degraded refresh must clear a stamp it can no longer justify"
        );
    }

    /// The shape below is not invented: it is the live `GET /models` response,
    /// captured 2026-08-08, trimmed to the fields this parser reads. Three
    /// things in it are the whole point, because a parser written from the
    /// issue text got all three wrong and would have fallen silently back to
    /// the built-in preset forever:
    ///
    ///   - the array is under `models`, not `data`
    ///   - entries key on `slug`, and carry no `id` or `model` at all
    ///   - the human label is `display_name`, not `name`
    #[test]
    fn codex_catalog_parser_reads_the_shape_the_endpoint_actually_returns() {
        let payload = serde_json::json!({
            "models": [
                {
                    "slug": "gpt-5.6-sol",
                    "display_name": "GPT-5.6 Sol",
                    "description": "Flagship",
                    "context_window": 272000,
                    "max_context_window": 272000
                },
                {
                    "slug": "gpt-5.3-codex-spark",
                    "display_name": "GPT-5.3 Codex Spark",
                    "context_window": 128000
                }
            ]
        });
        let presets = parse_openai_codex_catalog_models(&payload);
        assert_eq!(presets.len(), 2, "both live entries must parse");

        let sol = presets.get("gpt-5.6-sol").expect("slug is the catalog key");
        assert_eq!(sol.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(
            sol.model_provider.as_deref(),
            Some(OPENAI_CODEX_PROVIDER_ID)
        );
        assert_eq!(sol.name.as_deref(), Some("GPT-5.6 Sol"));
        assert_eq!(sol.context_window, Some(272_000));

        let spark = presets
            .get("gpt-5.3-codex-spark")
            .expect("a second model must not be lost — one entry is the bug");
        assert_eq!(spark.context_window, Some(128_000));
    }

    /// #265: parse per-entry `minimal_client_version` and top-level
    /// `client_version`. Only `minimal_client_version` is a floor — the live
    /// publisher field `0.147.0` must not degrade a catalog the server still
    /// serves to advertised `0.0.0`. Drive apply + ACP reason, no network.
    #[test]
    fn codex_catalog_parser_reads_minimal_client_version() {
        let payload = serde_json::json!({
            "client_version": "0.147.0",
            "models": [
                {
                    "slug": "gpt-5.6-sol",
                    "display_name": "GPT-5.6 Sol",
                    "minimal_client_version": "0.100.0",
                    "context_window": 272000
                },
                {
                    "slug": "gpt-5.6-terra",
                    "display_name": "GPT-5.6 Terra",
                    "minimal_client_version": "0.0.0",
                    "context_window": 272000
                },
                {
                    "slug": "gpt-5.5",
                    "display_name": "GPT-5.5",
                    "context_window": 272000
                }
            ]
        });
        assert_eq!(
            codex_catalog_payload_client_version(&payload).as_deref(),
            Some("0.147.0"),
            "top-level catalog client_version must be parsed when present"
        );
        let presets = parse_openai_codex_catalog_models(&payload);
        let below_floor_reason = openai_codex_catalog_client_version_floor_reason("0.100.0");
        assert_eq!(
            presets["gpt-5.6-sol"].catalog_degraded_reason.as_deref(),
            Some(below_floor_reason.as_str()),
            "a floor above advertised 0.0.0 must stamp catalog-degraded"
        );
        assert!(
            presets["gpt-5.6-terra"].catalog_degraded_reason.is_none(),
            "an equal floor must not degrade"
        );
        assert!(
            presets["gpt-5.5"].catalog_degraded_reason.is_none(),
            "a missing floor must not degrade"
        );

        let publisher_only = parse_openai_codex_catalog_models(&serde_json::json!({
            "client_version": "0.147.0",
            "models": [{
                "slug": "gpt-5.6-sol",
                "display_name": "GPT-5.6 Sol",
                "context_window": 272000
            }]
        }));
        assert!(
            publisher_only["gpt-5.6-sol"]
                .catalog_degraded_reason
                .is_none(),
            "top-level client_version is the publisher version, not a floor"
        );

        let catalog_wide = parse_openai_codex_catalog_models(&serde_json::json!({
            "client_version": "0.147.0",
            "minimal_client_version": "1.0.0",
            "models": [{
                "slug": "gpt-5.6-sol",
                "display_name": "GPT-5.6 Sol",
                "context_window": 272000
            }]
        }));
        assert_eq!(
            catalog_wide["gpt-5.6-sol"]
                .catalog_degraded_reason
                .as_deref(),
            Some(openai_codex_catalog_client_version_floor_reason("1.0.0").as_str())
        );

        let mut cfg = Config::new_from_toml_cfg(&toml::Value::Table(toml::map::Map::new()))
            .expect("empty config");
        cfg.config_models = presets;
        let resolved = resolve_model_list(&cfg, None);
        let sol = resolved.get("gpt-5.6-sol").expect("parsed Sol");
        assert_eq!(
            sol.info.catalog_degraded_reason.as_deref(),
            Some(below_floor_reason.as_str())
        );
        let (_ready, readiness_reason) = crate::agent::config::model_readiness(sol);
        assert_ne!(
            readiness_reason.as_deref(),
            Some(below_floor_reason.as_str()),
            "version floor is catalog-degraded state, not a readiness block"
        );
        let meta = crate::agent::config::to_acp_model_info(&resolved)
            .get(&acp::ModelId::new("gpt-5.6-sol"))
            .and_then(|model| model.meta.as_ref())
            .cloned()
            .expect("Sol ACP metadata");
        assert_eq!(
            meta.get("catalogDegradedReason")
                .and_then(serde_json::Value::as_str),
            Some(below_floor_reason.as_str())
        );
        let acp_models = crate::agent::config::to_acp_model_info(&resolved);
        let terra_meta = acp_models
            .get(&acp::ModelId::new("gpt-5.6-terra"))
            .and_then(|model| model.meta.as_ref())
            .expect("Terra ACP metadata");
        assert!(
            !terra_meta.contains_key("catalogDegradedReason"),
            "an in-range floor must not synthesize degraded ACP state"
        );
    }

    /// #267: live-shape `upgrade` on gpt-5.4 carries the target and markdown
    /// through parse → apply → ACP. The selected wire model must stay gpt-5.4.
    #[test]
    fn codex_catalog_parser_reads_upgrade_migration() {
        let migration = "GPT-5.4 will be deprecated soon\n\n\
                         Codex now uses GPT-5.6 Terra in place of GPT-5.4. Switch to GPT-5.6 Terra to continue.\n";
        let payload = serde_json::json!({
            "models": [
                {
                    "slug": "gpt-5.4",
                    "display_name": "GPT-5.4",
                    "model": "gpt-5.4",
                    "context_window": 272000,
                    "max_context_window": 1000000,
                    "upgrade": {
                        "model": "gpt-5.6-terra",
                        "migration_markdown": migration
                    }
                },
                {
                    "slug": "gpt-5.6-terra",
                    "display_name": "GPT-5.6 Terra",
                    "context_window": 272000
                }
            ]
        });
        let presets = parse_openai_codex_catalog_models(&payload);
        let selected = presets.get("gpt-5.4").expect("gpt-5.4 catalog entry");
        assert_eq!(
            selected.model.as_deref(),
            Some("gpt-5.4"),
            "upgrade must not rewrite the selected wire model"
        );
        let upgrade = selected
            .catalog_upgrade
            .as_ref()
            .expect("gpt-5.4 must carry the live-shape upgrade");
        assert_eq!(upgrade.model, "gpt-5.6-terra");
        assert_eq!(
            upgrade.migration_markdown,
            migration.trim_end(),
            "catalog markdown is stored trimmed; trailing newline is not semantic"
        );
        assert!(
            presets["gpt-5.6-terra"].catalog_upgrade.is_none(),
            "a current model must not inherit another entry's upgrade"
        );

        let mut user_models = IndexMap::from([(
            "gpt-5.4".to_owned(),
            ConfigModelOverride {
                name: Some("My 5.4".to_owned()),
                ..ConfigModelOverride::default()
            },
        )]);
        merge_openai_codex_preset_entries(
            &mut user_models,
            CodexCatalogListing::served(presets.clone()),
        );
        assert_eq!(
            user_models["gpt-5.4"].model.as_deref(),
            Some("gpt-5.4"),
            "metadata-only overlay must not auto-switch the selected model"
        );
        assert_eq!(
            user_models["gpt-5.4"]
                .catalog_upgrade
                .as_ref()
                .map(|upgrade| upgrade.model.as_str()),
            Some("gpt-5.6-terra"),
            "catalog upgrade must survive a metadata-only overlay"
        );

        let mut cfg = Config::new_from_toml_cfg(&toml::Value::Table(toml::map::Map::new()))
            .expect("empty config");
        cfg.config_models = presets;
        let resolved = resolve_model_list(&cfg, None);
        let applied = resolved.get("gpt-5.4").expect("applied gpt-5.4");
        assert_eq!(applied.info.model, "gpt-5.4");
        let applied_upgrade = applied
            .info
            .catalog_upgrade
            .as_ref()
            .expect("upgrade survives apply");
        assert_eq!(applied_upgrade.model, "gpt-5.6-terra");
        assert_eq!(applied_upgrade.migration_markdown, migration.trim_end());
        let meta = crate::agent::config::to_acp_model_info(&resolved)
            .get(&acp::ModelId::new("gpt-5.4"))
            .and_then(|model| model.meta.as_ref())
            .cloned()
            .expect("gpt-5.4 ACP metadata");
        assert_eq!(
            meta.get("modelSlug").and_then(serde_json::Value::as_str),
            Some("gpt-5.4"),
            "ACP must keep the selected slug"
        );
        assert_eq!(
            meta.get("catalogUpgradeModel")
                .and_then(serde_json::Value::as_str),
            Some("gpt-5.6-terra")
        );
        assert_eq!(
            meta.get("catalogUpgradeMigrationMarkdown")
                .and_then(serde_json::Value::as_str),
            Some(migration.trim_end())
        );
    }

    /// #245: catalog entries that differ from the Sol preset on wire flags
    /// must not be collapsed onto the preset's capabilities. The live table
    /// in the issue is the fixture; the point is that Spark's flags reach
    /// `codex_wire` instead of being dropped at parse time.
    #[test]
    fn codex_catalog_parser_reads_wire_capabilities_that_differ_from_the_preset() {
        let payload = serde_json::json!({
            "models": [
                {
                    "slug": "gpt-5.6-sol",
                    "display_name": "GPT-5.6 Sol",
                    "context_window": 272000,
                    "use_responses_lite": true,
                    "tool_mode": "code_mode_only",
                    "supports_reasoning_summary_parameter": true,
                    "supports_image_detail_original": true,
                    "input_modalities": ["text", "image"],
                    "default_reasoning_level": "low",
                    "effective_context_window_percent": 95
                },
                {
                    "slug": "gpt-5.3-codex-spark",
                    "display_name": "GPT-5.3 Codex Spark",
                    "context_window": 128000,
                    "use_responses_lite": false,
                    "tool_mode": null,
                    "supports_reasoning_summary_parameter": false,
                    "supports_image_detail_original": false,
                    "input_modalities": ["text"],
                    "default_reasoning_level": "high"
                }
            ]
        });
        let presets = parse_openai_codex_catalog_models(&payload);
        let sol = presets
            .get("gpt-5.6-sol")
            .expect("sol")
            .codex_wire
            .as_ref()
            .expect("sol wire caps");
        let spark = presets
            .get("gpt-5.3-codex-spark")
            .expect("spark")
            .codex_wire
            .as_ref()
            .expect("spark wire caps");

        assert_eq!(sol.use_responses_lite, Some(true));
        assert_eq!(spark.use_responses_lite, Some(false));
        assert_eq!(
            presets
                .get("gpt-5.6-sol")
                .and_then(|entry| entry.effective_context_window_percent),
            Some(95)
        );
        assert_eq!(sol.tool_mode.as_deref(), Some("code_mode_only"));
        assert_eq!(spark.tool_mode, None);
        assert_eq!(sol.supports_reasoning_summary_parameter, Some(true));
        assert_eq!(spark.supports_reasoning_summary_parameter, Some(false));
        assert_eq!(
            sol.input_modalities,
            vec!["text".to_owned(), "image".to_owned()]
        );
        assert_eq!(spark.input_modalities, vec!["text".to_owned()]);
        assert_eq!(sol.default_reasoning_level.as_deref(), Some("low"));
        assert_eq!(spark.default_reasoning_level.as_deref(), Some("high"));
        assert!(
            !spark.include_reasoning_summary(),
            "Spark must not inherit Sol's summary parameter"
        );
        assert!(sol.include_reasoning_summary());

        let spark_entry = presets.get("gpt-5.3-codex-spark").unwrap();
        assert_eq!(
            spark_entry.reasoning_effort,
            Some(xai_grok_sampling_types::ReasoningEffort::High),
            "catalog default_reasoning_level must become reasoning_effort"
        );
    }

    /// #274: a Sol catalog `use_responses_lite: true` must reach the shipped
    /// request builder; Spark's `false` must not.
    #[test]
    fn sol_catalog_use_responses_lite_reaches_the_request_builder() {
        let payload = serde_json::json!({
            "models": [
                {
                    "slug": "gpt-5.6-sol",
                    "use_responses_lite": true,
                    "context_window": 272000
                },
                {
                    "slug": "gpt-5.3-codex-spark",
                    "use_responses_lite": false,
                    "context_window": 128000
                }
            ]
        });
        let presets = parse_openai_codex_catalog_models(&payload);
        let sol_caps = presets
            .get("gpt-5.6-sol")
            .and_then(|entry| entry.codex_wire.clone())
            .expect("Sol wire caps");
        let spark_caps = presets
            .get("gpt-5.3-codex-spark")
            .and_then(|entry| entry.codex_wire.clone())
            .expect("Spark wire caps");
        assert!(sol_caps.responses_lite_enabled());
        assert!(!spark_caps.responses_lite_enabled());

        let mut sol_body = serde_json::json!({
            "model": "gpt-5.6-sol",
            "parallel_tool_calls": true,
            "reasoning": { "effort": "low", "summary": "concise" }
        });
        xai_grok_sampling_types::apply_codex_wire_capabilities(&mut sol_body, &sol_caps);
        assert_eq!(
            sol_body["reasoning"]["context"], "all_turns",
            "Sol catalog lite flag must reshape the request: {sol_body}"
        );
        assert_eq!(sol_body["parallel_tool_calls"], false);

        let mut spark_body = serde_json::json!({
            "model": "gpt-5.3-codex-spark",
            "parallel_tool_calls": true,
            "reasoning": { "effort": "high", "summary": "concise" }
        });
        xai_grok_sampling_types::apply_codex_wire_capabilities(&mut spark_body, &spark_caps);
        assert!(
            spark_body["reasoning"].get("context").is_none(),
            "Spark must not inherit Sol's lite body: {spark_body}"
        );
        assert_eq!(spark_body["parallel_tool_calls"], true);
    }

    /// #264: every live Codex catalog model reports 95; that value is the
    /// catalog auto-compact tier, not a user `[model.<id>]` override.
    #[test]
    fn parse_codex_catalog_effective_context_window_percent() {
        let parsed = parse_openai_codex_catalog_entry(&serde_json::json!({
            "slug": "gpt-5.6-sol",
            "context_window": 272000,
            "effective_context_window_percent": 95
        }))
        .expect("valid catalog entry")
        .1;
        assert_eq!(parsed.effective_context_window_percent, Some(95));
        assert_eq!(
            parsed.auto_compact_threshold_percent, None,
            "catalog percent must not be written into the user-per-model field"
        );

        let ignored = parse_openai_codex_catalog_entry(&serde_json::json!({
            "slug": "bad-percent",
            "effective_context_window_percent": 140
        }))
        .expect("entry remains usable")
        .1;
        assert_eq!(ignored.effective_context_window_percent, None);
    }

    /// #259: catalog `auto_compact_token_limit` is an absolute compact
    /// trigger. `null` / 0 / garbage stay `None` so percent-of-window remains.
    #[test]
    fn codex_catalog_parser_reads_auto_compact_token_limit() {
        let present = parse_openai_codex_catalog_entry(&serde_json::json!({
            "slug": "gpt-5.6-sol",
            "auto_compact_token_limit": 180_000
        }))
        .expect("valid catalog entry")
        .1;
        assert_eq!(
            present
                .codex_wire
                .as_ref()
                .and_then(|wire| wire.auto_compact_token_limit),
            Some(180_000)
        );

        for invalid in [
            serde_json::Value::Null,
            serde_json::json!(0),
            serde_json::json!(-1),
            serde_json::json!("180000"),
        ] {
            let parsed = parse_openai_codex_catalog_entry(&serde_json::json!({
                "slug": "gpt-codex-invalid-limit",
                "auto_compact_token_limit": invalid,
            }))
            .expect("entry remains usable")
            .1;
            assert_eq!(
                parsed
                    .codex_wire
                    .and_then(|wire| wire.auto_compact_token_limit),
                None
            );
        }
    }

    /// #261: parse keeps `supports_image_detail_original` and
    /// `base_instructions` / `model_messages` so apply/patch can read them.
    #[test]
    fn codex_catalog_parser_keeps_image_detail_original_and_instructions() {
        let parsed = parse_openai_codex_catalog_entry(&serde_json::json!({
            "slug": "gpt-5.6-sol",
            "supports_image_detail_original": true,
            "base_instructions": "legacy base",
            "model_messages": {
                "instructions_template": "template wins",
                "approvals": null
            }
        }))
        .expect("valid catalog entry")
        .1;
        let caps = parsed.codex_wire.expect("wire capabilities");
        assert_eq!(caps.supports_image_detail_original, Some(true));
        assert!(caps.allows_image_detail_original());
        assert_eq!(caps.base_instructions.as_deref(), Some("legacy base"));
        let messages = caps.model_messages.as_ref().expect("model_messages kept");
        assert_eq!(
            messages.instructions_template.as_deref(),
            Some("template wins")
        );
        assert!(messages.extra.contains_key("approvals"));
        assert_eq!(caps.catalog_instructions(), Some("template wins"));
    }

    #[test]
    fn codex_catalog_parser_reads_and_validates_truncation_policy() {
        use xai_grok_sampling_types::{TruncationMode, TruncationPolicyConfig};

        let valid = parse_openai_codex_catalog_entry(&serde_json::json!({
            "slug": "gpt-5.6-sol",
            "truncation_policy": { "mode": "tokens", "limit": 10000 }
        }))
        .expect("valid catalog entry")
        .1;
        let capabilities = valid.codex_wire.expect("Codex wire capabilities");
        assert_eq!(
            capabilities.truncation_policy,
            Some(TruncationPolicyConfig {
                mode: TruncationMode::Tokens,
                limit: 10_000,
            })
        );
        let round_trip: xai_grok_sampling_types::CodexWireCapabilities = serde_json::from_value(
            serde_json::to_value(&capabilities).expect("serialize capabilities"),
        )
        .expect("deserialize capabilities");
        assert_eq!(round_trip, capabilities);

        for invalid in [
            serde_json::json!({ "mode": "characters", "limit": 10000 }),
            serde_json::json!({ "mode": "tokens", "limit": 0 }),
            serde_json::json!({ "mode": "bytes", "limit": -1 }),
        ] {
            let parsed = parse_openai_codex_catalog_entry(&serde_json::json!({
                "slug": "invalid-policy-model",
                "truncation_policy": invalid,
            }))
            .expect("entry remains usable when its policy is invalid")
            .1;
            assert_eq!(
                parsed.codex_wire.and_then(|wire| wire.truncation_policy),
                None,
                "invalid units and non-positive limits must fail closed"
            );
        }
    }

    /// #261: the catalog's per-model list must survive through resolved
    /// model metadata and ACP so the TUI does not offer the legacy full menu to
    /// models that accept only a subset.
    #[test]
    fn codex_catalog_supported_reasoning_levels_drive_acp_effort_options() {
        let payload = serde_json::json!({
            "models": [
                {
                    "slug": "gpt-codex-a",
                    "default_reasoning_level": "low",
                    "supported_reasoning_levels": [
                        { "effort": "low", "description": "Fast" },
                        { "effort": "high", "description": "Deep" }
                    ]
                },
                {
                    "slug": "gpt-codex-b",
                    "default_reasoning_level": "xhigh",
                    "supported_reasoning_levels": [
                        { "effort": "medium", "description": "Balanced" },
                        { "effort": "xhigh", "description": "Maximum" }
                    ]
                }
            ]
        });
        let presets = parse_openai_codex_catalog_models(&payload);
        let a = presets.get("gpt-codex-a").expect("model a");
        let b = presets.get("gpt-codex-b").expect("model b");
        assert_eq!(
            a.reasoning_efforts
                .iter()
                .map(|option| option.value)
                .collect::<Vec<_>>(),
            vec![
                xai_grok_sampling_types::ReasoningEffort::Low,
                xai_grok_sampling_types::ReasoningEffort::High,
            ]
        );
        assert_eq!(a.reasoning_efforts[0].description.as_deref(), Some("Fast"));
        assert!(a.reasoning_efforts[0].default);
        assert_eq!(
            a.reasoning_effort,
            Some(xai_grok_sampling_types::ReasoningEffort::Low)
        );
        assert!(!a.reasoning_efforts[1].default);
        assert_eq!(
            b.reasoning_effort,
            Some(xai_grok_sampling_types::ReasoningEffort::Xhigh)
        );
        assert!(b.reasoning_efforts[1].default);

        let cfg = Config::default();
        let prefetched = presets
            .into_iter()
            .map(|(key, model)| {
                let entry = model.apply(&key, None, &cfg.endpoints);
                (key, entry)
            })
            .collect();
        let resolved = resolve_model_list(&cfg, Some(prefetched));
        let acp_models = crate::agent::config::to_acp_model_info(&resolved);
        let a_meta = acp_models
            .get(&acp::ModelId::new("gpt-codex-a"))
            .and_then(|model| model.meta.as_ref())
            .expect("model a ACP metadata");
        let b_meta = acp_models
            .get(&acp::ModelId::new("gpt-codex-b"))
            .and_then(|model| model.meta.as_ref())
            .expect("model b ACP metadata");
        assert_eq!(a_meta["reasoningEfforts"][0]["value"], "low");
        assert_eq!(a_meta["reasoningEfforts"][1]["value"], "high");
        assert_eq!(a_meta["reasoningEffort"], "low");
        assert_eq!(b_meta["reasoningEfforts"][0]["value"], "medium");
        assert_eq!(b_meta["reasoningEfforts"][1]["value"], "xhigh");
        assert_eq!(b_meta["reasoningEffort"], "xhigh");
    }

    /// #357: keep the shipped Codex picker aligned with the complete live
    /// catalog snapshot. This is intentionally an exact matrix: accepting
    /// Ultra on Luna (or omitting it from Terra) is a release-blocking
    /// capability mismatch, not a harmless presentation difference.
    #[test]
    fn codex_live_catalog_reasoning_capability_matrix_is_exact() {
        use xai_grok_sampling_types::ReasoningEffort;

        let payload = serde_json::json!({
            "models": [
                {
                    "slug": "gpt-5.6-sol",
                    "display_name": "GPT-5.6-Sol",
                    "default_reasoning_level": "low",
                    "supported_reasoning_levels": [
                        { "effort": "low" }, { "effort": "medium" },
                        { "effort": "high" }, { "effort": "xhigh" },
                        { "effort": "max" }, { "effort": "ultra" }
                    ]
                },
                {
                    "slug": "gpt-5.6-sol-wm",
                    "display_name": "GPT-5.6-Sol-WM",
                    "default_reasoning_level": "low",
                    "supported_reasoning_levels": [
                        { "effort": "low" }, { "effort": "medium" },
                        { "effort": "high" }, { "effort": "xhigh" },
                        { "effort": "max" }, { "effort": "ultra" }
                    ]
                },
                {
                    "slug": "gpt-5.6-terra",
                    "display_name": "GPT-5.6-Terra",
                    "default_reasoning_level": "medium",
                    "supported_reasoning_levels": [
                        { "effort": "low" }, { "effort": "medium" },
                        { "effort": "high" }, { "effort": "xhigh" },
                        { "effort": "max" }, { "effort": "ultra" }
                    ]
                },
                {
                    "slug": "gpt-5.6-luna",
                    "display_name": "GPT-5.6-Luna",
                    "default_reasoning_level": "medium",
                    "supported_reasoning_levels": [
                        { "effort": "low" }, { "effort": "medium" },
                        { "effort": "high" }, { "effort": "xhigh" },
                        { "effort": "max" }
                    ]
                },
                {
                    "slug": "gpt-5.5",
                    "display_name": "GPT-5.5",
                    "default_reasoning_level": "medium",
                    "supported_reasoning_levels": [
                        { "effort": "low" }, { "effort": "medium" },
                        { "effort": "high" }, { "effort": "xhigh" }
                    ]
                },
                {
                    "slug": "gpt-5.4",
                    "display_name": "GPT-5.4",
                    "default_reasoning_level": "medium",
                    "supported_reasoning_levels": [
                        { "effort": "low" }, { "effort": "medium" },
                        { "effort": "high" }, { "effort": "xhigh" }
                    ]
                },
                {
                    "slug": "gpt-5.4-mini",
                    "display_name": "GPT-5.4-Mini",
                    "default_reasoning_level": "medium",
                    "supported_reasoning_levels": [
                        { "effort": "low" }, { "effort": "medium" },
                        { "effort": "high" }, { "effort": "xhigh" }
                    ]
                },
                {
                    "slug": "gpt-5.3-codex-spark",
                    "display_name": "GPT-5.3-Codex-Spark",
                    "default_reasoning_level": "high",
                    "supported_reasoning_levels": [
                        { "effort": "low" }, { "effort": "medium" },
                        { "effort": "high" }, { "effort": "xhigh" }
                    ]
                },
                {
                    "slug": "codex-auto-review",
                    "display_name": "Codex Auto Review",
                    "default_reasoning_level": "medium",
                    "supported_reasoning_levels": [
                        { "effort": "low" }, { "effort": "medium" },
                        { "effort": "high" }, { "effort": "xhigh" },
                        { "effort": "max" }
                    ]
                }
            ]
        });

        let six = vec![
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::Xhigh,
            ReasoningEffort::Max,
            ReasoningEffort::Ultra,
        ];
        let five = vec![
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::Xhigh,
            ReasoningEffort::Max,
        ];
        let four = vec![
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::Xhigh,
        ];
        let expected = [
            ("gpt-5.6-sol", ReasoningEffort::Low, six.as_slice()),
            ("gpt-5.6-sol-wm", ReasoningEffort::Low, six.as_slice()),
            ("gpt-5.6-terra", ReasoningEffort::Medium, six.as_slice()),
            ("gpt-5.6-luna", ReasoningEffort::Medium, five.as_slice()),
            ("gpt-5.5", ReasoningEffort::Medium, four.as_slice()),
            ("gpt-5.4", ReasoningEffort::Medium, four.as_slice()),
            ("gpt-5.4-mini", ReasoningEffort::Medium, four.as_slice()),
            (
                "gpt-5.3-codex-spark",
                ReasoningEffort::High,
                four.as_slice(),
            ),
            (
                "codex-auto-review",
                ReasoningEffort::Medium,
                five.as_slice(),
            ),
        ];

        let presets = parse_openai_codex_catalog_models(&payload);
        assert_eq!(presets.len(), expected.len());
        assert_eq!(
            presets.keys().map(String::as_str).collect::<Vec<_>>(),
            expected
                .iter()
                .map(|(slug, _, _)| *slug)
                .collect::<Vec<_>>()
        );

        for (slug, default, efforts) in expected {
            let preset = presets
                .get(slug)
                .unwrap_or_else(|| panic!("missing {slug}"));
            assert_eq!(
                preset.supports_reasoning_effort,
                Some(true),
                "{slug} must advertise reasoning support"
            );
            assert_eq!(
                preset
                    .reasoning_efforts
                    .iter()
                    .map(|option| option.value)
                    .collect::<Vec<_>>(),
                efforts.to_vec(),
                "{slug} effort menu drifted"
            );
            assert_eq!(preset.reasoning_effort, Some(default), "{slug} default");
            let defaults = preset
                .reasoning_efforts
                .iter()
                .filter(|option| option.default)
                .collect::<Vec<_>>();
            assert_eq!(defaults.len(), 1, "{slug} must have one default");
            assert_eq!(defaults[0].value, default, "{slug} default marker");
        }
    }

    #[test]
    fn codex_catalog_preserves_ultra_as_an_authoritative_default() {
        let payload = serde_json::json!({
            "models": [{
                "slug": "gpt-5.6-sol",
                "default_reasoning_level": "ultra",
                "supported_reasoning_levels": [
                    { "effort": "max", "description": "Maximum reasoning" },
                    {
                        "effort": "ultra",
                        "description": "Maximum reasoning with proactive multi-agent guidance"
                    }
                ]
            }]
        });

        let presets = parse_openai_codex_catalog_models(&payload);
        let sol = presets.get("gpt-5.6-sol").expect("Sol catalog entry");
        assert_eq!(
            sol.reasoning_efforts
                .iter()
                .map(|option| option.value)
                .collect::<Vec<_>>(),
            vec![
                xai_grok_sampling_types::ReasoningEffort::Max,
                xai_grok_sampling_types::ReasoningEffort::Ultra,
            ]
        );
        assert_eq!(
            sol.reasoning_effort,
            Some(xai_grok_sampling_types::ReasoningEffort::Ultra)
        );
        assert!(!sol.reasoning_efforts[0].default);
        assert!(sol.reasoning_efforts[1].default);
        assert_eq!(
            sol.reasoning_efforts[1].description.as_deref(),
            Some("Maximum reasoning with proactive multi-agent guidance")
        );
    }

    #[test]
    fn codex_catalog_parser_preserves_mixed_ultra_menu() {
        let payload = serde_json::json!({
            "models": [{
                "slug": "gpt-5.6-sol",
                "default_reasoning_level": "high",
                "supported_reasoning_levels": [
                    { "effort": "low", "description": "Fast" },
                    { "effort": "high", "description": "Deep" },
                    { "effort": "max", "description": "Maximum reasoning" },
                    {
                        "effort": "ultra",
                        "description": "Maximum reasoning with automatic task delegation"
                    }
                ]
            }]
        });
        let presets = parse_openai_codex_catalog_models(&payload);
        let sol = presets.get("gpt-5.6-sol").expect("sol model");
        assert_eq!(
            sol.reasoning_efforts
                .iter()
                .map(|option| option.value)
                .collect::<Vec<_>>(),
            vec![
                xai_grok_sampling_types::ReasoningEffort::Low,
                xai_grok_sampling_types::ReasoningEffort::High,
                xai_grok_sampling_types::ReasoningEffort::Max,
                xai_grok_sampling_types::ReasoningEffort::Ultra,
            ]
        );
        let ultra = sol
            .reasoning_efforts
            .iter()
            .find(|option| option.value == xai_grok_sampling_types::ReasoningEffort::Ultra)
            .expect("ultra option");
        assert_eq!(ultra.id, "ultra");
        assert_eq!(ultra.label, "Ultra");
        assert_eq!(
            ultra.description.as_deref(),
            Some("Maximum reasoning with automatic task delegation")
        );
    }

    #[test]
    fn codex_catalog_parser_preserves_ultra_only_menu() {
        let payload = serde_json::json!({
            "models": [{
                "slug": "gpt-5.6-sol-wm",
                "default_reasoning_level": "ultra",
                "supported_reasoning_levels": [
                    {
                        "effort": "ultra",
                        "description": "Maximum reasoning with automatic task delegation"
                    }
                ]
            }]
        });
        let presets = parse_openai_codex_catalog_models(&payload);
        let model = presets.get("gpt-5.6-sol-wm").expect("watermark model");
        assert_eq!(model.reasoning_efforts.len(), 1);
        assert_eq!(
            model.reasoning_effort,
            Some(xai_grok_sampling_types::ReasoningEffort::Ultra)
        );
        assert!(model.reasoning_efforts[0].default);
        assert_eq!(
            model.reasoning_efforts[0].value,
            xai_grok_sampling_types::ReasoningEffort::Ultra
        );
        assert_eq!(model.supports_reasoning_effort, Some(true));
    }

    #[test]
    fn codex_catalog_default_reasoning_level_must_belong_to_supported_levels() {
        let payload = serde_json::json!({
            "models": [
                {
                    "slug": "gpt-codex-mismatch",
                    "default_reasoning_level": "high",
                    "supported_reasoning_levels": [
                        { "effort": "low", "description": "Fast" },
                        { "effort": "medium", "description": "Balanced" },
                        { "effort": "future", "description": "Unknown to this client" },
                        "not-an-object"
                    ]
                },
                {
                    "slug": "gpt-codex-no-efforts",
                    "default_reasoning_level": "high",
                    "supported_reasoning_levels": []
                }
            ]
        });
        let presets = parse_openai_codex_catalog_models(&payload);
        let mismatch = presets.get("gpt-codex-mismatch").expect("mismatch model");
        assert_eq!(mismatch.reasoning_efforts.len(), 2);
        assert_eq!(
            mismatch.reasoning_effort,
            Some(xai_grok_sampling_types::ReasoningEffort::Low),
            "an out-of-menu catalog default must not reach the wire"
        );
        assert!(mismatch.reasoning_efforts[0].default);
        assert_eq!(mismatch.supports_reasoning_effort, Some(true));

        let empty = presets.get("gpt-codex-no-efforts").expect("empty model");
        assert!(empty.reasoning_efforts.is_empty());
        assert_eq!(empty.reasoning_effort, None);
        assert_eq!(empty.supports_reasoning_effort, Some(false));
    }

    #[test]
    fn codex_catalog_metadata_override_can_disable_reasoning_effort() {
        let key = "gpt-codex-disabled";
        let mut models = IndexMap::from([(
            key.to_owned(),
            ConfigModelOverride {
                supports_reasoning_effort: Some(false),
                reasoning_effort: Some(xai_grok_sampling_types::ReasoningEffort::Xhigh),
                reasoning_efforts: vec![xai_grok_sampling_types::ReasoningEffortOption {
                    id: "xhigh".into(),
                    value: xai_grok_sampling_types::ReasoningEffort::Xhigh,
                    label: "X-High".into(),
                    description: None,
                    default: true,
                }],
                ..ConfigModelOverride::default()
            },
        )]);
        let presets = parse_openai_codex_catalog_models(&serde_json::json!({
            "models": [{
                "slug": key,
                "default_reasoning_level": "high",
                "supported_reasoning_levels": [
                    { "effort": "low" },
                    { "effort": "high" }
                ]
            }]
        }));
        merge_openai_codex_preset_entries(&mut models, CodexCatalogListing::served(presets));

        let merged = models.get(key).expect("merged metadata override");
        assert_eq!(merged.supports_reasoning_effort, Some(false));
        assert_eq!(merged.reasoning_effort, None);
        assert!(
            merged.reasoning_efforts.is_empty(),
            "an explicitly disabled override must not inherit the catalog menu"
        );

        let cfg = Config::default();
        let resolved = resolve_model_list(
            &cfg,
            Some(IndexMap::from([(
                key.to_owned(),
                merged.apply(key, None, &cfg.endpoints),
            )])),
        );
        let info = &resolved[key].info;
        assert!(!info.supports_reasoning_effort);
        assert!(info.reasoning_efforts.is_empty());
        assert_eq!(info.reasoning_effort, None);
        let sampler = crate::agent::config::sampling_config_for_model(
            &resolved[key],
            crate::agent::config::resolve_credentials(&resolved[key], None),
            None,
            None,
            None,
            None,
            &crate::agent::trusted_origins::TrustedXaiOrigins::default(),
        );
        assert_eq!(sampler.reasoning_effort, None);
    }

    #[test]
    fn codex_catalog_explicit_empty_reasoning_menu_disables_unclaimed_scalar_end_to_end() {
        use xai_grok_sampling_types::ReasoningEffort;

        let evaluate = |key: &str, user: ConfigModelOverride, catalog: serde_json::Value| {
            let mut models = IndexMap::from([(key.to_owned(), user)]);
            merge_openai_codex_preset_entries(
                &mut models,
                CodexCatalogListing::served(parse_openai_codex_catalog_models(&catalog)),
            );
            let merged = models.get(key).expect("merged metadata override");
            let cfg = Config::default();
            let resolved = resolve_model_list(
                &cfg,
                Some(IndexMap::from([(
                    key.to_owned(),
                    merged.apply(key, None, &cfg.endpoints),
                )])),
            );
            let info = &resolved[key].info;
            let sampler = crate::agent::config::sampling_config_for_model(
                &resolved[key],
                crate::agent::config::resolve_credentials(&resolved[key], None),
                None,
                None,
                None,
                None,
                &crate::agent::trusted_origins::TrustedXaiOrigins::default(),
            );
            (
                merged.supports_reasoning_effort,
                merged.reasoning_effort,
                merged.reasoning_efforts.len(),
                info.supports_reasoning_effort,
                info.reasoning_effort,
                sampler.reasoning_effort,
            )
        };

        let disabled = evaluate(
            "gpt-codex-empty-menu",
            ConfigModelOverride {
                reasoning_effort: Some(ReasoningEffort::High),
                ..ConfigModelOverride::default()
            },
            serde_json::json!({
                "models": [{
                    "slug": "gpt-codex-empty-menu",
                    "default_reasoning_level": "high",
                    "supported_reasoning_levels": []
                }]
            }),
        );
        assert_eq!(disabled, (Some(false), None, 0, false, None, None));

        let absent = evaluate(
            "gpt-codex-legacy-absent-menu",
            ConfigModelOverride {
                reasoning_effort: Some(ReasoningEffort::Xhigh),
                ..ConfigModelOverride::default()
            },
            serde_json::json!({
                "models": [{ "slug": "gpt-codex-legacy-absent-menu" }]
            }),
        );
        assert_eq!(
            absent,
            (
                None,
                Some(ReasoningEffort::Xhigh),
                0,
                false,
                Some(ReasoningEffort::Xhigh),
                Some(ReasoningEffort::Xhigh),
            ),
            "an absent catalog menu must retain the legacy scalar semantics"
        );

        let user_enabled = evaluate(
            "gpt-codex-user-enabled-empty-menu",
            ConfigModelOverride {
                reasoning_effort: Some(ReasoningEffort::High),
                supports_reasoning_effort: Some(true),
                ..ConfigModelOverride::default()
            },
            serde_json::json!({
                "models": [{
                    "slug": "gpt-codex-user-enabled-empty-menu",
                    "supported_reasoning_levels": []
                }]
            }),
        );
        assert_eq!(
            user_enabled,
            (
                Some(true),
                Some(ReasoningEffort::High),
                0,
                true,
                Some(ReasoningEffort::High),
                Some(ReasoningEffort::High),
            ),
            "an explicit user support opt-in must retain its legacy scalar"
        );
    }

    #[test]
    fn codex_catalog_metadata_override_rejects_default_outside_inherited_menu() {
        let key = "gpt-codex-limited";
        let mut models = IndexMap::from([(
            key.to_owned(),
            ConfigModelOverride {
                reasoning_effort: Some(xai_grok_sampling_types::ReasoningEffort::Xhigh),
                ..ConfigModelOverride::default()
            },
        )]);
        let presets = parse_openai_codex_catalog_models(&serde_json::json!({
            "models": [{
                "slug": key,
                "default_reasoning_level": "high",
                "supported_reasoning_levels": [
                    { "effort": "low" },
                    { "effort": "high" }
                ]
            }]
        }));
        merge_openai_codex_preset_entries(&mut models, CodexCatalogListing::served(presets));

        let merged = models.get(key).expect("merged metadata override");
        assert_eq!(
            merged.reasoning_effort,
            Some(xai_grok_sampling_types::ReasoningEffort::High),
            "a scalar outside the inherited menu must fall back to the catalog default"
        );
        assert!(
            merged
                .reasoning_efforts
                .iter()
                .all(|option| option.value != xai_grok_sampling_types::ReasoningEffort::Xhigh)
        );
    }

    #[test]
    fn codex_catalog_metadata_override_keeps_default_without_inherited_menu() {
        let key = "gpt-codex-legacy";
        let mut models = IndexMap::from([(
            key.to_owned(),
            ConfigModelOverride {
                reasoning_effort: Some(xai_grok_sampling_types::ReasoningEffort::Xhigh),
                ..ConfigModelOverride::default()
            },
        )]);
        let presets = IndexMap::from([(
            key.to_owned(),
            ConfigModelOverride {
                model: Some(key.to_owned()),
                supports_reasoning_effort: Some(true),
                reasoning_effort: None,
                reasoning_efforts: Vec::new(),
                ..ConfigModelOverride::default()
            },
        )]);

        merge_openai_codex_preset_entries(&mut models, CodexCatalogListing::served(presets));

        let merged = models.get(key).expect("merged metadata override");
        assert_eq!(
            merged.reasoning_effort,
            Some(xai_grok_sampling_types::ReasoningEffort::Xhigh),
            "without a restrictive catalog menu there is no evidence that the explicit tier is invalid"
        );
        assert!(merged.reasoning_efforts.is_empty());

        let cfg = Config::default();
        let resolved = resolve_model_list(
            &cfg,
            Some(IndexMap::from([(
                key.to_owned(),
                merged.apply(key, None, &cfg.endpoints),
            )])),
        );
        let sampler = crate::agent::config::sampling_config_for_model(
            &resolved[key],
            crate::agent::config::resolve_credentials(&resolved[key], None),
            None,
            None,
            None,
            None,
            &crate::agent::trusted_origins::TrustedXaiOrigins::default(),
        );
        assert_eq!(
            sampler.reasoning_effort,
            Some(xai_grok_sampling_types::ReasoningEffort::Xhigh)
        );
    }

    #[test]
    fn codex_catalog_metadata_narrower_menu_reconciles_scalar_end_to_end() {
        use xai_grok_sampling_types::{ReasoningEffort, ReasoningEffortOption};

        for (suffix, reasoning_efforts, expected) in [
            (
                "first",
                vec![ReasoningEffortOption {
                    id: "low".into(),
                    value: ReasoningEffort::Low,
                    label: "Low".into(),
                    description: None,
                    default: false,
                }],
                ReasoningEffort::Low,
            ),
            (
                "marked",
                vec![
                    ReasoningEffortOption {
                        id: "low".into(),
                        value: ReasoningEffort::Low,
                        label: "Low".into(),
                        description: None,
                        default: false,
                    },
                    ReasoningEffortOption {
                        id: "medium".into(),
                        value: ReasoningEffort::Medium,
                        label: "Medium".into(),
                        description: None,
                        default: true,
                    },
                ],
                ReasoningEffort::Medium,
            ),
        ] {
            let key = format!("gpt-codex-narrow-{suffix}");
            let mut models = IndexMap::from([(
                key.clone(),
                ConfigModelOverride {
                    reasoning_efforts,
                    ..ConfigModelOverride::default()
                },
            )]);
            let presets = parse_openai_codex_catalog_models(&serde_json::json!({
                "models": [{
                    "slug": key,
                    "default_reasoning_level": "high",
                    "supported_reasoning_levels": [
                        { "effort": "low" },
                        { "effort": "medium" },
                        { "effort": "high" }
                    ]
                }]
            }));
            merge_openai_codex_preset_entries(&mut models, CodexCatalogListing::served(presets));

            let merged = models.get(&key).expect("merged metadata override");
            assert_eq!(merged.reasoning_effort, Some(expected));
            assert_eq!(
                merged
                    .reasoning_efforts
                    .iter()
                    .filter(|option| option.default)
                    .map(|option| option.value)
                    .collect::<Vec<_>>(),
                vec![expected]
            );

            let cfg = Config::default();
            let resolved = resolve_model_list(
                &cfg,
                Some(IndexMap::from([(
                    key.clone(),
                    merged.apply(&key, None, &cfg.endpoints),
                )])),
            );
            let info = &resolved[&key].info;
            assert_eq!(info.reasoning_effort, Some(expected));
            assert_eq!(info.reasoning_efforts.len(), merged.reasoning_efforts.len());
            let sampler = crate::agent::config::sampling_config_for_model(
                &resolved[&key],
                crate::agent::config::resolve_credentials(&resolved[&key], None),
                None,
                None,
                None,
                None,
                &crate::agent::trusted_origins::TrustedXaiOrigins::default(),
            );
            assert_eq!(sampler.reasoning_effort, Some(expected));
        }
    }

    /// `data` / `id` / `name` are tolerances for shapes this endpoint does not
    /// currently return. They are kept so a server-side change does not break
    /// the fetch, and pinned so nobody mistakes them for the observed shape.
    ///
    /// Mini reports both fields; the budget is `max_context_window` (operative
    /// capacity), not `context_window` (pricing threshold). Small reports only
    /// the pricing field, which remains the fallback.
    #[test]
    fn codex_catalog_parser_tolerates_other_shapes_and_budgets_the_operative_window() {
        let payload = serde_json::json!({
            "data": [
                {
                    "id": "codex-mini",
                    "name": "Codex Mini",
                    "context_window": 64000,
                    "max_context_window": 256000
                },
                {
                    "model": "codex-small",
                    "context_window": 128000
                }
            ]
        });
        let presets = parse_openai_codex_catalog_models(&payload);
        let mini = presets.get("codex-mini").expect("mini preset should parse");
        assert_eq!(mini.model.as_deref(), Some("codex-mini"));
        assert_eq!(
            mini.model_provider.as_deref(),
            Some(OPENAI_CODEX_PROVIDER_ID)
        );
        // Operative capacity, not the 64k pricing threshold. `gpt-5.4` is the
        // live split: 272000 pricing vs 1000000 capacity.
        assert_eq!(mini.context_window, Some(256_000));
        let small = presets
            .get("codex-small")
            .expect("fallback slug preset should parse");
        assert_eq!(small.context_window, Some(128_000));
    }

    /// The live `gpt-5.4` shape (#266).
    ///
    /// Eight of nine models report `context_window` and `max_context_window`
    /// equal, so preferring either one looked identical. `gpt-5.4` does not:
    /// 272_000 is the billing/pricing threshold, 1_000_000 is operative token
    /// capacity. Budgeting the pricing field (#258) fires auto-compact at ~27%
    /// of the tokens the model accepts.
    #[test]
    fn codex_catalog_context_window_budgets_the_operative_window() {
        let obj = serde_json::json!({
            "context_window": 272_000,
            "max_context_window": 1_000_000
        });
        assert_eq!(
            codex_catalog_context_window(obj.as_object().expect("object")),
            Some(1_000_000),
            "gpt-5.4 is budgeted at max_context_window (operative capacity), not context_window (pricing threshold)"
        );

        let payload = serde_json::json!({
            "models": [{
                "slug": "gpt-5.4",
                "display_name": "GPT-5.4",
                "context_window": 272_000,
                "max_context_window": 1_000_000
            }]
        });
        let presets = parse_openai_codex_catalog_models(&payload);
        let entry = presets.get("gpt-5.4").expect("slug is the catalog key");
        assert_eq!(
            entry.context_window,
            Some(1_000_000),
            "the session budget is the operative capacity"
        );
    }

    /// Cheap inversion lock: this array *is* the lookup order. Putting
    /// `context_window` first is how #258 budgeted gpt-5.4 at the 272k
    /// pricing threshold.
    #[test]
    fn codex_catalog_context_window_keys_prefer_operative_capacity() {
        assert_eq!(
            CODEX_CATALOG_CONTEXT_WINDOW_KEYS,
            &[
                "max_context_window",
                "maxContextWindow",
                "context_window",
                "contextWindow",
            ]
        );
        let max_idx = CODEX_CATALOG_CONTEXT_WINDOW_KEYS
            .iter()
            .position(|key| *key == "max_context_window")
            .expect("max_context_window must be a candidate");
        let pricing_idx = CODEX_CATALOG_CONTEXT_WINDOW_KEYS
            .iter()
            .position(|key| *key == "context_window")
            .expect("context_window must remain as the pricing-threshold fallback");
        assert!(
            max_idx < pricing_idx,
            "inverting this order budgets gpt-5.4 at the 272k pricing threshold"
        );
    }

    /// The query parameter is not decoration: without it the endpoint answers
    /// HTTP 400 and the catalog silently stays at one hardcoded model.
    #[test]
    fn codex_catalog_url_carries_the_required_client_version() {
        let url = format!(
            "{}/models?client_version={}",
            crate::auth::openai_codex::CODEX_API_BASE_URL,
            OPENAI_CODEX_CATALOG_CLIENT_VERSION
        );
        assert!(
            url.contains("?client_version="),
            "GET /models without client_version is rejected: {url}"
        );
        assert!(!OPENAI_CODEX_CATALOG_CLIENT_VERSION.is_empty());
    }

    #[test]
    fn codex_catalog_fallback_uses_builtin_preset_when_fetch_is_empty() {
        let presets =
            effective_openai_codex_presets(Some(CodexCatalogListing::served(IndexMap::new())))
                .models;
        assert_eq!(presets.len(), 1);
        let preset = presets
            .get(OPENAI_CODEX_PRESET_MODEL_ID)
            .expect("built-in fallback preset should exist");
        assert_eq!(preset.model.as_deref(), Some(OPENAI_CODEX_PRESET_MODEL_ID));
        assert_eq!(
            preset.context_window,
            Some(OPENAI_CODEX_PRESET_CONTEXT_WINDOW)
        );
        assert_eq!(
            OPENAI_CODEX_PRESET_CONTEXT_WINDOW, 272_000,
            "#122: builtin fallback must match Sol's catalog window, not the 200k guess"
        );
    }

    fn catalog_test_identity(account_id: &str) -> CodexCatalogCacheIdentity {
        CodexCatalogCacheIdentity {
            account_id: Some(account_id.to_owned()),
            chatgpt_account_is_fedramp: false,
        }
    }

    fn catalog_test_payload(model: &str) -> serde_json::Value {
        serde_json::json!({
            "models": [{
                "slug": model,
                "display_name": model,
                "context_window": 123_456
            }]
        })
    }

    /// #262: the cache identity includes the verified account id. A cache hit
    /// for account A must remain a miss for account B even though both use the
    /// same endpoint and auth provider.
    #[test]
    fn codex_catalog_last_good_cache_is_account_keyed() {
        let home = tempfile::tempdir().expect("temporary catalog cache home");
        let account_a = catalog_test_identity("account-a");
        let account_b = catalog_test_identity("account-b");
        let path_a = codex_catalog_cache_path(home.path(), &account_a).expect("account A path");
        let path_b = codex_catalog_cache_path(home.path(), &account_b).expect("account B path");

        assert_ne!(path_a, path_b);
        assert!(!path_a.to_string_lossy().contains("account-a"));
        persist_codex_catalog_cache(&path_a, &catalog_test_payload("codex-a"));
        assert!(load_codex_catalog_cache(&path_a).is_some());
        assert!(
            load_codex_catalog_cache(&path_b).is_none(),
            "account B must not read account A's entitlements"
        );

        persist_codex_catalog_cache(&path_a, &catalog_test_payload("codex-a-new"));
        let refreshed_a = load_codex_catalog_cache(&path_a).expect("refreshed account A cache");
        assert!(refreshed_a.contains_key("codex-a-new"));
        assert!(
            !refreshed_a.contains_key("codex-a"),
            "a later successful refresh must replace the account's last-good snapshot"
        );

        persist_codex_catalog_cache(&path_b, &catalog_test_payload("codex-b"));
        assert!(
            load_codex_catalog_cache(&path_a)
                .unwrap()
                .contains_key("codex-a-new")
        );
        assert!(
            load_codex_catalog_cache(&path_b)
                .unwrap()
                .contains_key("codex-b")
        );
    }

    /// A crash may leave the deterministic PID/sequence temp path behind.
    /// The next refresh must clean that stale file and still persist bytes.
    #[test]
    fn codex_catalog_cache_temp_write_retries_stale_path() {
        let home = tempfile::tempdir().expect("temporary catalog cache home");
        let tmp = home.path().join("catalog.json.tmp.reused");
        std::fs::write(&tmp, b"stale partial catalog").expect("seed stale temp file");

        write_codex_catalog_cache_tmp(&tmp, b"fresh complete catalog")
            .expect("replace stale temp file");
        assert_eq!(
            std::fs::read(&tmp).expect("read refreshed temp file"),
            b"fresh complete catalog"
        );
    }

    /// #262: an expired bearer is not eligible for the live request, but its
    /// retained verified account id must still select that account's cache.
    #[test]
    fn codex_catalog_expired_credential_can_read_account_cache() {
        let home = tempfile::tempdir().expect("temporary catalog cache home");
        let manager = crate::auth::AuthManager::new_openai_codex(home.path());
        manager.hot_swap(crate::auth::GrokAuth {
            key: "expired-catalog-token".to_owned(),
            auth_mode: crate::auth::AuthMode::Oidc,
            expires_at: Some(chrono::Utc::now() - chrono::Duration::hours(1)),
            refresh_token: Some("retained-refresh-token".to_owned()),
            account_id: Some("account-expired".to_owned()),
            ..crate::auth::GrokAuth::test_default()
        });

        let (identity, live) =
            codex_catalog_access_from_manager(&manager).expect("retained account identity");
        assert!(
            live.is_none(),
            "expired bearer must not reach the live request"
        );
        let path = codex_catalog_cache_path(home.path(), &identity).expect("account cache path");
        persist_codex_catalog_cache(&path, &catalog_test_payload("codex-last-good"));

        let fallback = codex_catalog_fallback_models(Some(&path));
        assert!(fallback.contains_key("codex-last-good"));
        assert_eq!(
            fallback["codex-last-good"]
                .catalog_degraded_reason
                .as_deref(),
            Some(OPENAI_CODEX_SAVED_CATALOG_REASON)
        );
    }

    /// Disabling network discovery suppresses only the live request; it must
    /// not discard the account's already persisted model menu.
    #[test]
    fn codex_catalog_remote_fetch_disabled_uses_account_cache() {
        let home = tempfile::tempdir().expect("temporary catalog cache home");
        let identity = catalog_test_identity("account-offline");
        let path = codex_catalog_cache_path(home.path(), &identity).expect("account cache path");
        persist_codex_catalog_cache(&path, &catalog_test_payload("codex-offline"));

        let listing = load_codex_catalog_when_remote_fetch_disabled(Some(&path))
            .expect("saved offline catalog");
        assert!(
            listing.enumerates_account_slugs,
            "a catalog the server did serve, kept because remote fetch is off, still enumerates the account's slugs"
        );
        let models = listing.models;
        assert!(models.contains_key("codex-offline"));
        assert!(
            models["codex-offline"].catalog_degraded_reason.is_none(),
            "an intentional network policy is not a failed live refresh"
        );
    }

    /// #262: a failed live refresh keeps the account's last-good menu and
    /// carries a structured reason through ACP instead of silently looking
    /// identical to the old single-model preset.
    #[test]
    fn codex_catalog_saved_fallback_is_visible_in_acp_metadata() {
        let home = tempfile::tempdir().expect("temporary catalog cache home");
        let credential = catalog_test_identity("account-a");
        let path = codex_catalog_cache_path(home.path(), &credential).expect("cache path");
        persist_codex_catalog_cache(&path, &catalog_test_payload("codex-saved"));
        let fallback = codex_catalog_fallback_models(Some(&path));
        assert!(fallback.contains_key("codex-saved"));

        let mut cfg = Config::new_from_toml_cfg(&toml::Value::Table(toml::map::Map::new()))
            .expect("empty config");
        cfg.config_models = fallback;
        let resolved = resolve_model_list(&cfg, None);
        let meta = crate::agent::config::to_acp_model_info(&resolved)
            .get(&acp::ModelId::new("codex-saved"))
            .and_then(|model| model.meta.as_ref())
            .cloned()
            .expect("saved model ACP metadata");
        assert_eq!(
            meta.get("catalogDegradedReason")
                .and_then(serde_json::Value::as_str),
            Some(OPENAI_CODEX_SAVED_CATALOG_REASON)
        );
    }

    /// A user's display-text override must not erase operational fallback
    /// state while the built-in preset supplies the routing underneath it.
    #[test]
    fn codex_catalog_degraded_state_survives_description_override() {
        let presets = mark_codex_catalog_degraded(
            openai_codex_preset_models(),
            OPENAI_CODEX_SAVED_CATALOG_REASON,
        );
        let mut models = IndexMap::from([(
            OPENAI_CODEX_PRESET_MODEL_ID.to_owned(),
            ConfigModelOverride {
                description: Some("My Codex model".to_owned()),
                ..ConfigModelOverride::default()
            },
        )]);
        merge_openai_codex_preset_entries(&mut models, CodexCatalogListing::stand_in(presets));
        assert_eq!(
            models[OPENAI_CODEX_PRESET_MODEL_ID]
                .catalog_degraded_reason
                .as_deref(),
            Some(OPENAI_CODEX_SAVED_CATALOG_REASON)
        );

        let mut cfg = Config::new_from_toml_cfg(&toml::Value::Table(toml::map::Map::new()))
            .expect("empty config");
        cfg.config_models = models;
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved
            .get(OPENAI_CODEX_PRESET_MODEL_ID)
            .expect("merged Codex model");
        assert_eq!(
            model.info.catalog_degraded_reason.as_deref(),
            Some(OPENAI_CODEX_SAVED_CATALOG_REASON)
        );
        assert!(
            model
                .info
                .description
                .as_deref()
                .is_some_and(|description| description.starts_with("My Codex model — "))
        );
        let acp_models = crate::agent::config::to_acp_model_info(&resolved);
        let meta = acp_models
            .get(&acp::ModelId::new(OPENAI_CODEX_PRESET_MODEL_ID))
            .and_then(|model| model.meta.as_ref())
            .expect("Codex ACP metadata");
        assert_eq!(
            meta.get("catalogDegradedReason")
                .and_then(serde_json::Value::as_str),
            Some(OPENAI_CODEX_SAVED_CATALOG_REASON)
        );

        let healthy = ConfigModelOverride {
            description: Some(format!(
                "User-authored text containing {}but no runtime failure",
                OPENAI_CODEX_CATALOG_DEGRADED_MARKER
            )),
            ..ConfigModelOverride::default()
        }
        .apply(
            "healthy-custom",
            None,
            &crate::agent::config::EndpointsConfig::default(),
        );
        let healthy_models = IndexMap::from([("healthy-custom".to_owned(), healthy)]);
        let healthy_acp = crate::agent::config::to_acp_model_info(&healthy_models);
        assert!(
            healthy_acp[&acp::ModelId::new("healthy-custom")]
                .meta
                .as_ref()
                .is_none_or(|meta| !meta.contains_key("catalogDegradedReason")),
            "display text alone must not synthesize operational degraded state"
        );
    }

    /// #262's persisted-default criterion is already enforced by #131's
    /// substitution wire field. Pin the interaction with the built-in fallback
    /// so a missing saved/default model cannot become a silent model change.
    #[test]
    fn codex_catalog_builtin_fallback_reports_absent_persisted_default() {
        let fallback = codex_catalog_fallback_models(None);
        let mut cfg = Config::new_from_toml_cfg(&toml::Value::Table(toml::map::Map::new()))
            .expect("empty config");
        cfg.config_models = fallback;
        cfg.models.default = Some("gpt-5.4".to_owned());
        let resolved = resolve_model_list(&cfg, None);
        let (_key, _entry, source, _reason) =
            crate::agent::models::resolve_default_model(&cfg, &resolved, true);
        let substitution = crate::agent::models::substituted_preference(&cfg, source)
            .expect("absent persisted default must be reported");
        assert_eq!(
            substitution.to_meta_value(),
            serde_json::json!({
                "configuredModelId": "gpt-5.4",
                "source": "config"
            })
        );
    }

    #[test]
    fn codex_catalog_credential_source_must_be_openai_codex_provider() {
        assert!(codex_catalog_credential_source_is_valid(
            &xai_grok_sampler::CredentialSource::AuthProvider {
                name: OPENAI_CODEX_PROVIDER_ID.to_owned()
            }
        ));
        assert!(!codex_catalog_credential_source_is_valid(
            &xai_grok_sampler::CredentialSource::AuthProvider {
                name: "other-provider".to_owned()
            }
        ));
        assert!(!codex_catalog_credential_source_is_valid(
            &xai_grok_sampler::CredentialSource::Missing
        ));
    }
}
