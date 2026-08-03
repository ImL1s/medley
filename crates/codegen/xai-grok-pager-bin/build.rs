use std::path::PathBuf;
use std::process::Command;

/// Run `git` and return its trimmed stdout, or `None` if git is absent, the
/// command failed, or the output was empty.
fn git_output(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// Resolve a path inside the git directory, and return it only if it exists.
///
/// The existence check is the whole point. Cargo resolves
/// `cargo:rerun-if-changed` against the **package** root and treats a missing
/// entry as permanently dirty — it says so outright: "Dirty <crate>: the file
/// `…` is missing". So an unresolvable path does the opposite of what the
/// directive is for: instead of rebuilding when git moves, it rebuilds always.
///
/// `git rev-parse --git-path` is asked rather than walking up to `.git` by
/// hand, because `.git` is a *file* in a linked worktree and in a submodule,
/// and the real directory can be anywhere. Its answer is relative to the
/// working directory, which for a build script is the package root — exactly
/// what Cargo will resolve it against.
fn git_path(name: &str) -> Option<PathBuf> {
    let path = PathBuf::from(git_output(&["rev-parse", "--git-path", name])?);
    path.try_exists().unwrap_or(false).then_some(path)
}

/// Files whose change can mean `HEAD` now names a different commit.
///
/// `HEAD` alone is not enough, and that is the subtle half: committing on the
/// current branch does not touch `HEAD`, whose contents stay
/// `ref: refs/heads/<branch>`. The commit id moves in the branch ref, or in
/// `packed-refs` when the ref has been packed away and has no file of its own.
///
/// An empty result is correct and safe — a source tree with no git at all
/// reports `unknown` for the commit no matter what, so there is nothing to
/// watch.
fn commit_witnesses() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    paths.extend(git_path("HEAD"));
    // Fails on a detached HEAD, where HEAD holds the commit id directly and is
    // therefore already covered by the line above.
    if let Some(branch) = git_output(&["symbolic-ref", "-q", "HEAD"]) {
        paths.extend(git_path(&branch));
    }
    paths.extend(git_path("packed-refs"));
    paths
}

fn main() {
    for path in commit_witnesses() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    println!("cargo:rerun-if-env-changed=GROK_VERSION");

    let commit = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let version = std::env::var("GROK_VERSION")
        .or_else(|_| std::env::var("CARGO_PKG_VERSION"))
        .unwrap_or_else(|_| "0.0.0".to_string());

    println!(
        "cargo:rustc-env=VERSION_WITH_COMMIT={} ({})",
        version, commit
    );
}
