//! Process-level check that a fresh home lands in `~/.medley`.
//!
//! `grok_home()` memoizes for the life of the process, so each of these
//! `state_dir_*` files is its own test binary — one home, one resolution.

#[test]
fn fresh_home_resolves_and_creates_the_medley_dir() {
    let home = tempfile::TempDir::new().expect("temp home");
    // Safety: single-threaded test binary, set before any other thread exists.
    unsafe { std::env::set_var("HOME", home.path()) };
    unsafe { std::env::remove_var("MEDLEY_HOME") };
    unsafe { std::env::remove_var("GROK_HOME") };

    let resolved = xai_grok_config::grok_home();

    let expected = home.path().join(xai_grok_config::state_dir::STATE_DIR_NAME);
    assert_eq!(
        dunce::canonicalize(&resolved).unwrap(),
        dunce::canonicalize(&expected).unwrap(),
    );
    assert!(resolved.is_dir(), "grok_home() must create the directory");
    assert!(
        !home
            .path()
            .join(xai_grok_config::state_dir::LEGACY_STATE_DIR_NAME)
            .exists(),
        "a fresh install must not create the legacy directory"
    );
}
