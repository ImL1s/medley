//! Gate tests inject a fresh BootstrapProgress and assert only on their own
//! per-tmpdir database state.

use super::*;
use crate::session::info::Info;
use crate::session::storage::jsonl::JsonlStorageAdapter;
use crate::session::storage::search_fts::{META_KEY_SCHEMA_VERSION, SessionDoc};
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

// #477: the lease-takeover overlap path (a stale/crashed claimant's reindex
// body running concurrently with its successor's) rests on three properties
// stated in `reindex_all`'s own comments, none of which #440/#476 exercise:
//
//   1. `upsert_doc` is an UPSERT keyed by `session_id` -- concurrent writers
//      cannot structurally corrupt a row. Bootstrap callers additionally
//      fence writes on claim ownership so stale content cannot win (#515).
//   2. `claim_lost` gives a displaced claimant an early exit before it does
//      any (redundant, but wasted) per-session work.
//   3. the completion marker and orphan-prune writes are fenced on claim
//      ownership -- a stale claimant can never assert "done" or delete rows
//      a successor already wrote.
//
// Each gets its own deterministic test below, each paired with a positive
// control so a broken implementation that does *nothing* cannot pass
// vacuously. `test_upsert_doc_converges_under_every_interleaving_of_two_streams`
// is the property-shaped one -- it tests idempotence rather than assuming it.

/// All `C(a_len + b_len, a_len)` ways to riffle-merge two streams of lengths
/// `a_len` and `b_len` while preserving each stream's own internal order --
/// `true` at index `i` means "the i-th write in this interleaving comes from
/// stream A", `false` means stream B. This is the general shape of "two
/// concurrent reindex bodies each issuing their own ordered writes against
/// the same key, in every possible relative order the two could race in."
fn interleavings(a_len: usize, b_len: usize) -> Vec<Vec<bool>> {
    if a_len == 0 {
        return vec![vec![false; b_len]];
    }
    if b_len == 0 {
        return vec![vec![true; a_len]];
    }
    let mut result = Vec::new();
    for mut seq in interleavings(a_len - 1, b_len) {
        seq.insert(0, true);
        result.push(seq);
    }
    for mut seq in interleavings(a_len, b_len - 1) {
        seq.insert(0, false);
        result.push(seq);
    }
    result
}

#[test]
fn test_interleavings_generates_the_expected_count_and_shape() {
    // Self-check on the generator itself: C(6,3) = 20, and every sequence
    // must contain exactly 3 `true`s (stream A never gains or loses writes
    // just because of *when* they were interleaved with B's).
    let all = interleavings(3, 3);
    assert_eq!(all.len(), 20);
    for seq in &all {
        assert_eq!(seq.len(), 6);
        assert_eq!(seq.iter().filter(|&&is_a| is_a).count(), 3);
    }
    // No two sequences are identical -- otherwise this wouldn't be 20
    // *distinct* orderings.
    let mut sorted = all.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), 20);
}

fn stream_doc(session_id: &str, stream: &str, revision: usize) -> SessionDoc {
    SessionDoc {
        session_id: session_id.to_string(),
        cwd: "/ws".to_string(),
        updated_at_unix: revision as i64,
        title: format!("{stream}-rev{revision}"),
        content: format!("content from {stream} revision {revision}"),
        // Nothing inside `upsert_doc` validates this as a real content hash
        // -- using it as a plain revision label makes the read-back after
        // each interleaving unambiguous about exactly which write survived.
        content_hash: format!("{stream}-rev{revision}"),
    }
}

/// Property #1: `upsert_doc` converges to whichever write is *last in a
/// given interleaving*, for every one of the 20 possible ways two 3-write
/// streams targeting the same `session_id` can race, and the row is never
/// duplicated or left partially written regardless of arrival order.
///
/// This is a storage-layer property of `upsert_doc` alone -- it is
/// necessary but **not sufficient** to show overlap is harmless
/// end-to-end. `upsert_doc` has no ownership or recency fence: last
/// write always wins by *arrival order*, not by content freshness. Bootstrap
/// therefore must not call it without an ownership fence. The two tests
/// directly below bind that fence at both the SQL boundary and through
/// `reindex_all` (#515).
#[test]
fn test_upsert_doc_converges_under_every_interleaving_of_two_streams() {
    const SESSION_ID: &str = "s1";
    for pattern in interleavings(3, 3) {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("session_search.sqlite");
        let index = SessionSearchIndex::open_or_create(&db_path).unwrap();

        let mut a_rev = 0usize;
        let mut b_rev = 0usize;
        let mut last = stream_doc(SESSION_ID, "A", 1);
        for is_a in &pattern {
            let doc = if *is_a {
                a_rev += 1;
                stream_doc(SESSION_ID, "A", a_rev)
            } else {
                b_rev += 1;
                stream_doc(SESSION_ID, "B", b_rev)
            };
            last = doc.clone();
            index.upsert_doc(&doc).unwrap();
        }

        let ids = index.all_indexed_session_ids().unwrap();
        assert_eq!(
            ids,
            vec![SESSION_ID.to_string()],
            "pattern {pattern:?}: two overlapping writers must converge on \
             one row per session_id, never a duplicate"
        );
        assert_eq!(
            index.get_doc(SESSION_ID).unwrap().as_ref(),
            Some(&last),
            "pattern {pattern:?}: every stored column must match the last \
             write, not just content_hash"
        );
    }
}

/// A bootstrap document write is fenced atomically on claim ownership.
/// This models a stale claimant that already passed its early
/// `claim_lost` check, then reaches SQLite only after a successor has taken
/// the lease and written fresher content for the same session.
#[test]
fn test_stale_claimant_cannot_overwrite_successor_document() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("session_search.sqlite");
    let index = SessionSearchIndex::open_or_create(&db_path).unwrap();
    let now = chrono::Utc::now().timestamp();

    assert!(
        index
            .try_claim_bootstrap(now, TEST_TIMING.lease, "stale")
            .unwrap()
    );
    let takeover_at = now + TEST_TIMING.lease.as_secs() as i64 + 1;
    assert!(
        index
            .try_claim_bootstrap(takeover_at, TEST_TIMING.lease, "successor")
            .unwrap(),
        "the successor must first take over the stale claimant's expired lease"
    );
    assert!(
        index
            .upsert_doc_if_claim_owner(&stream_doc("s1", "SUCCESSOR", 1), "successor")
            .unwrap()
    );
    assert!(
        !index
            .upsert_doc_if_claim_owner(&stream_doc("s1", "STALE", 1), "stale")
            .unwrap(),
        "a displaced claimant must be rejected at the write boundary"
    );

    assert_eq!(
        index.get_doc("s1").unwrap(),
        Some(stream_doc("s1", "SUCCESSOR", 1)),
        "the successor's full row must survive a later stale write"
    );
}

/// End-to-end caller binding for #515: takeover happens after `reindex_all`
/// has read stale content and passed its early `claim_lost` check, but before
/// its SQLite write. The successor's row must still win.
#[tokio::test]
#[serial(search_cache_epoch)]
async fn test_reindex_write_is_fenced_against_takeover_after_content_read() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    let storage = JsonlStorageAdapter::with_root(root.clone());
    let info = Info {
        id: acp::SessionId::new("s1"),
        cwd: "/ws".to_string(),
    };
    storage
        .init_session(&info, acp::ModelId::new("test"))
        .await
        .unwrap();

    let stale_token = ClaimToken::new();
    let now = chrono::Utc::now().timestamp();
    let takeover_at = now + TEST_TIMING.lease.as_secs() as i64 + 1;
    let db_path = search_db_path(&root);
    with_search_index(&db_path, |index| {
        index.try_claim_bootstrap(now, TEST_TIMING.lease, stale_token.as_str())
    })
    .unwrap();

    let progress = Arc::new(BootstrapProgress::default());
    let db_for_hook = db_path.clone();
    *progress.before_session_write.lock().expect("hook mutex") = Some(Arc::new(move || {
        with_search_index(&db_for_hook, |index| {
            assert!(index.try_claim_bootstrap(takeover_at, TEST_TIMING.lease, "successor")?);
            assert!(
                index.upsert_doc_if_claim_owner(&stream_doc("s1", "SUCCESSOR", 1), "successor")?
            );
            Ok(())
        })
        .unwrap();
    }));

    reindex_all(
        &root,
        &storage,
        &progress,
        &stale_token,
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .unwrap();

    assert_eq!(
        with_search_index(&db_path, |index| index.get_doc("s1")).unwrap(),
        Some(stream_doc("s1", "SUCCESSOR", 1)),
        "a stale reindex task must not clobber the successor after takeover"
    );
    assert_eq!(progress.indexed.load(Ordering::Relaxed), 0);
    assert_eq!(read_marker(&db_path), None);
}

/// Companion to the single-key exhaustive test: two independent
/// `session_id`s interleaved with each other must not let one key's write
/// order affect the other's outcome -- `session_id` is genuinely the
/// isolation boundary, not merely "usually" the isolation boundary.
#[test]
fn test_upsert_doc_interleaving_does_not_cross_contaminate_other_keys() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("session_search.sqlite");
    let index = SessionSearchIndex::open_or_create(&db_path).unwrap();

    // s1: B's single write lands after A's two -- B should win.
    index.upsert_doc(&stream_doc("s1", "A", 1)).unwrap();
    index.upsert_doc(&stream_doc("s1", "A", 2)).unwrap();
    index.upsert_doc(&stream_doc("s1", "B", 1)).unwrap();
    // s2: the opposite order -- A's single write lands after B's two.
    index.upsert_doc(&stream_doc("s2", "B", 1)).unwrap();
    index.upsert_doc(&stream_doc("s2", "B", 2)).unwrap();
    index.upsert_doc(&stream_doc("s2", "A", 1)).unwrap();

    let mut ids = index.all_indexed_session_ids().unwrap();
    ids.sort();
    assert_eq!(ids, vec!["s1".to_string(), "s2".to_string()]);
    assert_eq!(
        index.get_content_hash("s1").unwrap().as_deref(),
        Some("B-rev1"),
        "s1's write order must decide s1's outcome independently of s2's"
    );
    assert_eq!(
        index.get_content_hash("s2").unwrap().as_deref(),
        Some("A-rev1"),
        "s2's write order must decide s2's outcome independently of s1's"
    );
}

/// Property #2: `reindex_all` checks `claim_lost` once per session, before
/// doing any of that session's (redundant, but wasted) read/upsert work.
/// Driven directly through `reindex_all`'s own `claim_lost` parameter --
/// deterministic, not a timing race — matching #440/#476's move away from
/// deadline-based interleaving toward driving the actual precondition.
#[tokio::test]
async fn test_claim_lost_flag_skips_every_session_write() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    let storage = JsonlStorageAdapter::with_root(root.clone());
    for id in ["s1", "s2", "s3"] {
        let info = Info {
            id: acp::SessionId::new(id),
            cwd: "/ws".to_string(),
        };
        storage
            .init_session(&info, acp::ModelId::new("test"))
            .await
            .unwrap();
    }

    let token = ClaimToken::new();
    let now = chrono::Utc::now().timestamp();
    let db_path = search_db_path(&root);
    with_search_index(&db_path, |index| {
        index.try_claim_bootstrap(now, TEST_TIMING.lease, token.as_str())
    })
    .unwrap();

    let progress = Arc::new(BootstrapProgress::default());
    // Set from the start: the deterministic equivalent of "the refresher
    // already observed the takeover before this claimant's per-session loop
    // began" -- the earliest and strongest form of the race, and the one
    // most likely to be hit if the early-exit check were ever removed.
    let claim_lost = Arc::new(AtomicBool::new(true));
    reindex_all(&root, &storage, &progress, &token, Arc::clone(&claim_lost))
        .await
        .unwrap();

    assert_eq!(
        progress.indexed.load(Ordering::Relaxed),
        0,
        "a displaced claimant must not index any session"
    );
    let indexed_ids = with_search_index(&db_path, |index| index.all_indexed_session_ids()).unwrap();
    assert!(
        indexed_ids.is_empty(),
        "no row must have been written by a displaced claimant, got {indexed_ids:?}"
    );
    assert_eq!(
        read_marker(&db_path),
        None,
        "a displaced claimant must not write the completion marker"
    );
}

/// #498 review: `claim_lost` true from the start cannot tell a per-session
/// check from a single check hoisted to `reindex_all` entry. Flip the flag
/// only after the first session has entered that check, then require later
/// sessions to observe the takeover.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial(search_cache_epoch)]
async fn test_claim_lost_flag_is_checked_again_after_reindex_starts() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    let storage = JsonlStorageAdapter::with_root(root.clone());
    for id in ["s1", "s2", "s3"] {
        let info = Info {
            id: acp::SessionId::new(id),
            cwd: "/ws".to_string(),
        };
        storage
            .init_session(&info, acp::ModelId::new("test"))
            .await
            .unwrap();
    }

    let token = ClaimToken::new();
    let now = chrono::Utc::now().timestamp();
    let db_path = search_db_path(&root);
    with_search_index(&db_path, |index| {
        index.try_claim_bootstrap(now, TEST_TIMING.lease, token.as_str())
    })
    .unwrap();

    let progress = Arc::new(BootstrapProgress::default());
    let claim_lost = Arc::new(AtomicBool::new(false));
    let first = Arc::new(AtomicBool::new(false));
    let started = Arc::new(tokio::sync::Notify::new());
    let released = Arc::new(AtomicBool::new(false));
    let lost_for_hook = Arc::clone(&claim_lost);
    let first_for_hook = Arc::clone(&first);
    let started_for_hook = Arc::clone(&started);
    let released_for_hook = Arc::clone(&released);
    *progress.session_claim_check.lock().expect("hook mutex") = Some(Arc::new(move || {
        if !first_for_hook.swap(true, Ordering::SeqCst) {
            started_for_hook.notify_one();
            return false;
        }
        while !released_for_hook.load(Ordering::Acquire) {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        lost_for_hook.load(Ordering::Acquire)
    }));

    let progress_task = Arc::clone(&progress);
    let lost_task = Arc::clone(&claim_lost);
    let root_task = root.clone();
    let join = tokio::spawn(async move {
        reindex_all(&root_task, &storage, &progress_task, &token, lost_task).await
    });

    tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
        .await
        .expect("per-session claim_lost check never ran");
    claim_lost.store(true, Ordering::Release);
    released.store(true, Ordering::Release);

    join.await.unwrap().unwrap();
    assert!(
        progress.indexed.load(Ordering::Relaxed) < 3,
        "a takeover after the first per-session check must stop later sessions, \
         got indexed={}",
        progress.indexed.load(Ordering::Relaxed)
    );
}

/// Control for the test above: with `claim_lost` false, the *same* sessions
/// must actually get indexed and the marker written. Without this pairing,
/// the previous test would pass just as well against a `reindex_all` that
/// never indexes anything regardless of `claim_lost` -- proving nothing
/// about the flag specifically.
#[tokio::test]
#[serial(search_cache_epoch)]
async fn test_claim_lost_false_control_indexes_normally() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    let storage = JsonlStorageAdapter::with_root(root.clone());
    for id in ["s1", "s2", "s3"] {
        let info = Info {
            id: acp::SessionId::new(id),
            cwd: "/ws".to_string(),
        };
        storage
            .init_session(&info, acp::ModelId::new("test"))
            .await
            .unwrap();
    }

    let token = ClaimToken::new();
    let now = chrono::Utc::now().timestamp();
    let db_path = search_db_path(&root);
    with_search_index(&db_path, |index| {
        index.try_claim_bootstrap(now, TEST_TIMING.lease, token.as_str())
    })
    .unwrap();

    let progress = Arc::new(BootstrapProgress::default());
    let claim_lost = Arc::new(AtomicBool::new(false));
    reindex_all(&root, &storage, &progress, &token, Arc::clone(&claim_lost))
        .await
        .unwrap();

    assert_eq!(progress.indexed.load(Ordering::Relaxed), 3);
    let mut indexed_ids =
        with_search_index(&db_path, |index| index.all_indexed_session_ids()).unwrap();
    indexed_ids.sort();
    assert_eq!(
        indexed_ids,
        vec!["s1".to_string(), "s2".to_string(), "s3".to_string()]
    );
    assert!(read_marker(&db_path).is_some());
}

/// Property #3: the completion marker and the orphan prune are fenced on
/// claim ownership -- both refuse to write once the claim row names a
/// different token, and both succeed for whoever the claim row actually
/// names. Exercised directly against the SQL fencing (`set_meta_if_claim_owner`,
/// `prune_missing_if_claim_owner`), which is the exact boundary the safety
/// argument depends on, with a positive control on each half so neither
/// assertion could pass against a function that just always returns
/// `false`/never prunes.
#[test]
fn test_marker_and_prune_writes_are_fenced_on_claim_ownership() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("session_search.sqlite");
    let index = SessionSearchIndex::open_or_create(&db_path).unwrap();
    let now = chrono::Utc::now().timestamp();

    // The successor holds the claim now; "stale" is the displaced claimant
    // that has not yet noticed (the exact race #477 is about).
    index
        .try_claim_bootstrap(now, TEST_TIMING.lease, "successor")
        .unwrap();

    let wrote = index
        .set_meta_if_claim_owner(META_KEY_LAST_BOOTSTRAP, "999", "stale")
        .unwrap();
    assert!(
        !wrote,
        "a stale claimant must not be able to write the completion marker"
    );
    assert_eq!(index.get_meta(META_KEY_LAST_BOOTSTRAP).unwrap(), None);

    // Positive control: the actual owner's write succeeds.
    let wrote = index
        .set_meta_if_claim_owner(META_KEY_LAST_BOOTSTRAP, "999", "successor")
        .unwrap();
    assert!(wrote);
    assert_eq!(
        index.get_meta(META_KEY_LAST_BOOTSTRAP).unwrap(),
        Some("999".to_string())
    );

    // A row the successor already indexed since the stale claimant's
    // startup snapshot -- from the stale claimant's point of view this
    // session_id is an orphan (not in its `expected_ids`) and it must not
    // be allowed to delete it.
    index
        .upsert_doc(&stream_doc("keep-me", "successor", 1))
        .unwrap();
    let empty_keep: HashSet<String> = HashSet::new();

    let pruned = index
        .prune_missing_if_claim_owner(now, "stale", &empty_keep)
        .unwrap();
    assert!(!pruned, "a stale claimant must not be able to prune");
    assert_eq!(
        index.all_indexed_session_ids().unwrap(),
        vec!["keep-me".to_string()],
        "the successor's row must survive a stale claimant's prune attempt"
    );

    // Positive control: the actual owner's prune succeeds.
    let pruned = index
        .prune_missing_if_claim_owner(now, "successor", &empty_keep)
        .unwrap();
    assert!(pruned);
    assert!(index.all_indexed_session_ids().unwrap().is_empty());
}
