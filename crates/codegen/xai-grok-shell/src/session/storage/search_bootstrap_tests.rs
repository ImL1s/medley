//! Gate tests inject a fresh BootstrapProgress and assert only on their own
//! per-tmpdir database state.

use super::*;
use crate::session::info::Info;
use crate::session::storage::jsonl::JsonlStorageAdapter;
use crate::session::storage::search_fts::META_KEY_SCHEMA_VERSION;
use agent_client_protocol as acp;
use serial_test::serial;

fn write_last_bootstrap_at(db_path: &Path) -> io::Result<()> {
    let now = chrono::Utc::now().timestamp();
    with_search_index(db_path, |index| {
        index.set_meta(META_KEY_LAST_BOOTSTRAP, &now.to_string())
    })
}

const TEST_TIMING: BootstrapTiming = BootstrapTiming {
    lease: Duration::from_secs(300),
    refresh: Duration::from_millis(50),
    peer_wait: Duration::from_millis(200),
    poll: Duration::from_millis(10),
};
const _: () = assert!(TEST_TIMING.refresh.as_millis() < TEST_TIMING.lease.as_millis());
const _: () = assert!(TEST_TIMING.poll.as_millis() < TEST_TIMING.peer_wait.as_millis());

/// Timing for the contended-gates test, which parks both gates on a seeded
/// peer claim until each has *deterministically* signalled that it observed
/// contention (see `test_concurrent_gates_single_flight`'s `peer_seen_hook`
/// wiring) — the peer wait only needs to comfortably outlast that
/// handshake, not a guessed hold duration.
const CONTENDED_TIMING: BootstrapTiming = BootstrapTiming {
    lease: TEST_TIMING.lease,
    refresh: TEST_TIMING.refresh,
    peer_wait: Duration::from_secs(30),
    poll: TEST_TIMING.poll,
};
/// Hang guard for `peer_seen_hook.notified()` below: on a correctly
/// functioning claim (mutual exclusion actually holding), both gates'
/// *first* claim attempt fails near-instantly while the seeded peer claim
/// is held, so this bound is never close to binding — it exists only to
/// turn "the claim mechanism silently let a first attempt through" into a
/// clean, distinguishable failure instead of a hung test. See #440.
const PEER_SEEN_HANG_GUARD: Duration = Duration::from_secs(10);

fn stamp_marker(db_path: &Path, value: &str) {
    with_search_index(db_path, |index| {
        index.set_meta(META_KEY_LAST_BOOTSTRAP, value)
    })
    .unwrap();
}

fn read_marker(db_path: &Path) -> Option<String> {
    with_search_index(db_path, |index| index.get_meta(META_KEY_LAST_BOOTSTRAP)).unwrap()
}

/// #475: `reindex_all`'s `epoch.changed()` check (`search_bootstrap.rs`)
/// reads `search_recovery::CACHE_EPOCH`, a *process-global* counter — not
/// scoped to this test's own tmpdir'd db. In production that is correct:
/// `SEARCH_INDEX_MANAGER` is a process-wide singleton (see `search.rs`'s own
/// note on it), so exactly one cache ever exists and "the epoch changed" and
/// "my cache's epoch changed" are the same fact. In this test binary they
/// are not: `test_shared_index_reopens_after_epoch_change` below bumps the
/// same global counter from its own, unrelated tmpdir. Paired with that test
/// under the default parallel harness this failed 9/15; isolated, 0/10;
/// serialized (`--test-threads=1`), 0/10 — confirming the coupling is the
/// concurrent scheduling, not the assertion. `#[serial(search_cache_epoch)]`
/// (same idiom as #319's EnvGuard tests and `heap_profile_monitor`'s named
/// group) makes that scheduling impossible rather than merely unlikely: every
/// test that can observe or move this counter carries the same tag, listed
/// where the counter is defined (`search_recovery.rs`).
#[tokio::test]
#[serial(search_cache_epoch)]
async fn test_claimant_reindexes_even_when_marker_exists() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = search_db_path(tmp.path());
    stamp_marker(&db_path, "123");

    let storage = JsonlStorageAdapter::with_root(tmp.path().to_path_buf());
    bootstrap_with_lease_inner(
        tmp.path(),
        &storage,
        &Arc::new(BootstrapProgress::default()),
        &TEST_TIMING,
        BootstrapRole::Launch,
        None,
    )
    .await
    .unwrap();

    // The reindex rewrote the marker and released the claim.
    assert_ne!(read_marker(&db_path).as_deref(), Some("123"));
    let claim =
        with_search_index(&db_path, |index| index.get_meta(META_KEY_BOOTSTRAP_CLAIM)).unwrap();
    assert_eq!(claim, None);
}

#[tokio::test]
async fn test_has_completed_bootstrap_marker_lifecycle() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    let db_path = search_db_path(root);

    assert_eq!(has_completed_bootstrap_marker(root).await, Some(false));

    write_last_bootstrap_at(&db_path).unwrap();
    assert_eq!(has_completed_bootstrap_marker(root).await, Some(true));

    // An older binary re-stamped a downgraded schema version.
    {
        let index = SessionSearchIndex::open_or_create(&db_path).unwrap();
        index.set_meta(META_KEY_SCHEMA_VERSION, "3").unwrap();
    }
    assert_eq!(
        has_completed_bootstrap_marker(root).await,
        Some(false),
        "a downgraded index must not count as bootstrapped even with a recent marker"
    );

    write_last_bootstrap_at(&db_path).unwrap();
    assert_eq!(has_completed_bootstrap_marker(root).await, Some(true));
}

#[tokio::test]
async fn test_waiter_adopts_peer_marker_without_reindexing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = search_db_path(tmp.path());

    let now = chrono::Utc::now().timestamp();
    with_search_index(&db_path, |index| {
        index.try_claim_bootstrap(now, TEST_TIMING.lease, "peer")
    })
    .unwrap();
    stamp_marker(&db_path, "123");

    let storage = JsonlStorageAdapter::with_root(tmp.path().to_path_buf());
    bootstrap_with_lease_inner(
        tmp.path(),
        &storage,
        &Arc::new(BootstrapProgress::default()),
        &TEST_TIMING,
        BootstrapRole::Launch,
        None,
    )
    .await
    .unwrap();

    assert_eq!(read_marker(&db_path).as_deref(), Some("123"));
}

#[tokio::test]
async fn test_try_bootstrap_returns_at_once_when_peer_holds_claim() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = search_db_path(tmp.path());

    let now = chrono::Utc::now().timestamp();
    with_search_index(&db_path, |index| {
        index.try_claim_bootstrap(now, TIMING.lease, "peer")
    })
    .unwrap();

    let storage = JsonlStorageAdapter::with_root(tmp.path().to_path_buf());
    let started = std::time::Instant::now();
    try_bootstrap_with_lease(
        tmp.path(),
        &storage,
        &Arc::new(BootstrapProgress::default()),
    )
    .await
    .unwrap();

    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a held claim must not block the recheck for the full peer wait"
    );
    assert_eq!(read_marker(&db_path), None, "no reindex ran");
}

#[tokio::test]
async fn test_recheck_adopts_marker_completed_after_its_probe() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = search_db_path(tmp.path());
    // A peer finished and released between the recheck's marker probe
    // and its claim attempt: the marker exists and the lease is free.
    stamp_marker(&db_path, "123");

    let storage = JsonlStorageAdapter::with_root(tmp.path().to_path_buf());
    try_bootstrap_with_lease(
        tmp.path(),
        &storage,
        &Arc::new(BootstrapProgress::default()),
    )
    .await
    .unwrap();

    assert_eq!(
        read_marker(&db_path).as_deref(),
        Some("123"),
        "the recheck must adopt the fresh marker, not reindex over it"
    );
    assert!(!has_bootstrap_claim(&db_path).unwrap());
}

#[tokio::test]
async fn test_waiter_gives_up_after_peer_wait() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = search_db_path(tmp.path());

    let now = chrono::Utc::now().timestamp();
    with_search_index(&db_path, |index| {
        index.try_claim_bootstrap(now, TEST_TIMING.lease, "peer")
    })
    .unwrap();

    let storage = JsonlStorageAdapter::with_root(tmp.path().to_path_buf());
    bootstrap_with_lease_inner(
        tmp.path(),
        &storage,
        &Arc::new(BootstrapProgress::default()),
        &TEST_TIMING,
        BootstrapRole::Launch,
        None,
    )
    .await
    .unwrap();

    assert_eq!(read_marker(&db_path), None, "no reindex ran");
}

/// #475: bumps the process-global `search_recovery::CACHE_EPOCH` on every
/// run (`reprobe` always answers "corrupt"). Carries the same
/// `#[serial(search_cache_epoch)]` tag as every test that *reads* that
/// counter — see `test_claimant_reindexes_even_when_marker_exists`.
#[test]
#[serial(search_cache_epoch)]
fn test_shared_index_reopens_after_epoch_change() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = search_db_path(tmp.path());
    let shared = SharedIndex::new();
    shared
        .with(&db_path, |index| index.set_meta("k", "v"))
        .unwrap();

    // A heal bumps the epoch and replaces the file.
    search_recovery::heal_unusable(
        &db_path,
        &rusqlite::Error::QueryReturnedNoRows,
        |_| Ok(false),
        |p| SessionSearchIndex::open_or_create(p).map(|_| ()),
    );

    let value = shared.with(&db_path, |index| index.get_meta("k")).unwrap();
    assert_eq!(
        value, None,
        "the connection must re-open at the new epoch, not keep the old fd"
    );
}

#[test]
fn test_read_write_last_bootstrap_at() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("session_search.sqlite");

    assert_eq!(try_read_last_bootstrap_at(&db_path).unwrap(), None);
    write_last_bootstrap_at(&db_path).unwrap();

    let ts = try_read_last_bootstrap_at(&db_path).unwrap().unwrap();
    let now = chrono::Utc::now().timestamp();
    assert!((now - ts).abs() < 5);
}

#[test]
fn test_clear_last_bootstrap_at() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("session_search.sqlite");

    write_last_bootstrap_at(&db_path).unwrap();
    assert!(try_read_last_bootstrap_at(&db_path).unwrap().is_some());

    clear_last_bootstrap_at(&db_path).unwrap();
    assert_eq!(try_read_last_bootstrap_at(&db_path).unwrap(), None);
}

/// #475: the winning gate reaches `reindex_all`, so this reads
/// `CACHE_EPOCH` the same as `test_claimant_reindexes_even_when_marker_exists`
/// — same tag, same reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(search_cache_epoch)]
async fn test_concurrent_gates_single_flight() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    let storage = JsonlStorageAdapter::with_root(root.clone());
    for id in ["s1", "s2"] {
        let info = Info {
            id: acp::SessionId::new(id),
            cwd: "/ws".to_string(),
        };
        storage
            .init_session(&info, acp::ModelId::new("test"))
            .await
            .unwrap();
    }
    // Seed a peer claim so neither gate can take the lease until both have
    // observed contention. Without it this test is a race it usually loses
    // on an idle machine: gate A claims, indexes two tiny sessions and
    // releases before gate B's first claim attempt even returns, and B —
    // still on its first iteration, where a launch deliberately ignores an
    // existing marker — reindexes too. Once both gates have latched
    // `peer_seen`, the loser's post-claim marker check adopts the winner's
    // work however fast the winner finished.
    //
    // #440: "both have observed contention" used to be established by
    // sleeping `PEER_CLAIM_HOLD` and hoping the scheduler cooperated inside
    // it — measured 9/12, 0/12 and 7/12 failures at 250/2500/250ms on a
    // loaded runner, with production `search_bootstrap` bytes identical
    // across all three arms, i.e. the fixture, not the gate. Each gate now
    // carries its own `Notify`, fired by `bootstrap_with_lease_inner` the
    // instant its `peer_seen` latches (see `PeerSeenHook`), and the peer
    // claim is released only once *both* have actually signalled — the
    // real precondition, driven directly, not guessed at.
    let claim_now = chrono::Utc::now().timestamp();
    with_search_index(&search_db_path(&root), |index| {
        index.try_claim_bootstrap(claim_now, CONTENDED_TIMING.lease, "peer")
    })
    .unwrap();

    let progress_a = Arc::new(BootstrapProgress::default());
    let progress_b = Arc::new(BootstrapProgress::default());
    let storage_a = storage.clone();
    let storage_b = storage.clone();
    let root_a = root.clone();
    let root_b = root;
    let pa = Arc::clone(&progress_a);
    let pb = Arc::clone(&progress_b);
    let peer_seen_a = Arc::new(Notify::new());
    let peer_seen_b = Arc::new(Notify::new());
    let hook_a = Arc::clone(&peer_seen_a);
    let hook_b = Arc::clone(&peer_seen_b);
    // Not load-bearing for correctness any more (the Notify handshake below
    // is what the assertion actually depends on) — kept only so both gates
    // are scheduled at roughly the same time instead of one starting a
    // whole poll interval ahead of the other.
    let start = Arc::new(tokio::sync::Barrier::new(2));
    let start_a = Arc::clone(&start);
    let start_b = Arc::clone(&start);
    let gate_a = tokio::spawn(async move {
        start_a.wait().await;
        bootstrap_with_lease_inner(
            &root_a,
            &storage_a,
            &pa,
            &CONTENDED_TIMING,
            BootstrapRole::Launch,
            Some(hook_a),
        )
        .await
    });
    let gate_b = tokio::spawn(async move {
        start_b.wait().await;
        bootstrap_with_lease_inner(
            &root_b,
            &storage_b,
            &pb,
            &CONTENDED_TIMING,
            BootstrapRole::Launch,
            Some(hook_b),
        )
        .await
    });
    // Both gates are now spinning on the seeded claim. Wait for each to
    // report — via the hook above, not a clock — that its first claim
    // attempt has already failed against it, then hand the claim over. The
    // bound here is a hang guard, not a duration to tune: on a correct
    // implementation both gates fail near-instantly (the peer claim is
    // trivially still held), so this only ever fires when a first claim
    // attempt slipped through despite the seed, which is a claim-exclusivity
    // defect worth reporting distinctly from a timeout.
    match tokio::time::timeout(PEER_SEEN_HANG_GUARD, async {
        peer_seen_a.notified().await;
        peer_seen_b.notified().await;
    })
    .await
    {
        Ok(()) => {}
        Err(_) => panic!(
            "gate(s) never observed peer contention within {PEER_SEEN_HANG_GUARD:?} — likely \
             a first claim attempt going through despite the seeded peer claim still being \
             held (claim exclusivity itself, not a slow runner), but read the state below \
             rather than trust this framing: a gate that snuck through has total > 0 and \
             probably already wrote the marker; a merely unscheduled one has neither. \
             a_total={} b_total={} marker_present={} claim_held={:?}",
            progress_a.total.load(Ordering::Relaxed),
            progress_b.total.load(Ordering::Relaxed),
            read_marker(&search_db_path(tmp.path())).is_some(),
            has_bootstrap_claim(&search_db_path(tmp.path())),
        ),
    }
    with_search_index(&search_db_path(tmp.path()), |index| {
        index.release_bootstrap_claim("peer")
    })
    .unwrap();
    let (a, b) = tokio::join!(gate_a, gate_b);
    let a = a.expect("gate a task panicked");
    let b = b.expect("gate b task panicked");
    assert!(a.is_ok(), "gate a: {a:?}");
    assert!(b.is_ok(), "gate b: {b:?}");

    let db_path = search_db_path(tmp.path());
    assert!(
        read_marker(&db_path).is_some(),
        "completion marker must exist after concurrent gates"
    );
    assert!(
        !has_bootstrap_claim(&db_path).unwrap(),
        "claim must be released after concurrent gates"
    );

    let a_ran = progress_a.total.load(Ordering::Relaxed) > 0;
    let b_ran = progress_b.total.load(Ordering::Relaxed) > 0;
    assert_eq!(
        usize::from(a_ran) + usize::from(b_ran),
        1,
        "exactly one gate must reindex, a_total={}, b_total={}",
        progress_a.total.load(Ordering::Relaxed),
        progress_b.total.load(Ordering::Relaxed),
    );
}
