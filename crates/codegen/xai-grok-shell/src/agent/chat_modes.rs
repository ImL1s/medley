//! grok.com chat-product model catalog: caches `/rest/modes` and maps modes to
//! the `SessionModelState` returned by `load_chat_session` (the chat analogue of
//! [`crate::agent::models::ModelsManager`]). NB: these "modes" populate the
//! desktop MODEL picker, not the ACP session plan-modes in `LoadSessionResponse.modes`.
use crate::auth::AuthManager;
use crate::remote::chat_models_client::{
    ChatModelsClient, ChatModelsError, ListModesResponse, Mode,
};
use agent_client_protocol as acp;
#[cfg(test)]
use parking_lot::Mutex as TestMutex;
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
/// Whether a [`ChatModesManager::model_state_with_authority`] snapshot is
/// **fresh enough to justify rejecting a request**, not merely whether a
/// response object exists (#483 review, rounds 2 and 3).
///
/// The axis is deliberately about consequence, not presence: refusing to
/// spawn a chat session is hard to undo; reporting a possibly-stale model
/// name for display is not. Measured against the actual fetch/cache logic
/// in [`ChatModesManager::model_state_with_authority`], there are four
/// distinct source states — fresh-non-empty, fresh-empty, stale, and
/// no-data-at-all — and the first two are `Authoritative` while the last
/// two are `NoInfo`, because staleness carries the same "cannot justify a
/// reject" risk as having nothing at all (entitlement can change in either
/// direction since a stale fetch). See that method's doc for the full
/// mapping table and why two earlier attempts at this split each got one
/// of these four cells wrong.
#[derive(Debug, Clone)]
pub(crate) enum ChatModelCatalog {
    /// A fresh (non-stale), successful `/rest/modes` response.
    /// `available_models` may still be empty — every mode currently
    /// requiring an upgrade, or the server itself returning zero modes, are
    /// both real, authoritative answers, not missing information.
    Authoritative(acp::SessionModelState),
    /// Not fresh enough to reject a request on: no response was ever
    /// obtained (unauthenticated, auth changed mid-fetch, a fetch failure
    /// with nothing cached), **or** the response held is stale (serving
    /// cached data while a background refresh runs, or cached data used
    /// after a failed live fetch). The carried state may still be
    /// non-empty in the stale sub-case — display tolerates staleness even
    /// though eligibility decisions must not rely on it.
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
    /// API keys share an empty `user_id`; this generation distinguishes
    /// credential replacements (#483 review).
    auth_generation: u64,
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
    /// Test-only network seam (#483 review, round 3): there is no mockable
    /// `ChatModelsClient`, so this is the only way to deterministically
    /// drive [`ChatModesManager::fetch`]'s `Ok`/`Err` outcomes -- in
    /// particular `Ok` with an empty `modes` list, the exact shape the
    /// round-3 review found mistagged. Consumed once (`.take()`) so a test
    /// controls exactly one fetch.
    #[cfg(test)]
    fetch_override: TestMutex<Option<Result<ListModesResponse, ChatModelsError>>>,
    /// Test-only: pause `fetch` after it has started so a test can
    /// `hot_swap` the credential before the response is stored (#483).
    #[cfg(test)]
    fetch_hold: TestMutex<
        Option<(
            std::sync::mpsc::SyncSender<()>,
            std::sync::mpsc::Receiver<()>,
        )>,
    >,
    /// Test-only: after the hold, republish the same credential so
    /// `auth()`-style generation bumps during a fetch are visible (#483).
    #[cfg(test)]
    simulate_in_fetch_auth_refresh: TestMutex<bool>,
}
impl ChatModesManager {
    pub(crate) fn new(auth: Arc<AuthManager>) -> Self {
        Self {
            inner: Arc::new(Inner {
                auth,
                cache: RwLock::new(None),
                fetch_lock: tokio::sync::Mutex::new(()),
                #[cfg(test)]
                fetch_override: TestMutex::new(None),
                #[cfg(test)]
                fetch_hold: TestMutex::new(None),
                #[cfg(test)]
                simulate_in_fetch_auth_refresh: TestMutex::new(false),
            }),
        }
    }
    /// Test-only: make the next [`Self::fetch`] call return `value` instead
    /// of hitting the network. Consumed once.
    #[cfg(test)]
    pub(crate) fn set_fetch_override_for_test(
        &self,
        value: Result<ListModesResponse, ChatModelsError>,
    ) {
        *self.inner.fetch_override.lock() = Some(value);
    }
    /// Test-only: `fetch` signals on `started` then blocks on `proceed`
    /// so a test can replace the API key mid-flight (#483 review).
    #[cfg(test)]
    pub(crate) fn set_fetch_hold_for_test(
        &self,
        started: std::sync::mpsc::SyncSender<()>,
        proceed: std::sync::mpsc::Receiver<()>,
    ) {
        *self.inner.fetch_hold.lock() = Some((started, proceed));
    }
    /// Test-only: next `fetch` republishes the current credential after the
    /// hold, the way `AuthManager::auth()` bumps generation on token refresh.
    #[cfg(test)]
    pub(crate) fn set_simulate_in_fetch_auth_refresh_for_test(&self) {
        *self.inner.simulate_in_fetch_auth_refresh.lock() = true;
    }
    /// The active grok.com identity, or `None` when unauthenticated. Modes are
    /// per-identity (tier/ACL), so every cache key and store is gated on it.
    pub(crate) fn current_user_id(&self) -> Option<String> {
        self.inner.auth.current_or_expired().map(|a| a.user_id)
    }
    pub(crate) fn current_auth_generation(&self) -> u64 {
        self.inner.auth.current_selection_generation()
    }
    fn cache_matches(&self, cached: &CachedModes, user_id: &str, locale: &str) -> bool {
        cached.user_id == user_id
            && cached.locale == locale
            && cached.auth_generation == self.current_auth_generation()
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
    /// The axis this method actually distinguishes is not "did we get a
    /// response" but **"is this fresh enough to justify rejecting a
    /// request"** — because that is the asymmetry that matters: refusing to
    /// spawn a session is a consequential, hard-to-undo action; reporting a
    /// possibly-stale model name for display is not (#483 review, round 3).
    /// Measured against the actual fetch/cache logic below, there are four
    /// distinct states, not two, and they do not all resolve the same way:
    ///
    /// | State | `available_models` | Fresh confirmation? | Tag |
    /// |---|---|---|---|
    /// | Fresh, non-empty | non-empty | yes | `Authoritative` |
    /// | Fresh, empty (every mode requires upgrade, or the server itself returned zero modes) | empty | yes | `Authoritative` |
    /// | Stale (serving cached data while a background refresh runs, or after a fetch failure) | whatever the cache holds | **no** | `NoInfo` (data still returned for display) |
    /// | No data at all (unauthenticated, auth changed mid-fetch, fetch failed with nothing cached) | empty | no | `NoInfo` |
    ///
    /// Two rows collapse to `NoInfo` for the same underlying reason:
    /// entitlement can change in *either* direction since a stale fetch —
    /// access revoked (should now reject, but stale data would still accept)
    /// or newly granted (would be wrongly rejected on stale data) — so stale
    /// data is not confident enough to justify rejection either way, exactly
    /// like having no data. It is still returned (not discarded) because
    /// display tolerates staleness the way it already tolerates an
    /// out-of-catalog id (see [`chat_new_session_model_state`]'s "picker may
    /// diverge from catalog" comment).
    ///
    /// The two rows that collapsed the *other* way in an earlier version of
    /// this method — "the server returned zero modes" was `NoInfo`, "stale
    /// cache" was `Authoritative` — were both wrong, caught across two
    /// separate review rounds. See [`ChatModelCatalog`].
    pub(crate) async fn model_state_with_authority(&self) -> ChatModelCatalog {
        let Some(user_id) = self.current_user_id() else {
            return ChatModelCatalog::NoInfo(empty_state());
        };
        let locale = DEFAULT_LOCALE;
        let auth_generation = self.current_auth_generation();
        {
            let guard = self.inner.cache.read();
            if let Some(c) = guard.as_ref()
                && self.cache_matches(c, &user_id, locale)
            {
                if c.fetched_at.elapsed() < CACHE_TTL {
                    return ChatModelCatalog::Authoritative(modes_to_model_state(&c.response));
                }
                // Stale: real data, but not fresh enough to reject a
                // request on (see the table above). Serve it for display
                // and let the caller trust the request; refresh in the
                // background so the *next* request gets a fresh answer.
                let stale = c.response.clone();
                drop(guard);
                self.spawn_refresh(user_id, locale);
                return ChatModelCatalog::NoInfo(modes_to_model_state(&stale));
            }
        }
        let _flight = self.inner.fetch_lock.lock().await;
        {
            let guard = self.inner.cache.read();
            if let Some(c) = guard.as_ref()
                && self.cache_matches(c, &user_id, locale)
                && c.fetched_at.elapsed() < CACHE_TTL
            {
                return ChatModelCatalog::Authoritative(modes_to_model_state(&c.response));
            }
        }
        match self.fetch(locale).await {
            Ok((resp, sent_generation)) => {
                if self.current_user_id().as_deref() != Some(user_id.as_str())
                    || self.current_auth_generation() != sent_generation
                {
                    return ChatModelCatalog::NoInfo(empty_state());
                }
                let mapped = modes_to_model_state(&resp);
                if mapped.available_models.is_empty() {
                    tracing::warn!(
                        raw_modes = resp.modes.len(),
                        "chat modes: authoritative fetch produced zero available modes \
                         (either the server returned none, or none passed the availability filter)"
                    );
                }
                // A fresh, successful, non-stale response is authoritative
                // regardless of whether its emptiness came from the raw
                // `resp.modes` list itself being empty or from every mode
                // failing the availability filter: both mean the same real
                // thing (this authenticated user currently has zero usable
                // chat modes), so both get to say `Unavailable` for a
                // requested id rather than being silently trusted through.
                // Cached either way -- caching behavior itself is unchanged
                // by this fix, only the authority tag is.
                self.store(user_id, locale.to_owned(), resp, sent_generation);
                ChatModelCatalog::Authoritative(mapped)
            }
            Err(err) => {
                tracing::warn!(error = %err, "chat modes fetch failed; serving cache/empty");
                let guard = self.inner.cache.read();
                match guard.as_ref() {
                    // Cached data after a failed live fetch carries the
                    // same staleness risk as the background-refresh case
                    // above, just reached via a different path -- treated
                    // the same way: displayable, not authoritative.
                    Some(c) if self.cache_matches(c, &user_id, locale) => {
                        ChatModelCatalog::NoInfo(modes_to_model_state(&c.response))
                    }
                    _ => ChatModelCatalog::NoInfo(empty_state()),
                }
            }
        }
    }
    async fn fetch(&self, locale: &str) -> Result<(ListModesResponse, u64), ChatModelsError> {
        let start_generation = self.current_auth_generation();
        #[cfg(test)]
        {
            let held = self.inner.fetch_hold.lock().take();
            if let Some((started, proceed)) = held {
                let _ = started.send(());
                let _ = tokio::task::spawn_blocking(move || proceed.recv()).await;
            }
            let refreshed = std::mem::take(&mut *self.inner.simulate_in_fetch_auth_refresh.lock());
            if refreshed && let Some(auth) = self.inner.auth.current() {
                self.inner.auth.hot_swap(auth);
            }
            if let Some(overridden) = self.inner.fetch_override.lock().take() {
                let generation = if refreshed {
                    self.current_auth_generation()
                } else {
                    start_generation
                };
                return overridden.map(|resp| (resp, generation));
            }
        }
        let client = ChatModelsClient::new(self.inner.auth.clone());
        // `list_modes` takes credential and generation from one
        // `auth_with_generation` snapshot. Sampling generation separately
        // from `auth()` can pair key A with B's generation across a
        // concurrent swap; API keys share an empty `user_id` (#483 review).
        match tokio::time::timeout(COLD_FETCH_TIMEOUT, client.list_modes(locale)).await {
            Ok(result) => result,
            Err(_elapsed) => Err(ChatModelsError::Timeout),
        }
    }
    fn store(
        &self,
        user_id: String,
        locale: String,
        response: ListModesResponse,
        auth_generation: u64,
    ) {
        *self.inner.cache.write() = Some(CachedModes {
            user_id,
            locale,
            auth_generation,
            fetched_at: Instant::now(),
            response,
        });
    }
    /// Best-effort stale refresh; skips if a fetch is already in flight.
    fn spawn_refresh(&self, user_id: String, locale: &'static str) {
        let generation = self.current_auth_generation();
        let me = self.clone();
        tokio::spawn(async move {
            let Ok(_flight) = me.inner.fetch_lock.try_lock() else {
                return;
            };
            if me.current_user_id().as_deref() != Some(user_id.as_str())
                || me.current_auth_generation() != generation
            {
                return;
            }
            if let Ok((resp, sent_generation)) = me.fetch(locale).await
                && me.current_user_id().as_deref() == Some(user_id.as_str())
                && me.current_auth_generation() == sent_generation
            {
                me.store(user_id, locale.to_owned(), resp, sent_generation);
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
    /// Test-only: seed the cache directly, at a controlled age, without a
    /// network fetch — the only way to deterministically reach the
    /// fresh-cache-hit and stale-cache-hit branches of
    /// [`Self::model_state_with_authority`] (#483 review, round 3). `age`
    /// is measured against [`CACHE_TTL`]; pass `Duration::ZERO` for fresh,
    /// anything `>= CACHE_TTL` for stale.
    #[cfg(test)]
    pub(crate) fn seed_cache_for_test(
        &self,
        user_id: impl Into<String>,
        age: Duration,
        response: ListModesResponse,
    ) {
        *self.inner.cache.write() = Some(CachedModes {
            user_id: user_id.into(),
            locale: DEFAULT_LOCALE.to_owned(),
            auth_generation: self.current_auth_generation(),
            fetched_at: Instant::now()
                .checked_sub(age)
                .expect("test age must not underflow Instant"),
            response,
        });
    }

    #[cfg(test)]
    pub(crate) fn auth_for_test(&self) -> Arc<AuthManager> {
        self.inner.auth.clone()
    }

    #[cfg(test)]
    fn cached_modes_for_test(&self) -> Option<ListModesResponse> {
        self.inner.cache.read().as_ref().map(|c| c.response.clone())
    }

    #[cfg(test)]
    fn cached_auth_generation_for_test(&self) -> Option<u64> {
        self.inner.cache.read().as_ref().map(|c| c.auth_generation)
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

    /// #483 review, round 3: an authenticated `ChatModesManager` with a
    /// user_id-matched cache entry seeded directly (no network fetch --
    /// there is no mock `ChatModelsClient`, and the fresh/stale-cache-hit
    /// branches don't need one; they're reached identically whether the
    /// cache was populated by a real fetch or seeded here, since both paths
    /// converge on the same `Authoritative(modes_to_model_state(&response))`
    /// / staleness tagging logic this test exercises).
    fn manager_authenticated_as(user_id: &str) -> ChatModesManager {
        use crate::auth::GrokAuth;
        let temp_dir = tempfile::tempdir().expect("temp auth root");
        let auth = std::sync::Arc::new(crate::auth::AuthManager::new(
            temp_dir.path(),
            crate::auth::GrokComConfig::default(),
        ));
        auth.hot_swap(GrokAuth {
            user_id: user_id.to_owned(),
            ..GrokAuth::test_default()
        });
        ChatModesManager::new(auth)
    }

    /// #483 review, round 3: the four states `model_state_with_authority`
    /// actually distinguishes, measured against the real fetch/cache logic
    /// rather than assumed. The two cells that must **not** collapse into
    /// each other, despite superficially similar data shapes, are the pair
    /// this round's review caught wrong in two different directions:
    /// "fresh but empty" (an authoritative zero-modes answer) must be
    /// `Authoritative`, and "stale but non-empty" (real data, just not
    /// fresh enough to reject on) must be `NoInfo` -- the opposite of what
    /// an emptiness-only check would produce for each.
    #[tokio::test(flavor = "current_thread")]
    async fn model_state_with_authority_distinguishes_all_four_states() {
        const USER: &str = "chat-483-round3-user";

        // Fresh, non-empty: no cache, a live fetch (via the test override,
        // since there is no mockable ChatModelsClient) succeeds with real
        // modes. Drives the actual `Ok(resp) => {...}` branch in
        // `model_state_with_authority`, not a cache-hit shortcut.
        let manager = manager_authenticated_as(USER);
        manager.set_fetch_override_for_test(Ok(ListModesResponse {
            modes: vec![available("auto", "Auto")],
            default_mode_id: "auto".to_owned(),
        }));
        match manager.model_state_with_authority().await {
            ChatModelCatalog::Authoritative(state) => {
                assert!(
                    !state.available_models.is_empty(),
                    "[fresh_non_empty] must carry the real modes"
                );
            }
            other => panic!("[fresh_non_empty] expected Authoritative, got {other:?}"),
        }

        // Fresh, empty: no cache, a live fetch succeeds but every mode
        // requires an upgrade -- a real, authoritative "zero available"
        // answer, not missing information. This is the *exact* code path
        // Codex's round-3 review flagged (`Ok(_)` with `resp.modes` after
        // the availability filter producing zero entries) -- must not
        // collapse into `NoInfo` just because `available_models` is empty.
        let manager = manager_authenticated_as(USER);
        manager.set_fetch_override_for_test(Ok(ListModesResponse {
            modes: vec![requires_upgrade("heavy")],
            default_mode_id: "heavy".to_owned(),
        }));
        match manager.model_state_with_authority().await {
            ChatModelCatalog::Authoritative(state) => {
                assert!(
                    state.available_models.is_empty(),
                    "[fresh_empty] the fetched response has zero available modes"
                );
            }
            other => panic!("[fresh_empty] expected Authoritative, got {other:?}"),
        }

        // Fresh, empty (the *other* shape #483 review flagged): the server
        // itself returns zero modes at all, not merely zero available
        // ones. Same authoritative answer, same code path, different raw
        // shape -- must land the same place as the case above.
        let manager = manager_authenticated_as(USER);
        manager.set_fetch_override_for_test(Ok(ListModesResponse {
            modes: Vec::new(),
            default_mode_id: String::new(),
        }));
        match manager.model_state_with_authority().await {
            ChatModelCatalog::Authoritative(state) => {
                assert!(
                    state.available_models.is_empty(),
                    "[fresh_empty_raw] a zero-mode response is still empty"
                );
            }
            other => panic!("[fresh_empty_raw] expected Authoritative, got {other:?}"),
        }

        // Stale: a cache entry older than CACHE_TTL, carrying real,
        // non-empty data. Must NOT collapse into Authoritative just because
        // it has data -- staleness means entitlement could have changed in
        // either direction since this response was fetched.
        let manager = manager_authenticated_as(USER);
        manager.seed_cache_for_test(
            USER,
            CACHE_TTL + Duration::from_secs(1),
            ListModesResponse {
                modes: vec![available("auto", "Auto")],
                default_mode_id: "auto".to_owned(),
            },
        );
        match manager.model_state_with_authority().await {
            ChatModelCatalog::NoInfo(state) => {
                assert!(
                    !state.available_models.is_empty(),
                    "[stale] must still carry the stale data for display, not discard it"
                );
            }
            other => panic!("[stale] expected NoInfo, got {other:?}"),
        }

        // No data at all: unauthenticated. Empty, and NoInfo for the same
        // reason `Unavailable` would be wrong here -- there is nothing to
        // reject the request against.
        let temp_dir = tempfile::tempdir().expect("temp auth root");
        let unauthenticated = ChatModesManager::new(std::sync::Arc::new(
            crate::auth::AuthManager::new(temp_dir.path(), crate::auth::GrokComConfig::default()),
        ));
        match unauthenticated.model_state_with_authority().await {
            ChatModelCatalog::NoInfo(state) => {
                assert!(
                    state.available_models.is_empty(),
                    "[no_data] unauthenticated must carry nothing"
                );
            }
            other => panic!("[no_data] expected NoInfo, got {other:?}"),
        }

        // A second flavor of "no data": authenticated, no cache, but the
        // live fetch itself fails. Same NoInfo(empty) outcome as being
        // unauthenticated, reached through a different code branch
        // (`Err(err) => { ... _ => NoInfo(empty_state()) }`).
        let manager = manager_authenticated_as(USER);
        manager.set_fetch_override_for_test(Err(
            crate::remote::chat_models_client::ChatModelsError::Timeout,
        ));
        match manager.model_state_with_authority().await {
            ChatModelCatalog::NoInfo(state) => {
                assert!(
                    state.available_models.is_empty(),
                    "[fetch_failure_no_cache] nothing was ever cached, so there is nothing to fall back to"
                );
            }
            other => panic!("[fetch_failure_no_cache] expected NoInfo, got {other:?}"),
        }
    }

    /// #483 review, round 4: `spawn_refresh` used to skip `Ok` responses
    /// whose `modes` list was empty, so a successful empty refresh never
    /// replaced a stale non-empty cache. Every later `session/new` then
    /// stayed `NoInfo` and trusted any model. Store the empty success so
    /// the next snapshot is `Authoritative` empty.
    #[tokio::test(flavor = "current_thread")]
    async fn spawn_refresh_stores_a_successful_empty_response() {
        const USER: &str = "chat-483-empty-refresh-user";
        let manager = manager_authenticated_as(USER);
        manager.seed_cache_for_test(
            USER,
            CACHE_TTL + Duration::from_secs(1),
            ListModesResponse {
                modes: vec![available("auto", "Auto")],
                default_mode_id: "auto".to_owned(),
            },
        );
        manager.set_fetch_override_for_test(Ok(ListModesResponse {
            modes: vec![],
            default_mode_id: String::new(),
        }));
        match manager.model_state_with_authority().await {
            ChatModelCatalog::NoInfo(state) => {
                assert!(
                    !state.available_models.is_empty(),
                    "[stale] must still carry display data while the refresh is in flight"
                );
            }
            other => panic!("[stale] expected NoInfo, got {other:?}"),
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            match manager.cached_modes_for_test() {
                Some(resp) if resp.modes.is_empty() => break,
                _ if std::time::Instant::now() >= deadline => {
                    panic!("background refresh never stored the empty success");
                }
                _ => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
        match manager.model_state_with_authority().await {
            ChatModelCatalog::Authoritative(state) => {
                assert!(
                    state.available_models.is_empty(),
                    "[refreshed] a stored empty success is authoritative"
                );
            }
            other => panic!("[refreshed] expected Authoritative empty, got {other:?}"),
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

    #[tokio::test(flavor = "current_thread")]
    async fn cache_misses_when_auth_generation_changes_for_the_same_user_id() {
        use crate::auth::GrokAuth;

        let manager = manager_authenticated_as("");
        manager.seed_cache_for_test(
            "",
            Duration::ZERO,
            ListModesResponse {
                modes: vec![available("a", "A")],
                default_mode_id: "a".to_owned(),
            },
        );
        manager.auth_for_test().hot_swap(GrokAuth {
            user_id: String::new(),
            key: "second-api-key".into(),
            ..GrokAuth::test_default()
        });
        manager.set_fetch_override_for_test(Ok(ListModesResponse {
            modes: vec![available("b", "B")],
            default_mode_id: "b".to_owned(),
        }));
        match manager.model_state_with_authority().await {
            ChatModelCatalog::Authoritative(state) => {
                assert_eq!(
                    state.current_model_id.0.as_ref(),
                    "b",
                    "a replaced API key must not reuse the previous key's catalog"
                );
            }
            other => panic!("expected Authoritative after generation change, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn spawn_refresh_does_not_stamp_a_fetch_onto_a_newer_generation() {
        use crate::auth::GrokAuth;

        let manager = manager_authenticated_as("");
        manager.seed_cache_for_test(
            "",
            CACHE_TTL + Duration::from_secs(1),
            ListModesResponse {
                modes: vec![available("a", "A")],
                default_mode_id: "a".to_owned(),
            },
        );
        let generation_before = manager
            .cached_auth_generation_for_test()
            .expect("seeded cache has a generation");
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let (proceed_tx, proceed_rx) = std::sync::mpsc::channel();
        manager.set_fetch_hold_for_test(started_tx, proceed_rx);
        manager.set_fetch_override_for_test(Ok(ListModesResponse {
            modes: vec![available("b", "B")],
            default_mode_id: "b".to_owned(),
        }));
        match manager.model_state_with_authority().await {
            ChatModelCatalog::NoInfo(_) => {}
            other => panic!("[stale] expected NoInfo while refresh is in flight, got {other:?}"),
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if started_rx.try_recv().is_ok() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!("background refresh never entered fetch");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        manager.auth_for_test().hot_swap(GrokAuth {
            user_id: String::new(),
            key: "second-api-key".into(),
            ..GrokAuth::test_default()
        });
        proceed_tx.send(()).expect("unblock fetch");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if std::time::Instant::now() >= deadline {
                break;
            }
        }
        let stored = manager
            .cached_modes_for_test()
            .expect("seeded cache remains");
        assert_eq!(
            stored.default_mode_id, "a",
            "a fetch started under key A must not replace the cache after key B lands"
        );
        assert_eq!(
            manager.cached_auth_generation_for_test(),
            Some(generation_before),
            "must not stamp A's response with B's generation"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn in_fetch_token_refresh_keeps_the_successful_catalog() {
        // A `/rest/modes` request that refreshes its own bearer bumps
        // selection generation without changing user_id. Discarding that
        // response as NoInfo would leave session/new with CatalogUnavailable
        // (#483 review).
        let manager = manager_authenticated_as("alice");
        manager.seed_cache_for_test(
            "alice",
            CACHE_TTL + Duration::from_secs(1),
            ListModesResponse {
                modes: vec![available("a", "A")],
                default_mode_id: "a".to_owned(),
            },
        );
        let generation_before = manager
            .cached_auth_generation_for_test()
            .expect("seeded cache has a generation");
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let (proceed_tx, proceed_rx) = std::sync::mpsc::channel();
        manager.set_fetch_hold_for_test(started_tx, proceed_rx);
        manager.set_simulate_in_fetch_auth_refresh_for_test();
        manager.set_fetch_override_for_test(Ok(ListModesResponse {
            modes: vec![available("fresh", "Fresh")],
            default_mode_id: "fresh".to_owned(),
        }));
        match manager.model_state_with_authority().await {
            ChatModelCatalog::NoInfo(_) => {}
            other => panic!("[stale] expected NoInfo while refresh is in flight, got {other:?}"),
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if started_rx.try_recv().is_ok() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!("background refresh never entered fetch");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        proceed_tx.send(()).expect("unblock fetch");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let stored = manager.cached_modes_for_test();
            if stored
                .as_ref()
                .is_some_and(|s| s.default_mode_id == "fresh")
            {
                assert!(
                    manager.cached_auth_generation_for_test().unwrap() > generation_before,
                    "token refresh must store under the post-refresh generation"
                );
                return;
            }
            if std::time::Instant::now() >= deadline {
                panic!(
                    "expected the refreshed catalog to be kept, got {:?}",
                    manager.cached_modes_for_test()
                );
            }
        }
    }
}
