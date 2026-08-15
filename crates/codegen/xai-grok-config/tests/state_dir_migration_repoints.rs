//! Process-level check of the migration the interactive prompt performs: copy,
//! then pin, so the rest of the process uses `~/.medley` and never writes back
//! into the directory shared with Grok Build.

#[test]
fn migrating_then_pinning_makes_the_medley_dir_live_for_this_process() {
    let home = tempfile::TempDir::new().expect("temp home");
    let legacy = home
        .path()
        .join(xai_grok_config::state_dir::LEGACY_STATE_DIR_NAME);
    std::fs::create_dir_all(legacy.join("sessions")).expect("create legacy dir");
    std::fs::write(legacy.join("auth.json"), b"{}").expect("seed auth");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(
            legacy.join("auth.json"),
            std::fs::Permissions::from_mode(0o600),
        )
        .expect("chmod auth");
    }
    // Safety: single-threaded test binary, set before any other thread exists.
    unsafe { std::env::set_var("HOME", home.path()) };
    #[cfg(windows)]
    unsafe { std::env::set_var("USERPROFILE", home.path()) };
    unsafe { std::env::remove_var("MEDLEY_HOME") };
    unsafe { std::env::remove_var("GROK_HOME") };

    let pending = xai_grok_config::state_dir::pending_migration().expect("migration is offerable");
    let stats = xai_grok_config::state_dir::migrate_copy(&pending.legacy, &pending.target)
        .expect("copy succeeds");
    assert_eq!(stats.files, 1);

    xai_grok_config::pin_grok_home(pending.target.clone()).expect("nothing resolved the dir yet");
    assert_eq!(xai_grok_config::grok_home(), pending.target);

    // The copy is not a move: the legacy tree is left intact for the official
    // install that owns it.
    assert!(legacy.join("auth.json").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(pending.target.join("auth.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "auth.json must stay owner-only");
    }

    // A second startup resolves to the medley dir on its own, with nothing left
    // to migrate.
    assert!(xai_grok_config::state_dir::pending_migration().is_none());
    assert_eq!(
        xai_grok_config::state_dir::resolve().source,
        xai_grok_config::state_dir::StateDirSource::Medley
    );
}
