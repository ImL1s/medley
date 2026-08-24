//! Real, owner-only `sessions/<encoded-cwd>` parents for fork/import publication.
//!
//! Lexical `starts_with` / `create_dir_all` follow symlink and reparse parents.
//! This walk opens each existing component without following links, creates
//! missing components owner-only, and revalidates identity before mutation.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};

use super::anchored::AnchoredDirectory;

/// Retained handles for `root/sessions/<encoded-cwd>`.
pub struct PublicationParent {
    root_dir: PathBuf,
    sessions: AnchoredDirectory,
    parent: AnchoredDirectory,
    parent_name: OsString,
    parent_path: PathBuf,
}

impl PublicationParent {
    pub fn path(&self) -> &Path {
        &self.parent_path
    }

    pub fn sessions_anchor(&self) -> &AnchoredDirectory {
        &self.sessions
    }

    pub fn parent_anchor(&self) -> &AnchoredDirectory {
        &self.parent
    }

    /// Re-open the sessions root and encoded-CWD parent without following
    /// links, then confirm they still match the retained identities.
    pub fn revalidate(&self) -> io::Result<()> {
        let root = AnchoredDirectory::open_root(&self.root_dir)?;
        let sessions = root.open_child_dir(OsStr::new("sessions"))?;
        sessions.ensure_owner_only()?;
        if !self.sessions.same_identity(&sessions)? {
            return Err(uncertain("sessions root identity changed"));
        }
        let parent = sessions.open_child_dir(&self.parent_name)?;
        parent.ensure_owner_only()?;
        if !self.parent.same_identity(&parent)? {
            return Err(uncertain("publication parent identity changed"));
        }
        Ok(())
    }

    /// Open or create the session-id child as a real owner-only directory.
    pub fn ensure_session_dir(&self, session_id: &OsStr) -> io::Result<AnchoredDirectory> {
        open_or_create_owner_only_child(&self.parent, session_id)
    }
}

/// Validate `root/sessions` and every existing `encoded_cwd` component without
/// following symlinks or reparse points. Create missing components owner-only,
/// then revalidate them. Fail closed on owner, type, or reparse uncertainty.
pub fn ensure_publication_parent(
    root_dir: &Path,
    encoded_cwd: &OsStr,
) -> io::Result<PublicationParent> {
    validate_single_component(encoded_cwd)?;
    let root = AnchoredDirectory::open_root(root_dir).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("publication root is not a real directory: {error}"),
        )
    })?;
    let sessions = open_or_create_owner_only_child(&root, OsStr::new("sessions"))?;
    let parent = open_or_create_owner_only_child(&sessions, encoded_cwd)?;

    // Re-open after any create so a raced replacement cannot stay hidden.
    let sessions_again = root.open_child_dir(OsStr::new("sessions"))?;
    sessions_again.ensure_owner_only()?;
    if !sessions.same_identity(&sessions_again)? {
        return Err(uncertain("sessions root identity changed after create"));
    }
    let parent_again = sessions_again.open_child_dir(encoded_cwd)?;
    parent_again.ensure_owner_only()?;
    if !parent.same_identity(&parent_again)? {
        return Err(uncertain(
            "publication parent identity changed after create",
        ));
    }

    Ok(PublicationParent {
        root_dir: root_dir.to_path_buf(),
        sessions: sessions_again,
        parent: parent_again,
        parent_name: encoded_cwd.to_owned(),
        parent_path: root_dir.join("sessions").join(encoded_cwd),
    })
}

fn open_or_create_owner_only_child(
    parent: &AnchoredDirectory,
    name: &OsStr,
) -> io::Result<AnchoredDirectory> {
    match parent.open_child_dir(name) {
        Ok(child) => {
            child.ensure_owner_only()?;
            Ok(child)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match parent.create_child_dir(name) {
                Ok(child) => {
                    child.ensure_owner_only()?;
                    Ok(child)
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let child = parent.open_child_dir(name)?;
                    child.ensure_owner_only()?;
                    Ok(child)
                }
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

fn validate_single_component(name: &OsStr) -> io::Result<()> {
    let path = Path::new(name);
    let mut components = path.components();
    match (components.next(), components.next()) {
        (Some(std::path::Component::Normal(component)), None) if !component.is_empty() => Ok(()),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "publication parent name must be exactly one path component",
        )),
    }
}

fn uncertain(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;

    const ENCODED: &str = "%2Frepo%2Fissue-340%2Fworkspace";

    fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut out = BTreeMap::new();
        fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
            let mut entries: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
            entries.sort_by_key(|e| e.file_name());
            for entry in entries {
                let path = entry.path();
                let rel = path.strip_prefix(root).unwrap().to_path_buf();
                let meta = fs::symlink_metadata(&path).unwrap();
                if meta.file_type().is_symlink() {
                    out.insert(
                        rel,
                        format!("symlink->{}", fs::read_link(&path).unwrap().display())
                            .into_bytes(),
                    );
                } else if meta.is_dir() {
                    out.insert(rel, b"dir".to_vec());
                    walk(root, &path, out);
                } else {
                    out.insert(rel, fs::read(&path).unwrap());
                }
            }
        }
        walk(root, root, &mut out);
        out
    }

    #[test]
    fn ensure_publication_parent_creates_owner_only_sessions_and_cwd() {
        let root = tempfile::tempdir().unwrap();
        let parent = ensure_publication_parent(root.path(), OsStr::new(ENCODED)).unwrap();
        parent.revalidate().unwrap();
        assert!(parent.path().is_dir());
        parent
            .ensure_session_dir(OsStr::new("019c0000-0000-7000-8000-000000000340"))
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            for directory in [
                root.path().join("sessions"),
                parent.path().to_path_buf(),
                parent.path().join("019c0000-0000-7000-8000-000000000340"),
            ] {
                assert_eq!(
                    fs::metadata(directory).unwrap().permissions().mode() & 0o777,
                    0o700
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn ensure_publication_parent_rejects_symlinked_encoded_cwd() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("canary"), b"outside-canary-340").unwrap();
        let before = snapshot(outside.path());
        let sessions = root.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        symlink(outside.path(), sessions.join(ENCODED)).unwrap();

        let Err(error) = ensure_publication_parent(root.path(), OsStr::new(ENCODED)) else {
            panic!("symlinked encoded-CWD parent must fail closed");
        };
        assert_ne!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(snapshot(outside.path()), before);
        assert_eq!(
            fs::read(outside.path().join("canary")).unwrap(),
            b"outside-canary-340"
        );
    }

    #[cfg(unix)]
    #[test]
    fn ensure_publication_parent_rejects_symlinked_sessions_root() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("canary"), b"sessions-canary").unwrap();
        let before = snapshot(outside.path());
        symlink(outside.path(), root.path().join("sessions")).unwrap();

        assert!(ensure_publication_parent(root.path(), OsStr::new(ENCODED)).is_err());
        assert_eq!(snapshot(outside.path()), before);
    }

    #[cfg(windows)]
    #[test]
    fn ensure_publication_parent_rejects_reparse_encoded_cwd() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("canary"), b"win-canary-340").unwrap();
        let sessions = root.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        if let Err(error) =
            std::os::windows::fs::symlink_dir(outside.path(), sessions.join(ENCODED))
        {
            eprintln!("skip windows reparse fixture (cannot create directory symlink): {error}");
            return;
        }
        let before = snapshot(outside.path());
        assert!(ensure_publication_parent(root.path(), OsStr::new(ENCODED)).is_err());
        assert_eq!(snapshot(outside.path()), before);
    }
}
