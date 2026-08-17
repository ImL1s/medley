//! Process-level check that an existing `~/.grok` keeps being used, so a
//! headless run never silently starts a fresh state directory.

#[test]
fn existing_legacy_dir_stays_live_and_is_flagged_for_migration() {
    let home = tempfile::TempDir::new().expect("temp home");
    let legacy = home
        .path()
        .join(xai_grok_config::state_dir::LEGACY_STATE_DIR_NAME);
    std::fs::create_dir_all(&legacy).expect("create legacy dir");
    std::fs::write(legacy.join("config.toml"), b"x = 1").expect("seed config");
    // Safety: single-threaded test binary, set before any other thread exists.
    unsafe { std::env::set_var("HOME", home.path()) };
    #[cfg(windows)]
    unsafe {
        std::env::set_var("USERPROFILE", home.path())
    };
    unsafe { std::env::remove_var("MEDLEY_HOME") };
    unsafe { std::env::remove_var("GROK_HOME") };

    let pending = xai_grok_config::state_dir::pending_migration().expect("migration is offerable");
    assert_eq!(
        dunce::canonicalize(&pending.legacy).unwrap(),
        dunce::canonicalize(&legacy).unwrap(),
    );
    assert!(
        pending
            .target
            .ends_with(xai_grok_config::state_dir::STATE_DIR_NAME)
    );

    let resolved = xai_grok_config::grok_home();
    assert_eq!(
        dunce::canonicalize(&resolved).unwrap(),
        dunce::canonicalize(&legacy).unwrap(),
        "an un-migrated install must keep reading its existing state"
    );
    assert!(
        !pending.target.exists(),
        "the medley dir must not appear without an explicit migration"
    );
}
