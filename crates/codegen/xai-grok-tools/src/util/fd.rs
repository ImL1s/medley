//! Runtime resolution for the PI `glob` backend's optional vendored `fd`.

use std::path::PathBuf;
use std::sync::OnceLock;

const FD_PATH_OVERRIDE_ENV: &str = "GROK_TOOLS_FD_PATH";

#[cfg(bundle_fd)]
const FD_BYTES: &[u8] = include_bytes!(concat!(
    env!("OUT_DIR"),
    "/bundle-fd/fd-",
    env!("GROK_TOOLS_FD_VER"),
    "-",
    env!("GROK_TOOLS_FD_TARGET"),
    ".bin"
));

#[cfg(bundle_fd)]
fn extract_bundled_fd() -> std::io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let versioned_name = concat!(
        "fd-",
        env!("GROK_TOOLS_FD_VER"),
        "-",
        env!("GROK_TOOLS_FD_TARGET")
    );
    let dir = crate::util::grok_home().join("vendor");
    let dest = dir.join(versioned_name);
    if !dest.exists() {
        std::fs::create_dir_all(&dir)?;

        // Write via a temp file then atomically rename, so concurrent first-use
        // attempts cannot leave a partially written binary behind.
        let tmp = dir.join(format!(
            "{versioned_name}.tmp.{}.{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::write(&tmp, FD_BYTES)?;
        let mut perms = std::fs::metadata(&tmp)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&tmp, perms)?;
        if let Err(e) = std::fs::rename(&tmp, &dest) {
            let _ = std::fs::remove_file(&tmp);
            if !dest.exists() {
                return Err(e);
            }
        }
    }
    Ok(dest)
}

fn resolve_env_override() -> Option<PathBuf> {
    std::env::var_os(FD_PATH_OVERRIDE_ENV)
        .map(PathBuf::from)
        .filter(|p| p.is_file())
}

fn resolve_fd_from(
    env_path: Option<PathBuf>,
    bundled: Option<PathBuf>,
    vendor_path: PathBuf,
    which_path: Option<PathBuf>,
) -> PathBuf {
    if let Some(path) = env_path
        && path.is_file()
    {
        return path;
    }
    if let Some(path) = bundled {
        return path;
    }
    if vendor_path.is_file() {
        return vendor_path;
    }
    which_path.unwrap_or_else(|| PathBuf::from("fd"))
}

fn cached_fd_path() -> PathBuf {
    static FD_EXEC: OnceLock<PathBuf> = OnceLock::new();
    FD_EXEC
        .get_or_init(|| {
            let bundled = {
                #[cfg(bundle_fd)]
                {
                    extract_bundled_fd().ok()
                }
                #[cfg(not(bundle_fd))]
                {
                    None
                }
            };
            resolve_fd_from(
                None,
                bundled,
                crate::util::grok_home().join("vendor").join("fd"),
                which::which("fd").ok(),
            )
        })
        .clone()
}

/// Resolve the `fd` executable used by the PI glob backend.
///
/// Order:
/// 1) runtime override (`GROK_TOOLS_FD_PATH`) if it points at a regular file;
/// 2) bundled binary extracted to `~/.grok/vendor/fd-<ver>-<target>` (when built with `bundle_fd`);
/// 3) `~/.grok/vendor/fd` if present;
/// 4) `which fd`, else literal `"fd"`.
pub fn fd_path() -> PathBuf {
    resolve_env_override().unwrap_or_else(cached_fd_path)
}

#[cfg(test)]
mod tests {
    #[cfg(all(bundle_fd, unix))]
    use super::*;

    #[cfg(all(bundle_fd, unix))]
    #[test]
    fn bundled_fd_override_artifact_extracts_exact_and_executes() {
        assert_eq!(
            env!("GROK_TOOLS_FD_TARGET"),
            "override",
            "run this test with GROK_TOOLS_BUNDLE_FD_PATH set"
        );
        let path = fd_path();
        let extracted = std::fs::read(&path).expect("read extracted override fd");
        assert_eq!(
            extracted.as_slice(),
            FD_BYTES,
            "extracted override payload must match embedded bytes"
        );

        let out = std::process::Command::new(&path)
            .arg("--version")
            .output()
            .expect("extracted override fd should start");
        assert!(
            out.status.success(),
            "override artifact should execute successfully:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[cfg(all(bundle_fd, unix))]
    #[test]
    fn bundled_fd_release_artifact_extracts_exact_and_executes() {
        assert_ne!(
            env!("GROK_TOOLS_FD_TARGET"),
            "override",
            "run this test in a release build without GROK_TOOLS_BUNDLE_FD_PATH"
        );
        let path = fd_path();
        let extracted = std::fs::read(&path).expect("read extracted release fd");
        assert_eq!(
            extracted.as_slice(),
            FD_BYTES,
            "extracted release payload must match embedded bytes"
        );

        let out = std::process::Command::new(&path)
            .arg("--version")
            .output()
            .expect("extracted release fd should start");
        assert!(
            out.status.success(),
            "release artifact should execute successfully:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            String::from_utf8_lossy(&out.stdout)
                .to_ascii_lowercase()
                .contains("fd"),
            "release artifact should identify as fd: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
}
