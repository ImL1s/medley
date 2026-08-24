//! Directory operations anchored to a retained operating-system handle.
//!
//! Once opened, an [`AnchoredDirectory`] never resolves children through the
//! original pathname again. This prevents a concurrently renamed or replaced
//! parent pathname from redirecting a mutation into another tree.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::path::{Component, Path};

/// A real directory retained by file descriptor/handle.
#[derive(Debug)]
pub struct AnchoredDirectory {
    file: File,
    parent: Option<File>,
    name: Option<OsString>,
}

/// A failed retained rename together with the still-live source anchor.
#[derive(Debug)]
pub struct AnchoredRenameError {
    pub error: io::Error,
    pub source: AnchoredDirectory,
}

impl AnchoredDirectory {
    /// Open and retain `path`, rejecting symlinks and Windows reparse points.
    pub fn open_root(path: &Path) -> io::Result<Self> {
        platform::open_root(path).map(|file| Self {
            file,
            parent: None,
            name: None,
        })
    }

    /// Open a real direct child directory without following its final link.
    pub fn open_child_dir(&self, name: &OsStr) -> io::Result<Self> {
        validate_child_name(name)?;
        let parent = self.file.try_clone()?;
        platform::open_child_dir(&self.file, name).map(|file| Self {
            file,
            parent: Some(parent),
            name: Some(name.to_owned()),
        })
    }

    /// Create a direct child directory with owner-only permissions and retain it.
    ///
    /// This is exclusive: an existing child is reported as `AlreadyExists`.
    pub fn create_child_dir(&self, name: &OsStr) -> io::Result<Self> {
        validate_child_name(name)?;
        let parent = self.file.try_clone()?;
        platform::create_child_dir(&self.file, name).map(|file| Self {
            file,
            parent: Some(parent),
            name: Some(name.to_owned()),
        })
    }

    /// Exclusively create an owner-only direct child regular file.
    pub fn create_child_file_new(&self, name: &OsStr) -> io::Result<File> {
        validate_child_name(name)?;
        platform::create_child_file_new(&self.file, name)
    }

    /// Open an existing direct child regular file without following links.
    pub fn open_child_file(&self, name: &OsStr) -> io::Result<File> {
        validate_child_name(name)?;
        platform::open_child_file(&self.file, name)
    }

    /// Open or create an owner-only direct child regular file for locking.
    ///
    /// The returned file is read-write, is opened relative to this retained
    /// directory without following links or reparse points, and is validated
    /// as owned by the current user. Unix additionally requires exactly one
    /// hard link. On Windows, the held handle does not share delete access so
    /// its directory entry cannot be replaced while an advisory lock is active.
    pub fn open_or_create_owner_only_child_file(&self, name: &OsStr) -> io::Result<File> {
        validate_child_name(name)?;
        platform::open_or_create_owner_only_child_file(&self.file, name)
    }

    /// Remove a named empty direct child directory without following links.
    pub fn remove_empty_child_dir(&self, name: &OsStr) -> io::Result<()> {
        validate_child_name(name)?;
        platform::remove_empty_child_dir(&self.file, name)
    }

    /// Remove a named direct child marker file without following links.
    pub fn remove_marker(&self, name: &OsStr) -> io::Result<()> {
        validate_child_name(name)?;
        platform::remove_marker(&self.file, name)
    }

    /// Flush directory metadata to stable storage where the platform supports it.
    pub fn sync(&self) -> io::Result<()> {
        self.file.sync_all()
    }

    /// Verify that the retained directory belongs to the current user and
    /// tighten its permissions to owner-only using this same handle.
    pub fn ensure_owner_only(&self) -> io::Result<()> {
        platform::ensure_owner_only(&self.file)
    }

    /// Return whether this retained directory is already private to its owner.
    pub fn is_owner_only(&self) -> io::Result<bool> {
        platform::is_owner_only(&self.file)
    }

    /// Compare the stable filesystem identity of two retained directories.
    pub fn same_identity(&self, other: &Self) -> io::Result<bool> {
        platform::same_identity(&self.file, &other.file)
    }

    /// Compare this retained directory with an already-open file handle.
    pub fn same_identity_as_file(&self, other: &File) -> io::Result<bool> {
        platform::same_identity(&self.file, other)
    }

    /// Remove this retained child directory through its original anchored parent.
    pub fn remove_self(self) -> io::Result<()> {
        let parent = self.parent.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "root anchor has no parent")
        })?;
        let name = self.name.as_deref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "root anchor has no child name")
        })?;
        platform::remove_retained_child_dir(parent, name, &self.file)
    }

    /// Recursively remove this retained child directory without following links.
    pub fn remove_tree_self(self) -> io::Result<()> {
        platform::remove_tree_contents(&self.file)?;
        self.remove_self()
    }

    /// List direct child names through the retained directory handle.
    pub fn child_names(&self) -> io::Result<Vec<OsString>> {
        platform::child_names(&self.file)
    }

    /// Rename this retained child directory without replacement.
    pub fn try_rename_self_no_replace(
        mut self,
        target_parent: &Self,
        target_name: &OsStr,
    ) -> Result<Self, AnchoredRenameError> {
        if let Err(error) = validate_child_name(target_name) {
            return Err(AnchoredRenameError {
                error,
                source: self,
            });
        }
        let new_parent = match target_parent.file.try_clone() {
            Ok(parent) => parent,
            Err(error) => {
                return Err(AnchoredRenameError {
                    error,
                    source: self,
                });
            }
        };
        let new_name = target_name.to_owned();
        let Some(source_parent) = self.parent.as_ref() else {
            return Err(AnchoredRenameError {
                error: io::Error::new(io::ErrorKind::InvalidInput, "root anchor has no parent"),
                source: self,
            });
        };
        let Some(source_name) = self.name.as_deref() else {
            return Err(AnchoredRenameError {
                error: io::Error::new(io::ErrorKind::InvalidInput, "root anchor has no child name"),
                source: self,
            });
        };
        if let Err(error) = platform::rename_retained_child_dir_no_replace(
            source_parent,
            source_name,
            &self.file,
            &target_parent.file,
            target_name,
        ) {
            return Err(AnchoredRenameError {
                error,
                source: self,
            });
        }
        self.parent = Some(new_parent);
        self.name = Some(new_name);
        Ok(self)
    }

    /// Rename this retained child directory without replacement.
    pub fn rename_self_no_replace(
        self,
        target_parent: &Self,
        target_name: &OsStr,
    ) -> io::Result<Self> {
        self.try_rename_self_no_replace(target_parent, target_name)
            .map_err(|failure| failure.error)
    }

    /// Rename a child directory between retained parents without replacement.
    pub fn rename_child_dir_no_replace(
        &self,
        source_name: &OsStr,
        target_parent: &Self,
        target_name: &OsStr,
    ) -> io::Result<()> {
        validate_child_name(source_name)?;
        validate_child_name(target_name)?;
        platform::rename_child_dir_no_replace(
            &self.file,
            source_name,
            &target_parent.file,
            target_name,
        )
    }
}

fn validate_child_name(name: &OsStr) -> io::Result<()> {
    let path = Path::new(name);
    let mut components = path.components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(component)), None) if !component.is_empty() => Ok(()),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "anchored child name must be exactly one normal path component",
        )),
    }
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd, RawFd};
    use std::os::unix::ffi::OsStrExt as _;

    fn c_name(name: &OsStr) -> io::Result<CString> {
        CString::new(name.as_bytes()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "child name contains a NUL byte",
            )
        })
    }

    fn open_directory_at(parent: RawFd, name: &OsStr) -> io::Result<File> {
        let name = c_name(name)?;
        let fd = unsafe {
            libc::openat(
                parent,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { File::from_raw_fd(fd) })
        }
    }

    fn open_regular_file_at(
        parent: RawFd,
        name: &OsStr,
        flags: i32,
        mode: u32,
    ) -> io::Result<File> {
        let name = c_name(name)?;
        let fd = unsafe {
            libc::openat(
                parent,
                name.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                mode,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "anchored child is not a regular file",
            ));
        }
        Ok(file)
    }

    pub(super) fn open_root(path: &Path) -> io::Result<File> {
        use std::os::unix::fs::OpenOptionsExt as _;

        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        if !file.metadata()?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "anchored root is not a directory",
            ));
        }
        Ok(file)
    }

    pub(super) fn open_child_dir(parent: &File, name: &OsStr) -> io::Result<File> {
        open_directory_at(parent.as_raw_fd(), name)
    }

    pub(super) fn open_child_file(parent: &File, name: &OsStr) -> io::Result<File> {
        open_regular_file_at(parent.as_raw_fd(), name, libc::O_RDONLY, 0)
    }

    pub(super) fn create_child_file_new(parent: &File, name: &OsStr) -> io::Result<File> {
        let file = open_regular_file_at(
            parent.as_raw_fd(),
            name,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )?;
        let metadata = file.metadata()?;
        if std::os::unix::fs::MetadataExt::uid(&metadata) != unsafe { libc::geteuid() } {
            drop(file);
            let _ = unlink(parent, name, 0);
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "created file is not owned by the current user",
            ));
        }
        let result = unsafe { libc::fchmod(file.as_raw_fd(), 0o600) };
        if result != 0 {
            let error = io::Error::last_os_error();
            drop(file);
            let _ = unlink(parent, name, 0);
            return Err(error);
        }
        Ok(file)
    }

    fn validate_owner_only_lock_file(file: &File) -> io::Result<()> {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let metadata = file.metadata()?;
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "anchored lock file is not owned by the current user",
            ));
        }
        if metadata.nlink() != 1 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "anchored lock file must have exactly one hard link",
            ));
        }
        if metadata.permissions().mode() & 0o777 != 0o600 {
            let mut permissions = metadata.permissions();
            permissions.set_mode(0o600);
            file.set_permissions(permissions)?;
        }
        Ok(())
    }

    pub(super) fn open_or_create_owner_only_child_file(
        parent: &File,
        name: &OsStr,
    ) -> io::Result<File> {
        let file = match open_regular_file_at(
            parent.as_raw_fd(),
            name,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            0o600,
        ) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                open_regular_file_at(parent.as_raw_fd(), name, libc::O_RDWR, 0)?
            }
            Err(error) => return Err(error),
        };
        validate_owner_only_lock_file(&file)?;
        Ok(file)
    }

    pub(super) fn create_child_dir(parent: &File, name: &OsStr) -> io::Result<File> {
        let name = c_name(name)?;
        let result = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }

        let file = match open_directory_at(parent.as_raw_fd(), OsStr::from_bytes(name.as_bytes())) {
            Ok(file) => file,
            Err(error) => {
                let _ = unsafe {
                    libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR)
                };
                return Err(error);
            }
        };
        let secure_result = (|| {
            let metadata = file.metadata()?;
            if std::os::unix::fs::MetadataExt::uid(&metadata) != unsafe { libc::geteuid() } {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "created directory is not owned by the current user",
                ));
            }
            let chmod_result = unsafe { libc::fchmod(file.as_raw_fd(), 0o700) };
            if chmod_result != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        })();
        if let Err(error) = secure_result {
            drop(file);
            let _ =
                unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) };
            return Err(error);
        }
        Ok(file)
    }

    pub(super) fn ensure_owner_only(file: &File) -> io::Result<()> {
        let metadata = file.metadata()?;
        if std::os::unix::fs::MetadataExt::uid(&metadata) != unsafe { libc::geteuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "anchored directory is not owned by the current user",
            ));
        }
        let result = unsafe { libc::fchmod(file.as_raw_fd(), 0o700) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub(super) fn is_owner_only(file: &File) -> io::Result<bool> {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let metadata = file.metadata()?;
        Ok(metadata.uid() == unsafe { libc::geteuid() }
            && metadata.permissions().mode() & 0o077 == 0)
    }

    pub(super) fn same_identity(left: &File, right: &File) -> io::Result<bool> {
        use std::os::unix::fs::MetadataExt as _;

        let left = left.metadata()?;
        let right = right.metadata()?;
        Ok(left.dev() == right.dev() && left.ino() == right.ino())
    }

    fn unlink(parent: &File, name: &OsStr, flags: i32) -> io::Result<()> {
        let name = c_name(name)?;
        let result = unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    struct DirectoryStream(*mut libc::DIR);

    impl Drop for DirectoryStream {
        fn drop(&mut self) {
            unsafe {
                libc::closedir(self.0);
            }
        }
    }

    fn errno_location() -> *mut libc::c_int {
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        unsafe {
            libc::__error()
        }
        #[cfg(not(any(target_os = "macos", target_os = "ios")))]
        unsafe {
            libc::__errno_location()
        }
    }

    pub(super) fn child_names(directory: &File) -> io::Result<Vec<OsString>> {
        let dot = CString::new(".").expect("static child name");
        let enumeration = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                dot.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if enumeration < 0 {
            return Err(io::Error::last_os_error());
        }
        let stream = unsafe { libc::fdopendir(enumeration) };
        if stream.is_null() {
            let error = io::Error::last_os_error();
            unsafe {
                libc::close(enumeration);
            }
            return Err(error);
        }
        let stream = DirectoryStream(stream);
        let mut names = Vec::new();
        loop {
            unsafe {
                *errno_location() = 0;
            }
            let entry = unsafe { libc::readdir(stream.0) };
            if entry.is_null() {
                let errno = unsafe { *errno_location() };
                return if errno == 0 {
                    Ok(names)
                } else {
                    Err(io::Error::from_raw_os_error(errno))
                };
            }
            let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if name != b"." && name != b".." {
                names.push(OsStr::from_bytes(name).to_owned());
            }
        }
    }

    pub(super) fn remove_tree_contents(directory: &File) -> io::Result<()> {
        for name in child_names(directory)? {
            let name_c = c_name(&name)?;
            let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
            let result = unsafe {
                libc::fstatat(
                    directory.as_raw_fd(),
                    name_c.as_ptr(),
                    metadata.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if result != 0 {
                return Err(io::Error::last_os_error());
            }
            let metadata = unsafe { metadata.assume_init() };
            if metadata.st_mode & libc::S_IFMT == libc::S_IFDIR {
                let child = open_directory_at(directory.as_raw_fd(), &name)?;
                remove_tree_contents(&child)?;
                remove_retained_child_dir(directory, &name, &child)?;
            } else {
                unlink(directory, &name, 0)?;
            }
        }
        Ok(())
    }

    pub(super) fn remove_empty_child_dir(parent: &File, name: &OsStr) -> io::Result<()> {
        unlink(parent, name, libc::AT_REMOVEDIR)
    }

    pub(super) fn remove_marker(parent: &File, name: &OsStr) -> io::Result<()> {
        unlink(parent, name, 0)
    }

    pub(super) fn remove_retained_child_dir(
        parent: &File,
        name: &OsStr,
        retained_child: &File,
    ) -> io::Result<()> {
        let current = open_directory_at(parent.as_raw_fd(), name)?;
        if !same_identity(&current, retained_child)? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "retained child identity no longer matches its anchored name",
            ));
        }
        unlink(parent, name, libc::AT_REMOVEDIR)
    }

    pub(super) fn rename_child_dir_no_replace(
        source_parent: &File,
        source_name: &OsStr,
        target_parent: &File,
        target_name: &OsStr,
    ) -> io::Result<()> {
        let source_name = c_name(source_name)?;
        let target_name = c_name(target_name)?;

        #[cfg(target_os = "linux")]
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                source_parent.as_raw_fd(),
                source_name.as_ptr(),
                target_parent.as_raw_fd(),
                target_name.as_ptr(),
                1_u32, // RENAME_NOREPLACE
            ) as libc::c_int
        };

        #[cfg(any(target_os = "macos", target_os = "ios"))]
        let result = unsafe {
            unsafe extern "C" {
                fn renameatx_np(
                    fromfd: libc::c_int,
                    from: *const libc::c_char,
                    tofd: libc::c_int,
                    to: *const libc::c_char,
                    flags: libc::c_uint,
                ) -> libc::c_int;
            }
            const RENAME_EXCL: libc::c_uint = 0x0000_0004;
            renameatx_np(
                source_parent.as_raw_fd(),
                source_name.as_ptr(),
                target_parent.as_raw_fd(),
                target_name.as_ptr(),
                RENAME_EXCL,
            )
        };

        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios"))]
        {
            if result == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }

        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "ios")))]
        {
            let _ = (source_parent, target_parent);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "anchored no-replace rename is unsupported on this Unix platform",
            ))
        }
    }

    pub(super) fn rename_retained_child_dir_no_replace(
        source_parent: &File,
        source_name: &OsStr,
        retained_child: &File,
        target_parent: &File,
        target_name: &OsStr,
    ) -> io::Result<()> {
        let current = open_directory_at(source_parent.as_raw_fd(), source_name)?;
        if !same_identity(&current, retained_child)? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "retained child identity no longer matches its anchored name",
            ));
        }
        rename_child_dir_no_replace(source_parent, source_name, target_parent, target_name)
    }
}

#[cfg(windows)]
#[path = "anchored_windows.rs"]
mod platform;

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn rejects_non_component_child_names() {
        let invalid = ["", ".", "..", "a/b"];
        for name in invalid {
            assert_eq!(
                validate_child_name(OsStr::new(name)).unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
                "unexpectedly accepted {name:?}"
            );
        }
        validate_child_name(OsStr::new("child")).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn retained_parent_prevents_path_swap_from_redirecting_mutations() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        let retained_path = temp.path().join("retained");
        let outside = temp.path().join("outside");
        std::fs::create_dir(&root_path).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(root_path.join("marker"), b"retained").unwrap();
        std::fs::write(outside.join("marker"), b"outside sentinel").unwrap();

        let root = AnchoredDirectory::open_root(&root_path).unwrap();
        root.ensure_owner_only().unwrap();
        std::fs::rename(&root_path, &retained_path).unwrap();
        symlink(&outside, &root_path).unwrap();

        root.remove_marker(OsStr::new("marker")).unwrap();
        root.create_child_dir(OsStr::new("created")).unwrap();

        assert!(!retained_path.join("marker").exists());
        assert!(retained_path.join("created").is_dir());
        assert_eq!(
            std::fs::read(outside.join("marker")).unwrap(),
            b"outside sentinel"
        );
        assert!(!outside.join("created").exists());
    }

    #[cfg(unix)]
    #[test]
    fn owner_only_lock_file_rejects_symlinks_and_hardlinks() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        let outside = temp.path().join("outside");
        std::fs::create_dir(&root_path).unwrap();
        std::fs::write(&outside, b"sentinel").unwrap();
        let root = AnchoredDirectory::open_root(&root_path).unwrap();

        symlink(&outside, root_path.join("symlink.lock")).unwrap();
        assert!(
            root.open_or_create_owner_only_child_file(OsStr::new("symlink.lock"))
                .is_err()
        );
        assert_eq!(std::fs::read(&outside).unwrap(), b"sentinel");

        std::fs::hard_link(&outside, root_path.join("hardlink.lock")).unwrap();
        assert!(
            root.open_or_create_owner_only_child_file(OsStr::new("hardlink.lock"))
                .is_err()
        );
        assert_eq!(std::fs::read(&outside).unwrap(), b"sentinel");
    }

    #[cfg(unix)]
    #[test]
    fn anchored_rename_is_no_replace_and_preserves_both_trees_on_collision() {
        let temp = tempfile::tempdir().unwrap();
        let source_path = temp.path().join("source-parent");
        let target_path = temp.path().join("target-parent");
        std::fs::create_dir(&source_path).unwrap();
        std::fs::create_dir(&target_path).unwrap();
        std::fs::create_dir(source_path.join("session")).unwrap();
        std::fs::write(source_path.join("session/source"), b"source").unwrap();
        std::fs::create_dir(target_path.join("session")).unwrap();
        std::fs::write(target_path.join("session/target"), b"target").unwrap();

        let source = AnchoredDirectory::open_root(&source_path).unwrap();
        let target = AnchoredDirectory::open_root(&target_path).unwrap();
        let error = source
            .rename_child_dir_no_replace(OsStr::new("session"), &target, OsStr::new("session"))
            .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read(source_path.join("session/source")).unwrap(),
            b"source"
        );
        assert_eq!(
            std::fs::read(target_path.join("session/target")).unwrap(),
            b"target"
        );
    }
}
