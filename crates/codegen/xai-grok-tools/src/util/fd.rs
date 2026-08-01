//! Runtime resolution for the optional vendored `fd` executable.

use std::path::PathBuf;
use std::sync::OnceLock;

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
fn resolve_bundled_fd() -> std::io::Result<PathBuf> {
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    let path = crate::util::grok_home().join("vendor").join(concat!(
        "fd-",
        env!("GROK_TOOLS_FD_VER"),
        "-",
        env!("GROK_TOOLS_FD_TARGET")
    ));
    if !path.exists() {
        fs::create_dir_all(path.parent().expect("fd vendor path has a parent"))?;
        fs::write(&path, FD_BYTES)?;
        #[cfg(unix)]
        {
            let mut permissions = fs::metadata(&path)?.permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&path, permissions)?;
        }
    }
    Ok(path)
}

/// Get the path to the `fd` executable.
///
/// Release builds with the `pi` feature extract the bundled binary under
/// `~/.grok/vendor/`. Debug builds, unsupported targets, and builds without a
/// bundle override resolve `fd` through `PATH` instead.
pub fn fd_path() -> PathBuf {
    static FD_EXEC: OnceLock<PathBuf> = OnceLock::new();
    FD_EXEC
        .get_or_init(|| {
            #[cfg(bundle_fd)]
            {
                resolve_bundled_fd().unwrap_or_else(|_| PathBuf::from("fd"))
            }
            #[cfg(not(bundle_fd))]
            {
                PathBuf::from("fd")
            }
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(bundle_fd)]
    #[test]
    fn bundled_fd_is_extracted_and_executable() {
        let path = fd_path();
        assert!(path.starts_with(crate::util::grok_home().join("vendor")));
        assert!(path.is_file(), "missing extracted fd: {}", path.display());

        let status = std::process::Command::new(&path)
            .arg("--version")
            .status()
            .expect("extracted fd should start");
        assert!(status.success(), "extracted fd should execute successfully");
    }

    #[cfg(not(bundle_fd))]
    #[test]
    fn unbundled_fd_uses_path_lookup() {
        assert_eq!(fd_path(), PathBuf::from("fd"));
    }
}
