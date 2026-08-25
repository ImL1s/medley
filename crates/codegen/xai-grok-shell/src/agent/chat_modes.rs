//! grok.com chat-product model catalog: caches `/rest/modes` and maps modes to
//! the `SessionModelState` returned by `load_chat_session` (the chat analogue of
//! [`crate::agent::models::ModelsManager`]). NB: these "modes" populate the
//! desktop MODEL picker, not the ACP session plan-modes in `LoadSessionResponse.modes`.
use crate::auth::AuthManager;
use crate::remote::chat_models_client::{
    ChatModelsClient, ChatModelsError, ListModesResponse, Mode,
};
use agent_client_protocol as acp;
use parking_lot::RwLock;
use std::sync::Arc;
use std::time::{Duration, Instant};
/// ~54 min, matching grok-web's refetch cadence.
const CACHE_TTL: Duration = Duration::from_secs(54 * 60);
/// Cold-miss budget on the `session/load` critical path (warm/stale served instantly).
const COLD_FETCH_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_LOCALE: &str = "en";
/// Process-wide flag set by the pager when started with `--chat` so initialize
/// and early UI seed the chat `/rest/modes` catalog instead of build models.
pub const GROK_CHAT_MODE_ENV: &str = "GROK_CHAT_MODE";
/// True when the process is a gateway light-frontend (`--chat`) agent.
/// Hard-off in release builds so it can't be enabled via env.
pub fn process_chat_mode_enabled() -> bool {
    false
}
/// Whether a [`ChatModesManager::model_state_with_authority`] snapshot
/// represents a real `/rest/modes` answer or the absence of one (#483
/// review finding).
///
/// A raw [`acp::SessionModelState`] with `available_models.is_empty()` is
/// ambiguous on its own: it could mean the server authoritatively reported
/// zero currently-available modes (e.g. every mode requires an upgrade —
/// [`modes_to_model_state`]'s filter genuinely produces this from a
/// well-formed response, it is not hypothetical), or it could mean no real
/// answer was ever obtained at all (unauthenticated, a fetch failure with
/// nothing cached, auth changing mid-fetch, or a degenerate all-empty
/// response). Those two facts warrant different handling — the first is
/// "this id is not available", the second is "we cannot evaluate this id"
/// — so this type keeps them apart instead of collapsing both into the
/// same empty [`acp::SessionModelState`] the way returning one bare value
/// would.
#[derive(Debug, Clone)]
pub(crate) enum ChatModelCatalog {
    /// Built from a real `/rest/modes` response — fresh, or served from a
    /// cache that itself came from a real response (including a stale one
    /// while a background refresh is in flight). `available_models` may
    /// still be empty; that is a real, authoritative answer in that case,
    /// not missing information.
    Authoritative(acp::SessionModelState),
    /// No real `/rest/modes` response was ever obtained. `available_models`
    /// is always empty here and the emptiness carries no information about
    /// the user's actual chat modes.
    NoInfo(acp::SessionModelState),
}
impl ChatModelCatalog {
    /// Discard the authoritative/no-info distinction, keeping only the
    /// state — for callers that only display it (see
    /// [`ChatModesManager::model_state`]'s doc for why they don't need the
    /// distinction).
    pub(crate) fn into_state(self) -> acp::SessionModelState {
        match self {
            Self::Authoritative(state) | Self::NoInfo(state) => state,
        }
    }
}
#[derive(Clone)]
struct CachedModes {
    /// Keyed by identity; a mismatch is a miss so one user's modes never leak to another.
    user_id: String,
    locale: String,
    fetched_at: Instant,
    response: ListModesResponse,
}
/// Thread-safe, cheaply-cloneable manager. Cloning bumps the inner `Arc`.
#[derive(Clone)]
pub(crate) struct ChatModesManager {
    inner: Arc<Inner>,
}
struct Inner {
    auth: Arc<AuthManager>,
    cache: RwLock<Option<CachedModes>>,
    /// Single-flight guard so concurrent fetches coalesce.
    fetch_lock: tokio::sync::Mutex<()>,
}
impl ChatModesManager {
    pub(crate) fn new(auth: Arc<AuthManager>) -> Self {
        Self {
            inner: Arc::new(Inner {
                auth,
                cache: RwLock::new(None),
                fetch_lock: tokio::sync::Mutex::new(()),
            }),
        }
    }
    /// The active grok.com identity, or `None` when unauthenticated. Modes are
    /// per-identity (tier/ACL), so every cache key and store is gated on it.
    fn current_user_id(&self) -> Option<String> {
        self.inner.auth.current_or_expired().map(|a| a.user_id)
    }
    /// Chat model state for a `session/load` response. On missing auth or fetch
    /// failure, serves last-good cache else empty — never the build catalog.
    ///
    /// Discards the authoritative/no-info distinction [`Self::model_state_with_authority`]
    /// carries — existing callers of this method only display the result, so
    /// they don't need it. A caller that must tell "the server said zero
    /// modes are available" apart from "we have no idea what's available"
    /// (#483 review finding) needs that method instead.
    pub(crate) async fn model_state(&self) -> acp::SessionModelState {
        self.model_state_with_authority().await.into_state()
    }
    /// Same as [`Self::model_state`], but keeps the fact of *how* the
    /// returned state was produced instead of discarding it.
    ///
    /// A response with zero `available_models` is ambiguous by itself: it
    /// could mean "the server authoritatively says every mode currently
    /// requires an upgrade" (a real answer a caller may need to act on) or
    /// "we have no server answer at all" (unauthenticated, a fetch failure
    /// with nothing cached, or a malformed empty response) — a caller with
    /// nothing to validate against. Every return point below is tagged with
    /// which one it is; see [`ChatModelCatalog`].
    pub(crate) async fn model_state_with_authority(&self) -> ChatModelCatalog {
        let Some(user_id) = self.current_user_id() else {
            return ChatModelCatalog::NoInfo(empty_state());
        };
        let locale = DEFAULT_LOCALE;
        {
            let guard = self.inner.cache.read();
            if let Some(c) = guard.as_ref()
                && c.user_id == user_id
                && c.locale == locale
            {
                if c.fetched_at.elapsed() < CACHE_TTL {
                    return ChatModelCatalog::Authoritative(modes_to_model_state(&c.response));
                }
                let stale = c.response.clone();
                drop(guard);
                self.spawn_refresh(user_id, locale);
                return ChatModelCatalog::Authoritative(modes_to_model_state(&stale));
            }
        }
        let _flight = self.inner.fetch_lock.lock().await;
        {
            let guard = self.inner.cache.read();
            if let Some(c) = guard.as_ref()
                && c.user_id == user_id
                && c.locale == locale
                && c.fetched_at.elapsed() < CACHE_TTL
            {
                return ChatModelCatalog::Authoritative(modes_to_model_state(&c.response));
            }
        }
        match self.fetch(locale).await {
            Ok(resp) if !resp.modes.is_empty() => {
                if self.current_user_id().as_deref() != Some(user_id.as_str()) {
                    return ChatModelCatalog::NoInfo(empty_state());
                }
                let mapped = modes_to_model_state(&resp);
                if mapped.available_models.is_empty() {
                    tracing::warn!(
                        raw_modes = resp.modes.len(),
                        "chat modes: fetch returned modes but none selectable after availability filter"
                    );
                }
                self.store(user_id, locale.to_owned(), resp);
                ChatModelCatalog::Authoritative(mapped)
            }
            // The server itself returned zero modes (not merely zero
            // *available* ones) -- a degenerate response shape, not the
            // well-formed "every mode requires upgrade" answer
            // `Authoritative` with an empty `available_models` represents.
            // Treated as no information rather than an authoritative
            // rejection.
            Ok(_) => ChatModelCatalog::NoInfo(empty_state()),
            Err(err) => {
                tracing::warn!(error = %err, "chat modes fetch failed; serving cache/empty");
                let guard = self.inner.cache.read();
                match guard.as_ref() {
                    Some(c) if c.user_id == user_id => {
                        ChatModelCatalog::Authoritative(modes_to_model_state(&c.response))
                    }
                    _ => ChatModelCatalog::NoInfo(empty_state()),
                }
            }
        }
    }
    async fn fetch(&self, locale: &str) -> Result<ListModesResponse, ChatModelsError> {
        let client = ChatModelsClient::new(self.inner.auth.clone());
        match tokio::time::timeout(COLD_FETCH_TIMEOUT, client.list_modes(locale)).await {
            Ok(result) => result,
            Err(_elapsed) => Err(ChatModelsError::Timeout),
        }
    }
    fn store(&self, user_id: String, locale: String, response: ListModesResponse) {
        *self.inner.cache.write() = Some(CachedModes {
            user_id,
            locale,
            fetched_at: Instant::now(),
            response,
        });
    }
    /// Best-effort stale refresh; skips if a fetch is already in flight.
    fn spawn_refresh(&self, user_id: String, locale: &'static str) {
        let me = self.clone();
        tokio::spawn(async move {
            let Ok(_flight) = me.inner.fetch_lock.try_lock() else {
                return;
            };
            if me.current_user_id().as_deref() != Some(user_id.as_str()) {
                return;
            }
            if let Ok(resp) = me.fetch(locale).await
                && !resp.modes.is_empty()
                && me.current_user_id().as_deref() == Some(user_id.as_str())
            {
                me.store(user_id, locale.to_owned(), resp);
            }
        });
    }
    /// Kick a background `/rest/modes` fill when auth is already present so
    /// `--chat` initialize / first `session/new` hit a warm cache.
    pub(crate) fn warm_in_background(&self) {
        let Some(user_id) = self.current_user_id() else {
            return;
        };
        self.spawn_refresh(user_id, DEFAULT_LOCALE);
    }
}
fn empty_state() -> acp::SessionModelState {
    acp::SessionModelState::new(acp::ModelId::from(String::new()), Vec::new())
}
/// Maps grok.com modes → `SessionModelState`: keeps only `available` modes,
/// reconciles `current_model_id` (default → first available → empty, never
/// out-of-set), and stashes `badgeText`/`iconHint`/`tags` in `_meta`.
pub(crate) fn modes_to_model_state(resp: &ListModesResponse) -> acp::SessionModelState {
    let available_models: Vec<acp::ModelInfo> = resp
        .modes
        .iter()
        .filter(|m| m.is_available())
        .map(mode_to_model_info)
        .collect();
    let current_model_id = reconcile_current(&resp.default_mode_id, &available_models);
    acp::SessionModelState::new(current_model_id, available_models)
}
fn mode_to_model_info(m: &Mode) -> acp::ModelInfo {
    let name = if m.title.trim().is_empty() {
        m.id.clone()
    } else {
        m.title.clone()
    };
    acp::ModelInfo::new(acp::ModelId::from(m.id.clone()), name)
        .description(if m.description.is_empty() {
            None
        } else {
            Some(m.description.clone())
        })
        .meta(build_meta(m))
}
fn build_meta(m: &Mode) -> Option<acp::Meta> {
    let mut map = serde_json::Map::new();
    if let Some(badge) = m.badge_text.as_deref().filter(|s| !s.is_empty()) {
        map.insert("badgeText".to_owned(), serde_json::json!(badge));
    }
    if !m.icon_hint.is_empty() {
        map.insert("iconHint".to_owned(), serde_json::json!(m.icon_hint));
    }
    if !m.tags.is_empty() {
        map.insert("tags".to_owned(), serde_json::json!(m.tags));
    }
    if map.is_empty() { None } else { Some(map) }
}
fn reconcile_current(default_mode_id: &str, available: &[acp::ModelInfo]) -> acp::ModelId {
    let in_set = |id: &str| available.iter().any(|m| m.model_id.0.as_ref() == id);
    if !default_mode_id.is_empty() && in_set(default_mode_id) {
        acp::ModelId::from(default_mode_id.to_owned())
    } else if let Some(first) = available.first() {
        first.model_id.clone()
    } else {
        acp::ModelId::from(String::new())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::chat_models_client::ModeAvailability;
    fn available(id: &str, title: &str) -> Mode {
        Mode {
            id: id.to_owned(),
            title: title.to_owned(),
            availability: ModeAvailability {
                available: Some(serde_json::json!({})),
                ..Default::default()
            },
            ..Default::default()
        }
    }
    fn requires_upgrade(id: &str) -> Mode {
        Mode {
            id: id.to_owned(),
            availability: ModeAvailability {
                requires_upgrade: Some(serde_json::json!({ "message": "Upgrade" })),
                ..Default::default()
            },
            ..Default::default()
        }
    }
    #[test]
    fn filters_to_available_modes() {
        let resp = ListModesResponse {
            modes: vec![
                available("auto", "Auto"),
                requires_upgrade("heavy"),
                available("fast", "Fast"),
            ],
            default_mode_id: "auto".to_owned(),
        };
        let state = modes_to_model_state(&resp);
        let ids: Vec<String> = state
            .available_models
            .iter()
            .map(|m| m.model_id.0.to_string())
            .collect();
        assert_eq!(ids, vec!["auto".to_string(), "fast".to_string()]);
        assert_eq!(state.current_model_id.0.as_ref(), "auto");
    }
    #[test]
    fn default_outside_filtered_set_falls_back_to_first_available() {
        let resp = ListModesResponse {
            modes: vec![requires_upgrade("heavy"), available("fast", "Fast")],
            default_mode_id: "heavy".to_owned(),
        };
        let state = modes_to_model_state(&resp);
        assert_eq!(state.current_model_id.0.as_ref(), "fast");
        assert!(
            state
                .available_models
                .iter()
                .any(|m| m.model_id == state.current_model_id)
        );
    }
    #[test]
    fn empty_default_falls_back_to_first() {
        let resp = ListModesResponse {
            modes: vec![available("a", "A"), available("b", "B")],
            default_mode_id: String::new(),
        };
        let state = modes_to_model_state(&resp);
        assert_eq!(state.current_model_id.0.as_ref(), "a");
    }
    #[test]
    fn no_available_modes_yields_empty_current() {
        let resp = ListModesResponse {
            modes: vec![requires_upgrade("heavy")],
            default_mode_id: "heavy".to_owned(),
        };
        let state = modes_to_model_state(&resp);
        assert!(state.available_models.is_empty());
        assert_eq!(state.current_model_id.0.as_ref(), "");
    }
    #[test]
    fn maps_fields_and_meta() {
        let mut m = available("auto", "Auto");
        m.description = "Picks the best model".to_owned();
        m.badge_text = Some("New".to_owned());
        m.icon_hint = "rocket".to_owned();
        m.tags = vec!["TAG_PRIMARY".to_owned()];
        let resp = ListModesResponse {
            modes: vec![m],
            default_mode_id: "auto".to_owned(),
        };
        let state = modes_to_model_state(&resp);
        let info = &state.available_models[0];
        assert_eq!(info.name, "Auto");
        assert_eq!(info.description.as_deref(), Some("Picks the best model"));
        let meta = info.meta.as_ref().unwrap();
        assert_eq!(meta["badgeText"], serde_json::json!("New"));
        assert_eq!(meta["iconHint"], serde_json::json!("rocket"));
        assert_eq!(meta["tags"], serde_json::json!(["TAG_PRIMARY"]));
    }
    #[test]
    fn name_falls_back_to_id_when_title_blank() {
        let mut m = available("grok-4.5", "");
        m.title = "   ".to_owned();
        let resp = ListModesResponse {
            modes: vec![m],
            default_mode_id: String::new(),
        };
        let state = modes_to_model_state(&resp);
        assert_eq!(state.available_models[0].name, "grok-4.5");
    }
}
