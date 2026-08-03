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
    protoc_in_dirs(
        env::split_paths(&env::var_os("PATH")?),
        env::current_dir().ok().as_deref(),
    )
}

/// Pure half of [`protoc_on_path`]. Split out because `PATH` is process-global
/// state: a test that set it would race every other test in the binary. `cwd`
/// is passed in for the same reason.
///
/// Candidates are accepted only if they actually **run**, not merely exist.
/// The OS skips unusable entries during its own `PATH` lookup, so a directory
/// or a non-executable file named `protoc` early on `PATH` is invisible to
/// `check_protoc_good("protoc")` — but picking it here would hand that path to
/// `protoc_executable` and fail the build with `PermissionDenied`, having
/// resolved a `protoc` the caller had already proven works.
///
/// An empty `PATH` component means the current directory (POSIX), and joining
/// one naively yields the bare name `protoc` — the very value this function
/// exists to avoid returning. It would also defeat the runnability check
/// above, because `Command::new` performs its own `PATH` lookup for a name
/// with no separator: an unusable `protoc` in the working directory would be
/// validated by a *different*, working one further down `PATH`, and the bare
/// name handed back regardless. With no working directory to resolve against
/// there is no path to return, so the component is skipped.
fn protoc_in_dirs(dirs: impl IntoIterator<Item = PathBuf>, cwd: Option<&Path>) -> Option<PathBuf> {
    dirs.into_iter()
        .filter_map(|dir| {
            if dir.as_os_str().is_empty() {
                cwd.map(Path::to_path_buf)
            } else {
                Some(dir)
            }
        })
        .map(|dir| dir.join("protoc"))
        .find(|candidate| {
            // `try_exists` first: cheap, and skips the subprocess for the
            // majority of PATH entries that hold no protoc at all.
            candidate.try_exists().unwrap_or(false) && check_protoc_good(candidate).is_ok()
        })
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
    /// A stub `protoc` that runs and exits zero, so `check_protoc_good`
    /// accepts it. Returns the directory holding it.
    #[cfg(unix)]
    fn stub_protoc_dir(tag: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "xai-proto-build-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let protoc = dir.join("protoc");
        std::fs::write(&protoc, b"#!/bin/sh\nexit 0\n").expect("write stub");
        std::fs::set_permissions(&protoc, std::fs::Permissions::from_mode(0o755))
            .expect("chmod stub");
        dir
    }

    #[cfg(unix)]
    #[test]
    fn path_lookup_returns_a_resolvable_path_not_a_bare_name() {
        let dir = stub_protoc_dir("resolvable");
        let protoc = dir.join("protoc");

        let found = protoc_in_dirs([dir.clone()], None).expect("stub is on the search path");

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
            protoc_in_dirs([PathBuf::from("/nonexistent-aXbYcZ/bin")], None),
            None
        );
    }

    /// Existing is not the same as usable, and the difference is invisible to
    /// the caller: the OS skips unusable entries during its own `PATH` lookup,
    /// so `check_protoc_good("protoc")` succeeds on the *later* directory
    /// while a naive scan would return the earlier one. Handing that back
    /// fails the build with `PermissionDenied` after protoc had been proven to
    /// work — a worse outcome than the bare-name bug this function replaced.
    #[cfg(unix)]
    #[test]
    fn unrunnable_candidates_are_skipped_in_favour_of_a_working_one() {
        let good = stub_protoc_dir("runnable");

        // A non-executable file named protoc, earlier on the search path.
        let shadow = std::env::temp_dir().join(format!(
            "xai-proto-build-shadow-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&shadow).expect("temp dir");
        std::fs::write(shadow.join("protoc"), b"not executable\n").expect("write shadow");

        assert_eq!(
            protoc_in_dirs([shadow.clone(), good.clone()], None),
            Some(good.join("protoc")),
            "a protoc that cannot run must not shadow one that can"
        );

        // And a *directory* named protoc, which also exists but cannot run.
        let dir_shadow = std::env::temp_dir().join(format!(
            "xai-proto-build-dirshadow-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(dir_shadow.join("protoc")).expect("temp dir");

        assert_eq!(
            protoc_in_dirs([dir_shadow.clone(), good.clone()], None),
            Some(good.join("protoc")),
            "a directory named protoc must not shadow a real one"
        );

        for d in [good, shadow, dir_shadow] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    /// `PATH=/usr/bin:` has three components, and the third is empty. POSIX
    /// reads that as the current directory; `PathBuf::join` reads it as
    /// nothing at all and produces the bare `protoc` — reintroducing exactly
    /// the unresolvable value this module exists to eliminate, by a route the
    /// runnability check cannot catch, since `Command::new("protoc")` would
    /// happily validate it against some other directory on `PATH`.
    #[cfg(unix)]
    #[test]
    fn an_empty_path_entry_resolves_against_the_working_directory() {
        let cwd = stub_protoc_dir("emptyentry");

        let found =
            protoc_in_dirs([PathBuf::new()], Some(&cwd)).expect("the working directory has one");

        assert_eq!(found, cwd.join("protoc"));
        assert_ne!(
            found,
            PathBuf::from("protoc"),
            "an empty component must name the working directory, not collapse \
             to the bare name"
        );

        std::fs::remove_dir_all(&cwd).ok();
    }

    /// With no working directory there is nothing for an empty component to
    /// mean, and the bare name is not an acceptable substitute. A contract
    /// assertion rather than a regression catch: the pre-fix code also
    /// returned `None` here, because `protoc` does not resolve from the
    /// package root — which is the whole problem.
    #[test]
    fn an_empty_path_entry_without_a_working_directory_is_skipped() {
        assert_eq!(protoc_in_dirs([PathBuf::new()], None), None);
    }
}
