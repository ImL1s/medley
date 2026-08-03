use anyhow::{Context, bail};
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn check_protoc_good(protoc: &Path) -> anyhow::Result<()> {
    let output = Command::new(protoc)
        .arg("--version")
        .output()
        .context("Failed to execute protoc")?;

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "protoc --version failed, likely dotslash is missing; \
             try `cargo install dotslash`; stdout: {stdout:?}, stderr: {stderr:?}"
        );
    }
    Ok(())
}

fn is_github_actions() -> bool {
    env::var_os("GITHUB_ACTIONS").is_some()
}

/// First `protoc` on `PATH`, as an absolute path.
///
/// `check_protoc_good` proves a `protoc` is *runnable*, but running it does not
/// say where it lives, and the caller needs a real path to hand to
/// `cargo:rerun-if-changed`. Returns `None` when `PATH` is unset or nothing in
/// it matches, leaving the caller to decide what to do.
fn protoc_on_path() -> Option<PathBuf> {
    protoc_in_dirs(env::split_paths(&env::var_os("PATH")?))
}

/// Pure half of [`protoc_on_path`]. Split out because `PATH` is process-global
/// state: a test that set it would race every other test in the binary.
fn protoc_in_dirs(dirs: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    dirs.into_iter()
        .map(|dir| dir.join("protoc"))
        // `try_exists` rather than `exists`: a broken symlink is not something
        // to hand Cargo as a fingerprint input.
        .find(|candidate| candidate.try_exists().unwrap_or(false))
}

/// Find `protoc` command.
///
/// Search order:
/// 1. `$PROTOC` environment variable (set by Bazel `build_script_env` or user override)
/// 2. `bin/protoc` walking up parent directories (dotslash wrapper for local dev)
/// 3. `protoc` on `$PATH` (system install or other tooling)
///
/// When `bin/protoc` exists but fails to execute (e.g. the dotslash wrapper running
/// in Bazel remote execution where `dotslash` is not installed), the error is not fatal —
/// we fall through to the PATH-based lookup instead.
///
/// Every branch returns a path that **resolves from the package root**, because
/// callers hand the result to `cargo:rerun-if-changed`. See the comment on
/// branch 3 for what a bare name costs.
///
/// Returns `Ok(None)` if not found and not in a strict environment (GitHub Actions).
pub fn find_protoc() -> anyhow::Result<Option<PathBuf>> {
    // 1. Check the PROTOC env var first. This is the standard override used by prost-build
    //    and is set by Bazel cargo_build_script build_script_env to point at a hermetic
    //    protoc binary instead of the dotslash wrapper.
    if let Ok(protoc_env) = env::var("PROTOC") {
        let protoc = PathBuf::from(&protoc_env);
        if protoc.try_exists()? {
            check_protoc_good(&protoc)?;
            return Ok(Some(protoc));
        }
    }

    // 2. Walk up directories looking for bin/protoc (dotslash wrapper).
    let cwd = env::current_dir()?;
    let mut dir = cwd.clone();
    let mut dir_rel = PathBuf::new();
    loop {
        // Return relative path to make build more deterministic.
        let protoc = dir_rel.join("bin/protoc");
        if protoc.try_exists()? {
            match check_protoc_good(&protoc) {
                Ok(()) => return Ok(Some(protoc)),
                Err(e) => {
                    // bin/protoc exists but can't execute — likely the dotslash wrapper
                    // in an environment without dotslash (e.g. Bazel remote execution).
                    // Fall through to PATH-based lookup below.
                    eprintln!(
                        "bin/protoc found at `{}` but failed to execute: {e:#}; \
                         trying protoc from PATH as fallback",
                        protoc.display()
                    );
                    break;
                }
            }
        }
        if !dir.pop() {
            break;
        }
        dir_rel.push("..");
    }

    // 3. Try protoc from PATH (system install or other tooling).
    //
    // Resolved to the absolute path it came from, not returned as the bare
    // name. Callers emit `cargo:rerun-if-changed` for whatever this returns,
    // and Cargo resolves that relative to the *package* root — where no file
    // called `protoc` exists. Cargo treats a missing rerun-if-changed path as
    // permanently dirty ("Dirty …: the file `protoc` is missing"), so the bare
    // name made this crate's build script, and every crate downstream of it,
    // recompile on every single cargo invocation.
    if check_protoc_good(Path::new("protoc")).is_ok() {
        return Ok(Some(
            protoc_on_path().unwrap_or_else(|| PathBuf::from("protoc")),
        ));
    }

    // 4. Not found anywhere.
    if is_github_actions() {
        return Err(anyhow::anyhow!(
            "`protoc` not found (checked $PROTOC env, bin/protoc, and PATH)"
        ));
    }
    eprintln!("`protoc` not found; likely it is missing in docker image");
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this exists to prevent, in one sentence: a `protoc` path that
    /// does not resolve becomes `cargo:rerun-if-changed=<missing>`, Cargo calls
    /// that permanently dirty, and every crate downstream of the build script
    /// recompiles on every cargo invocation. In this repository that turned a
    /// ~30-minute CI job into ~122 minutes, of which 1.2 seconds was actually
    /// running tests.
    ///
    /// So what matters is not merely *finding* protoc — it is returning
    /// something Cargo can still find afterwards, from the package root.
    #[test]
    fn path_lookup_returns_a_resolvable_path_not_a_bare_name() {
        let dir = std::env::temp_dir().join(format!(
            "xai-proto-build-find-protoc-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let protoc = dir.join("protoc");
        std::fs::write(&protoc, b"#!/bin/sh\nexit 0\n").expect("write stub");

        let found = protoc_in_dirs([dir.clone()]).expect("stub is on the search path");

        assert_eq!(found, protoc);
        assert!(
            found.try_exists().unwrap_or(false),
            "a path that does not resolve is what caused the rebuild loop: {found:?}"
        );
        assert_ne!(
            found,
            PathBuf::from("protoc"),
            "returning the bare name is the bug; Cargo resolves it against the \
             package root, where no such file exists"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Missing directories are skipped rather than returned. `PATH` routinely
    /// contains entries that do not exist, and handing one of those to
    /// `rerun-if-changed` would reintroduce the same permanent-dirty state.
    #[test]
    fn absent_candidates_are_skipped() {
        assert_eq!(
            protoc_in_dirs([PathBuf::from("/nonexistent-aXbYcZ/bin")]),
            None
        );
    }
}
