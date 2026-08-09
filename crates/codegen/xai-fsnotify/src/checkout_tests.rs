use super::*;
use std::fs;
use tempfile::TempDir;

/// A repository declaring `deps/lib` as a submodule, beside checkouts it does not.
fn workspace() -> TempDir {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    git2::Repository::init(root).unwrap();
    fs::write(
        root.join(".gitmodules"),
        "[submodule \"deps/lib\"]\n\tpath = deps/lib\n\turl = ../lib.git\n",
    )
    .unwrap();

    fs::create_dir_all(root.join("deps/lib/src")).unwrap();
    fs::write(
        root.join("deps/lib/.git"),
        "gitdir: ../../.git/modules/lib\n",
    )
    .unwrap();

    fs::create_dir_all(root.join("vendor/upstream/.git")).unwrap();

    let worktree = root.join(".harness/worktrees/feature");
    fs::create_dir_all(worktree.join("src")).unwrap();
    fs::write(
        worktree.join(".git"),
        "gitdir: /elsewhere/.git/worktrees/x\n",
    )
    .unwrap();

    fs::create_dir_all(root.join("sl-repo/.sl")).unwrap();
    fs::create_dir_all(root.join("crates/core")).unwrap();
    temp
}

#[test]
fn only_an_undeclared_checkout_is_another_workspace() {
    let temp = workspace();
    let root = temp.path();

    assert!(!is_another_workspace(&root.join("deps/lib")));
    assert!(is_another_workspace(&root.join("vendor/upstream")));
    assert!(is_another_workspace(
        &root.join(".harness/worktrees/feature")
    ));
    assert!(is_another_workspace(&root.join("sl-repo")));
    assert!(!is_another_workspace(&root.join("crates/core")));
}

/// The prefix test in `is_declared_submodule` must resolve both sides.
///
/// On macOS the bug reproduces with no help at all, because `/var` links to
/// `private/var` and so every temp dir is already symlinked — which is why
/// `only_an_undeclared_checkout_is_another_workspace` fails there and passes
/// on Linux. This builds the symlink explicitly so the mechanism is covered
/// on every platform, including the ubuntu runner that is the only thing CI
/// executes.
#[cfg(unix)]
#[test]
fn a_declared_submodule_reached_through_a_symlink_stays_declared() {
    let temp = workspace();
    let elsewhere = TempDir::new().unwrap();
    let link = elsewhere.path().join("link-to-root");
    std::os::unix::fs::symlink(temp.path(), &link).unwrap();

    assert!(
        !is_another_workspace(&link.join("deps/lib")),
        "a submodule declared in .gitmodules is still declared when the path \
         reaching it goes through a symlink; misreading it as a foreign \
         checkout ends watch coverage over the submodule"
    );
    // The same resolution must not start swallowing real foreign checkouts.
    assert!(is_another_workspace(&link.join("vendor/upstream")));
    assert!(is_another_workspace(&link.join("sl-repo")));
}

/// Per-dir decides at every level, so a worktree anywhere ends coverage.
/// Fan-out watches each top-level child recursively, so only a checkout that
/// is itself a top-level child does.
#[test]
fn coverage_follows_the_watch_strategy() {
    let temp = workspace();
    let root = temp.path();
    let nested = root.join(".harness/worktrees/feature/src");
    let top_level = root.join("vendored");
    fs::create_dir_all(top_level.join(".git")).unwrap();

    let per_dir = |path: &Path| watch_root_covers_with(WatchStrategy::PerDir, root, path);
    assert!(per_dir(root));
    assert!(per_dir(&root.join("deps/lib/src")));
    assert!(!per_dir(&nested));

    let fanout = |path: &Path| watch_root_covers_with(WatchStrategy::Fanout, root, path);
    assert!(fanout(&nested));
    assert!(!fanout(&top_level.join("src")));
}
