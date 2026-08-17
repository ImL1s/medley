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

    /// Remove this retained child directory through its original anchored parent.
    ///
    /// Root anchors cannot remove themselves. Unix verifies that the source name
    /// still resolves to this retained directory before unlinking. Windows marks
    /// this retained handle for deletion directly.
    pub fn remove_self(self) -> io::Result<()> {
        let parent = self.parent.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "root anchor has no parent")
        })?;
        let name = self.name.as_deref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "root anchor has no child name")
        })?;
        platform::remove_retained_child_dir(parent, name, &self.file)
    }

    /// Recursively remove this retained child directory without resolving any
    /// descendant through a pathname.
    ///
    /// Real descendant directories are opened without following their final
    /// link. Symlinks and Windows reparse points are removed as leaf entries and
    /// are never traversed. Root anchors cannot remove themselves.
    pub fn remove_tree_self(self) -> io::Result<()> {
        platform::remove_tree_contents(&self.file)?;
        self.remove_self()
    }

    /// List direct child names through the retained directory handle.
    pub fn child_names(&self) -> io::Result<Vec<OsString>> {
        platform::child_names(&self.file)
    }

    /// Rename this retained child directory without replacement.
    ///
    /// This consumes and returns the source anchor with its lineage updated to
    /// the target parent/name. The retained child handle itself is unchanged, so
    /// callers can compare it with a newly opened public pathname after commit.
    /// On Windows the already-open child handle is renamed directly, avoiding an
    /// incompatible second open with `DELETE`.
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
    ///
    /// Prefer [`Self::try_rename_self_no_replace`] when the source must survive
    /// a collision. This compatibility form drops the source on error.
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
mod platform {
    use super::*;
    use std::mem::{MaybeUninit, offset_of, size_of};
    use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
    use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{
        GetSecurityInfo, SE_FILE_OBJECT, SetSecurityInfo,
    };
    use windows::Win32::Security::GetTokenInformation;
    use windows::Win32::Security::{
        ACL, ACL_REVISION, AddAccessAllowedAceEx, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION,
        EqualSid, GetLengthSid, InitializeAcl, InitializeSecurityDescriptor, OBJECT_INHERIT_ACE,
        OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        PSID, SE_DACL_PROTECTED, SECURITY_DESCRIPTOR, SetSecurityDescriptorControl,
        SetSecurityDescriptorDacl, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ALL_ACCESS, FILE_APPEND_DATA,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_INFO,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_LIST_DIRECTORY,
        FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_RENAME_INFO,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE, FILE_WRITE_ATTRIBUTES,
        FILE_WRITE_DATA, FileDispositionInfo, FileRenameInfoEx, GetFileInformationByHandle,
        GetFinalPathNameByHandleW, SYNCHRONIZE, SetFileInformationByHandle,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    const FILE_OPEN: u32 = 1;
    const FILE_CREATE: u32 = 2;
    const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
    const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;
    const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x0000_0020;
    const FILE_OPEN_REPARSE_POINT_NT: u32 = 0x0020_0000;
    const OBJ_CASE_INSENSITIVE: u32 = 0x0000_0040;
    const SECURITY_DESCRIPTOR_REVISION: u32 = 1;

    #[repr(C)]
    struct UnicodeString {
        length: u16,
        maximum_length: u16,
        buffer: *mut u16,
    }

    #[repr(C)]
    struct ObjectAttributes {
        length: u32,
        root_directory: HANDLE,
        object_name: *mut UnicodeString,
        attributes: u32,
        security_descriptor: *mut core::ffi::c_void,
        security_quality_of_service: *mut core::ffi::c_void,
    }

    #[repr(C)]
    struct IoStatusBlock {
        status_or_pointer: usize,
        information: usize,
    }

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtCreateFile(
            file_handle: *mut HANDLE,
            desired_access: u32,
            object_attributes: *mut ObjectAttributes,
            io_status_block: *mut IoStatusBlock,
            allocation_size: *mut i64,
            file_attributes: u32,
            share_access: u32,
            create_disposition: u32,
            create_options: u32,
            ea_buffer: *mut core::ffi::c_void,
            ea_length: u32,
        ) -> i32;
        fn RtlNtStatusToDosError(status: i32) -> u32;
        fn NtSetInformationFile(
            file_handle: HANDLE,
            io_status_block: *mut IoStatusBlock,
            file_information: *const core::ffi::c_void,
            length: u32,
            file_information_class: u32,
        ) -> i32;
        fn NtQueryDirectoryFile(
            file_handle: HANDLE,
            event: HANDLE,
            apc_routine: *mut core::ffi::c_void,
            apc_context: *mut core::ffi::c_void,
            io_status_block: *mut IoStatusBlock,
            file_information: *mut core::ffi::c_void,
            length: u32,
            file_information_class: u32,
            return_single_entry: u8,
            file_name: *mut UnicodeString,
            restart_scan: u8,
        ) -> i32;
    }

    const FILE_NAMES_INFORMATION: u32 = 12;
    const FILE_RENAME_INFORMATION: u32 = 10;
    const FILE_RENAME_INFORMATION_EX: u32 = 65;
    const FILE_RENAME_REPLACE_IF_EXISTS: u32 = 0x0000_0001;
    const FILE_RENAME_POSIX_SEMANTICS: u32 = 0x0000_0002;
    const FILE_RENAME_IGNORE_READONLY_ATTRIBUTE: u32 = 0x0000_0040;
    // POSIX exclusive-without-replace is rejected on current GHA images.
    // Collision is enforced by probing the destination first; REPLACE is
    // required for POSIX semantics (open SHARE_DELETE children can follow).
    const FILE_RENAME_POSIX_REPLACE: u32 = FILE_RENAME_REPLACE_IF_EXISTS
        | FILE_RENAME_POSIX_SEMANTICS
        | FILE_RENAME_IGNORE_READONLY_ATTRIBUTE;
    const FILE_DISPOSITION_INFORMATION_EX: u32 = 64;
    const FILE_DISPOSITION_DELETE: u32 = 0x0000_0001;
    const FILE_DISPOSITION_POSIX_SEMANTICS: u32 = 0x0000_0002;
    const FILE_DISPOSITION_IGNORE_READONLY_ATTRIBUTE: u32 = 0x0000_0010;
    const STATUS_NO_MORE_FILES: i32 = 0x8000_0006_u32 as i32;
    const STATUS_NO_SUCH_FILE: i32 = 0xC000_000F_u32 as i32;
    const STATUS_OBJECT_NAME_COLLISION: i32 = 0xC000_0035_u32 as i32;
    const STATUS_DIRECTORY_NOT_EMPTY: i32 = 0xC000_0101_u32 as i32;

    fn nt_error(status: i32) -> io::Error {
        io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32)
    }

    fn open_relative_with_share(
        parent: &File,
        name: &OsStr,
        disposition: u32,
        object_options: u32,
        desired_access: u32,
        share_access: u32,
        security_descriptor: Option<*mut core::ffi::c_void>,
    ) -> io::Result<File> {
        let mut wide: Vec<u16> = name.encode_wide().collect();
        let byte_len = wide
            .len()
            .checked_mul(2)
            .and_then(|length| u16::try_from(length).ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "child name is too long"))?;
        let mut unicode = UnicodeString {
            length: byte_len,
            maximum_length: byte_len,
            buffer: wide.as_mut_ptr(),
        };
        let mut attributes = ObjectAttributes {
            length: size_of::<ObjectAttributes>() as u32,
            root_directory: HANDLE(parent.as_raw_handle()),
            object_name: &mut unicode,
            attributes: OBJ_CASE_INSENSITIVE,
            security_descriptor: security_descriptor.unwrap_or(std::ptr::null_mut()),
            security_quality_of_service: std::ptr::null_mut(),
        };
        let mut status_block = IoStatusBlock {
            status_or_pointer: 0,
            information: 0,
        };
        let mut handle = HANDLE::default();
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                desired_access,
                &mut attributes,
                &mut status_block,
                std::ptr::null_mut(),
                0,
                share_access,
                disposition,
                object_options | FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT_NT,
                std::ptr::null_mut(),
                0,
            )
        };
        if status < 0 {
            Err(nt_error(status))
        } else {
            Ok(unsafe { File::from_raw_handle(handle.0) })
        }
    }

    fn open_relative(
        parent: &File,
        name: &OsStr,
        disposition: u32,
        object_options: u32,
        desired_access: u32,
        security_descriptor: Option<*mut core::ffi::c_void>,
    ) -> io::Result<File> {
        open_relative_with_share(
            parent,
            name,
            disposition,
            object_options,
            desired_access,
            (FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0,
            security_descriptor,
        )
    }

    fn file_information(file: &File) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }
            .map_err(io::Error::other)?;
        Ok(info)
    }

    fn reject_reparse_directory(file: File) -> io::Result<File> {
        let info = file_information(&file)?;
        if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "anchored directory is a reparse point",
            ));
        }
        if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "anchored root is not a directory",
            ));
        }
        Ok(file)
    }

    fn reject_reparse_regular_file(file: File) -> io::Result<File> {
        let info = file_information(&file)?;
        if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
            || info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "anchored child is not a real regular file",
            ));
        }
        Ok(file)
    }

    pub(super) fn open_root(path: &Path) -> io::Result<File> {
        let file = std::fs::OpenOptions::new()
            .access_mode(
                (FILE_LIST_DIRECTORY
                    | FILE_READ_ATTRIBUTES
                    | FILE_WRITE_ATTRIBUTES
                    | FILE_TRAVERSE
                    | FILE_WRITE_DATA
                    | FILE_APPEND_DATA
                    | DELETE
                    | windows::Win32::Storage::FileSystem::READ_CONTROL
                    | windows::Win32::Storage::FileSystem::WRITE_DAC
                    | SYNCHRONIZE)
                    .0,
            )
            .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
            .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0)
            .open(path)?;
        reject_reparse_directory(file)
    }

    pub(super) fn open_child_dir(parent: &File, name: &OsStr) -> io::Result<File> {
        let privileged = (FILE_LIST_DIRECTORY
            | FILE_READ_ATTRIBUTES
            | FILE_WRITE_ATTRIBUTES
            | FILE_TRAVERSE
            | FILE_WRITE_DATA
            | FILE_APPEND_DATA
            | DELETE
            | windows::Win32::Storage::FileSystem::READ_CONTROL
            | windows::Win32::Storage::FileSystem::WRITE_DAC
            | SYNCHRONIZE)
            .0;
        // GHA (and other Administrators-owned temp dirs) often deny WRITE_DAC
        // on a directory created by create_dir_all. Publication only needs
        // add-child / delete-child access.
        let reduced = (FILE_LIST_DIRECTORY
            | FILE_READ_ATTRIBUTES
            | FILE_WRITE_ATTRIBUTES
            | FILE_TRAVERSE
            | FILE_WRITE_DATA
            | FILE_APPEND_DATA
            | DELETE
            | SYNCHRONIZE)
            .0;
        let file = match open_relative(
            parent,
            name,
            FILE_OPEN,
            FILE_DIRECTORY_FILE,
            privileged,
            None,
        ) {
            Ok(file) => file,
            Err(error)
                if error.kind() == io::ErrorKind::PermissionDenied
                    || error.raw_os_error() == Some(5) =>
            {
                open_relative(parent, name, FILE_OPEN, FILE_DIRECTORY_FILE, reduced, None)?
            }
            Err(error) => return Err(error),
        };
        reject_reparse_directory(file)
    }

    pub(super) fn open_child_file(parent: &File, name: &OsStr) -> io::Result<File> {
        let file = open_relative(
            parent,
            name,
            FILE_OPEN,
            FILE_NON_DIRECTORY_FILE,
            (FILE_READ_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE).0,
            None,
        )?;
        reject_reparse_regular_file(file)
    }

    pub(super) fn create_child_file_new(parent: &File, name: &OsStr) -> io::Result<File> {
        let file = open_relative(
            parent,
            name,
            FILE_CREATE,
            FILE_NON_DIRECTORY_FILE,
            (FILE_READ_DATA
                | FILE_READ_ATTRIBUTES
                | FILE_WRITE_DATA
                | SYNCHRONIZE
                | DELETE
                | windows::Win32::Storage::FileSystem::READ_CONTROL
                | windows::Win32::Storage::FileSystem::WRITE_DAC)
                .0,
            None,
        )?;
        let file = reject_reparse_regular_file(file)?;
        if let Err(error) = crate::util::secure_file::ensure_owner_only_permissions_for_file(&file)
        {
            let _ = delete_by_handle(&file);
            return Err(error);
        }
        Ok(file)
    }

    fn ensure_owner_only_regular_file(file: &File) -> io::Result<()> {
        let security =
            OwnerOnlySecurity::new_with_inheritance(windows::Win32::Security::ACE_FLAGS(0))?;
        let _ = crate::util::secure_file::enable_windows_privilege(windows::core::w!(
            "SeTakeOwnershipPrivilege"
        ));
        let _ = crate::util::secure_file::enable_windows_privilege(windows::core::w!(
            "SeRestorePrivilege"
        ));
        unsafe {
            let handle = HANDLE(file.as_raw_handle());
            let owned = SetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION
                    | DACL_SECURITY_INFORMATION
                    | PROTECTED_DACL_SECURITY_INFORMATION,
                Some(security.sid()),
                None,
                Some(security.acl()),
                None,
            );
            if owned.0 == 0 {
                return Ok(());
            }
            let status = SetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(security.acl()),
                None,
            );
            if status.0 == 0 {
                Ok(())
            } else {
                Err(io::Error::from_raw_os_error(status.0 as i32))
            }
        }
    }

    pub(super) fn open_or_create_owner_only_child_file(
        parent: &File,
        name: &OsStr,
    ) -> io::Result<File> {
        let desired_access = (FILE_READ_DATA
            | FILE_READ_ATTRIBUTES
            | FILE_WRITE_DATA
            | SYNCHRONIZE
            | windows::Win32::Storage::FileSystem::READ_CONTROL
            | windows::Win32::Storage::FileSystem::WRITE_DAC)
            .0;
        let share_access = (FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0;
        let mut security =
            OwnerOnlySecurity::new_with_inheritance(windows::Win32::Security::ACE_FLAGS(0))?;
        let file = match open_relative_with_share(
            parent,
            name,
            FILE_CREATE,
            FILE_NON_DIRECTORY_FILE,
            desired_access,
            share_access,
            Some((&mut security.descriptor as *mut SECURITY_DESCRIPTOR).cast()),
        ) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => open_relative_with_share(
                parent,
                name,
                FILE_OPEN,
                FILE_NON_DIRECTORY_FILE,
                desired_access,
                share_access,
                None,
            )?,
            Err(error) => return Err(error),
        };
        let file = reject_reparse_regular_file(file)?;
        ensure_owner_only_regular_file(&file)?;
        Ok(file)
    }

    #[cfg(test)]
    pub(super) fn owner_only_lock_file_security_is_current_user_protected(
        file: &File,
    ) -> io::Result<bool> {
        let current_user =
            OwnerOnlySecurity::new_with_inheritance(windows::Win32::Security::ACE_FLAGS(0))?;
        unsafe {
            let mut dacl = std::ptr::null_mut();
            let mut raw_descriptor = PSECURITY_DESCRIPTOR::default();
            let status = GetSecurityInfo(
                HANDLE(file.as_raw_handle()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(&mut dacl),
                None,
                Some(&mut raw_descriptor),
            );
            if status.0 != 0 {
                return Err(io::Error::from_raw_os_error(status.0 as i32));
            }
            struct LocalDescriptor(PSECURITY_DESCRIPTOR);
            impl Drop for LocalDescriptor {
                fn drop(&mut self) {
                    unsafe {
                        let _ = LocalFree(Some(HLOCAL(self.0.0)));
                    }
                }
            }
            let _descriptor = LocalDescriptor(raw_descriptor);

            let mut control = 0u16;
            let mut revision = 0u32;
            windows::Win32::Security::GetSecurityDescriptorControl(
                raw_descriptor,
                &mut control,
                &mut revision,
            )
            .map_err(io::Error::other)?;
            if control & SE_DACL_PROTECTED.0 == 0 || dacl.is_null() {
                return Ok(false);
            }

            // GHA temp volumes often keep Administrators as NTFS owner even
            // after WRITE_DAC. The lock contract is a protected DACL with
            // current-user full control and no foreign allow ACEs — the same
            // policy as directory owner-only verification.
            let current_sid = current_user.sid();
            let mut current_user_full_control = false;
            let mut foreign_allow = false;
            for index in 0..(*dacl).AceCount {
                let mut raw_ace = std::ptr::null_mut();
                if windows::Win32::Security::GetAce(dacl, u32::from(index), &mut raw_ace).is_err()
                    || raw_ace.is_null()
                {
                    continue;
                }
                let ace = &*(raw_ace as *const windows::Win32::Security::ACCESS_ALLOWED_ACE);
                if ace.Header.AceType != 0 {
                    continue;
                }
                let ace_sid = PSID(std::ptr::addr_of!(ace.SidStart).cast_mut().cast());
                let full_control = ace.Mask == FILE_ALL_ACCESS.0
                    || ace.Mask & FILE_ALL_ACCESS.0 == FILE_ALL_ACCESS.0;
                if EqualSid(ace_sid, current_sid).is_ok() {
                    if full_control {
                        current_user_full_control = true;
                    }
                } else {
                    foreign_allow = true;
                }
            }
            Ok(current_user_full_control && !foreign_allow)
        }
    }

    pub(super) fn create_child_dir(parent: &File, name: &OsStr) -> io::Result<File> {
        let mut security = OwnerOnlySecurity::new()?;
        let file = open_relative(
            parent,
            name,
            FILE_CREATE,
            FILE_DIRECTORY_FILE,
            (FILE_LIST_DIRECTORY
                | FILE_READ_ATTRIBUTES
                | FILE_WRITE_ATTRIBUTES
                | FILE_TRAVERSE
                | FILE_WRITE_DATA
                | FILE_APPEND_DATA
                | SYNCHRONIZE
                | DELETE
                | windows::Win32::Storage::FileSystem::READ_CONTROL
                | windows::Win32::Storage::FileSystem::WRITE_DAC)
                .0,
            Some((&mut security.descriptor as *mut SECURITY_DESCRIPTOR).cast()),
        )?;
        let file = reject_reparse_directory(file)?;
        if let Err(error) = ensure_owner_only(&file) {
            let _ = delete_by_handle(&file);
            return Err(error);
        }
        Ok(file)
    }

    struct OwnerOnlySecurity {
        _token_info: Vec<usize>,
        _acl: Vec<usize>,
        descriptor: SECURITY_DESCRIPTOR,
    }

    impl OwnerOnlySecurity {
        fn new() -> io::Result<Self> {
            Self::new_with_inheritance(CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE)
        }

        fn new_with_inheritance(
            inheritance: windows::Win32::Security::ACE_FLAGS,
        ) -> io::Result<Self> {
            unsafe {
                let mut token = HANDLE::default();
                OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
                    .map_err(io::Error::other)?;
                struct Token(HANDLE);
                impl Drop for Token {
                    fn drop(&mut self) {
                        let _ = unsafe { CloseHandle(self.0) };
                    }
                }
                let token = Token(token);
                let mut required = 0;
                let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut required);
                if required == 0 {
                    return Err(io::Error::last_os_error());
                }
                let mut token_info =
                    vec![0_usize; (required as usize).div_ceil(size_of::<usize>())];
                GetTokenInformation(
                    token.0,
                    TokenUser,
                    Some(token_info.as_mut_ptr().cast()),
                    required,
                    &mut required,
                )
                .map_err(io::Error::other)?;
                let sid = (*(token_info.as_ptr().cast::<TOKEN_USER>())).User.Sid;
                let sid_len = GetLengthSid(sid);
                let acl_len = size_of::<ACL>()
                    + size_of::<windows::Win32::Security::ACCESS_ALLOWED_ACE>()
                    - size_of::<u32>()
                    + sid_len as usize;
                let mut acl_storage = vec![0_usize; acl_len.div_ceil(size_of::<usize>())];
                let acl = acl_storage.as_mut_ptr().cast::<ACL>();
                InitializeAcl(acl, acl_len as u32, ACL_REVISION).map_err(io::Error::other)?;
                AddAccessAllowedAceEx(acl, ACL_REVISION, inheritance, FILE_ALL_ACCESS.0, sid)
                    .map_err(io::Error::other)?;
                let mut descriptor = SECURITY_DESCRIPTOR::default();
                let descriptor_pointer = windows::Win32::Security::PSECURITY_DESCRIPTOR(
                    (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                );
                InitializeSecurityDescriptor(descriptor_pointer, SECURITY_DESCRIPTOR_REVISION)
                    .map_err(io::Error::other)?;
                SetSecurityDescriptorDacl(descriptor_pointer, true, Some(acl), false)
                    .map_err(io::Error::other)?;
                SetSecurityDescriptorControl(
                    descriptor_pointer,
                    SE_DACL_PROTECTED,
                    SE_DACL_PROTECTED,
                )
                .map_err(io::Error::other)?;
                Ok(Self {
                    _token_info: token_info,
                    _acl: acl_storage,
                    descriptor,
                })
            }
        }

        fn sid(&self) -> PSID {
            unsafe { (*(self._token_info.as_ptr().cast::<TOKEN_USER>())).User.Sid }
        }

        fn acl(&self) -> *const ACL {
            self._acl.as_ptr().cast()
        }
    }

    pub(super) fn ensure_owner_only(file: &File) -> io::Result<()> {
        let security = OwnerOnlySecurity::new()?;
        let _ = crate::util::secure_file::enable_windows_privilege(windows::core::w!(
            "SeTakeOwnershipPrivilege"
        ));
        let _ = crate::util::secure_file::enable_windows_privilege(windows::core::w!(
            "SeRestorePrivilege"
        ));
        unsafe {
            let handle = HANDLE(file.as_raw_handle());
            let owned = SetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION
                    | DACL_SECURITY_INFORMATION
                    | PROTECTED_DACL_SECURITY_INFORMATION,
                Some(security.sid()),
                None,
                Some(security.acl()),
                None,
            );
            if owned.0 == 0 {
                return Ok(());
            }
            // GHA temp directories may stay owned by Administrators even after
            // privilege enable. WRITE_DAC can still install a protected
            // current-user ACL; fail only if that also fails.
            let status = SetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(security.acl()),
                None,
            );
            if status.0 == 0 {
                Ok(())
            } else {
                Err(io::Error::from_raw_os_error(status.0 as i32))
            }
        }
    }

    pub(super) fn is_owner_only(file: &File) -> io::Result<bool> {
        // Windows private staging directories are created with a protected,
        // current-user-only DACL. Reapplying that policy via the retained
        // handle also verifies ownership and cannot be redirected by a path
        // replacement.
        ensure_owner_only(file).map(|()| true)
    }

    pub(super) fn same_identity(left: &File, right: &File) -> io::Result<bool> {
        let left = file_information(left)?;
        let right = file_information(right)?;
        Ok(left.dwVolumeSerialNumber == right.dwVolumeSerialNumber
            && left.nFileIndexHigh == right.nFileIndexHigh
            && left.nFileIndexLow == right.nFileIndexLow)
    }

    fn delete_by_handle(file: &File) -> io::Result<()> {
        // Junctions and other reparse points often reject FileDispositionInfo
        // with ERROR_ACCESS_DENIED (0x80070005) on GHA images. POSIX
        // FileDispositionInformationEx unlinks the name immediately and
        // ignores the read-only bit, which is what remove_tree needs so it
        // can delete a junction leaf without walking its target.
        #[repr(C)]
        struct FileDispositionInformationEx {
            flags: u32,
        }
        let info = FileDispositionInformationEx {
            flags: FILE_DISPOSITION_DELETE
                | FILE_DISPOSITION_POSIX_SEMANTICS
                | FILE_DISPOSITION_IGNORE_READONLY_ATTRIBUTE,
        };
        let mut status_block = IoStatusBlock {
            status_or_pointer: 0,
            information: 0,
        };
        let status = unsafe {
            NtSetInformationFile(
                HANDLE(file.as_raw_handle()),
                &mut status_block,
                (&raw const info).cast(),
                size_of::<FileDispositionInformationEx>() as u32,
                FILE_DISPOSITION_INFORMATION_EX,
            )
        };
        if status >= 0 {
            return Ok(());
        }

        let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
        unsafe {
            SetFileInformationByHandle(
                HANDLE(file.as_raw_handle()),
                FileDispositionInfo,
                (&raw const disposition).cast(),
                size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        }
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::Other,
                format!("delete-by-handle failed (ntstatus={status:#010x}, win32={error})"),
            )
        })
    }

    pub(super) fn child_names(directory: &File) -> io::Result<Vec<OsString>> {
        // Names-only on the retained handle. ReOpenFile + FileIdBothDirectoryInfo
        // returns ACCESS_DENIED on GHA once the directory contains a junction,
        // because the file-id query follows the reparse target.
        #[repr(C)]
        struct FileNamesInformation {
            next_entry_offset: u32,
            file_index: u32,
            file_name_length: u32,
            file_name: [u16; 1],
        }
        let name_offset = offset_of!(FileNamesInformation, file_name);
        let mut names = Vec::new();
        let mut storage = vec![0_u8; 16 * 1024];
        let mut restart_scan = 1_u8;
        loop {
            let mut status_block = IoStatusBlock {
                status_or_pointer: 0,
                information: 0,
            };
            let status = unsafe {
                NtQueryDirectoryFile(
                    HANDLE(directory.as_raw_handle()),
                    HANDLE::default(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut status_block,
                    storage.as_mut_ptr().cast(),
                    storage.len() as u32,
                    FILE_NAMES_INFORMATION,
                    0,
                    std::ptr::null_mut(),
                    restart_scan,
                )
            };
            restart_scan = 0;
            if status == STATUS_NO_MORE_FILES || status == STATUS_NO_SUCH_FILE {
                return Ok(names);
            }
            if status < 0 {
                return Err(nt_error(status));
            }

            let mut offset = 0_usize;
            loop {
                if offset
                    .checked_add(name_offset)
                    .is_none_or(|end| end > storage.len())
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "directory entry offset exceeds enumeration buffer",
                    ));
                }
                let info = unsafe { &*storage.as_ptr().add(offset).cast::<FileNamesInformation>() };
                let name_len = info.file_name_length as usize / size_of::<u16>();
                let name_bytes = name_len.saturating_mul(size_of::<u16>());
                if offset
                    .checked_add(name_offset)
                    .and_then(|start| start.checked_add(name_bytes))
                    .is_none_or(|end| end > storage.len())
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "directory entry name exceeds enumeration buffer",
                    ));
                }
                let name = unsafe {
                    std::slice::from_raw_parts(
                        storage.as_ptr().add(offset + name_offset).cast::<u16>(),
                        name_len,
                    )
                };
                if name != [b'.' as u16] && name != [b'.' as u16, b'.' as u16] {
                    names.push(OsString::from_wide(name));
                }
                if info.next_entry_offset == 0 {
                    break;
                }
                offset = offset
                    .checked_add(info.next_entry_offset as usize)
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "directory entry offset overflow",
                        )
                    })?;
            }
        }
    }

    pub(super) fn remove_tree_contents(directory: &File) -> io::Result<()> {
        for name in child_names(directory)? {
            let entry = open_relative(
                directory,
                &name,
                FILE_OPEN,
                0,
                (FILE_LIST_DIRECTORY
                    | FILE_READ_ATTRIBUTES
                    | FILE_TRAVERSE
                    | FILE_WRITE_DATA
                    | DELETE
                    | SYNCHRONIZE)
                    .0,
                None,
            )?;
            let information = file_information(&entry)?;
            let is_directory = information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
            let is_reparse = information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0;
            if is_directory && !is_reparse {
                remove_tree_contents(&entry)?;
            }
            delete_by_handle(&entry)?;
        }
        Ok(())
    }

    pub(super) fn remove_empty_child_dir(parent: &File, name: &OsStr) -> io::Result<()> {
        let file = open_relative(
            parent,
            name,
            FILE_OPEN,
            FILE_DIRECTORY_FILE,
            (DELETE | SYNCHRONIZE).0,
            None,
        )?;
        delete_by_handle(&file)
    }

    pub(super) fn remove_marker(parent: &File, name: &OsStr) -> io::Result<()> {
        let file = open_relative(
            parent,
            name,
            FILE_OPEN,
            FILE_NON_DIRECTORY_FILE,
            (DELETE | SYNCHRONIZE).0,
            None,
        )?;
        delete_by_handle(&file)
    }

    pub(super) fn remove_retained_child_dir(
        _parent: &File,
        _name: &OsStr,
        retained_child: &File,
    ) -> io::Result<()> {
        delete_by_handle(retained_child)
    }

    pub(super) fn rename_child_dir_no_replace(
        source_parent: &File,
        source_name: &OsStr,
        target_parent: &File,
        target_name: &OsStr,
    ) -> io::Result<()> {
        let source = open_relative(
            source_parent,
            source_name,
            FILE_OPEN,
            FILE_DIRECTORY_FILE,
            (DELETE | FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES | SYNCHRONIZE).0,
            None,
        )?;
        rename_retained_handle_no_replace(&source, target_parent, target_name)
    }

    fn fill_rename_info(
        storage: &mut [MaybeUninit<FILE_RENAME_INFO>],
        wide: &[u16],
        root: HANDLE,
        flags: u32,
        replace_if_exists: bool,
    ) -> u32 {
        let header_len = offset_of!(FILE_RENAME_INFO, FileName);
        let name_bytes = ((wide.len() - 1) * 2) as u32;
        let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        unsafe {
            if flags != 0 {
                (*info).Anonymous.Flags = flags;
            } else {
                (*info).Anonymous.ReplaceIfExists = replace_if_exists;
            }
            (*info).RootDirectory = root;
            (*info).FileNameLength = name_bytes;
            std::ptr::copy_nonoverlapping(
                wide.as_ptr().cast::<u8>(),
                storage.as_mut_ptr().cast::<u8>().add(header_len),
                wide.len() * 2,
            );
        }
        (header_len + wide.len() * 2) as u32
    }

    fn allocate_rename_storage(wide: &[u16]) -> io::Result<Vec<MaybeUninit<FILE_RENAME_INFO>>> {
        let header_len = offset_of!(FILE_RENAME_INFO, FileName);
        let total_len = header_len
            .checked_add(wide.len().saturating_mul(2))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "child name is too long"))?;
        let slot_count = total_len.div_ceil(size_of::<FILE_RENAME_INFO>());
        let mut storage = Vec::<MaybeUninit<FILE_RENAME_INFO>>::new();
        storage.try_reserve_exact(slot_count).map_err(|error| {
            io::Error::new(
                io::ErrorKind::OutOfMemory,
                format!("failed to allocate rename information: {error}"),
            )
        })?;
        storage.resize_with(slot_count, MaybeUninit::zeroed);
        Ok(storage)
    }

    fn destination_extended_dos_path(parent: &File, name: &OsStr) -> io::Result<Vec<u16>> {
        let mut buffer = vec![0_u16; 512];
        let mut chars = unsafe {
            GetFinalPathNameByHandleW(
                HANDLE(parent.as_raw_handle()),
                &mut buffer,
                FILE_NAME_NORMALIZED,
            )
        };
        if chars == 0 {
            return Err(io::Error::last_os_error());
        }
        if chars as usize > buffer.len() {
            buffer.resize(chars as usize, 0);
            chars = unsafe {
                GetFinalPathNameByHandleW(
                    HANDLE(parent.as_raw_handle()),
                    &mut buffer,
                    FILE_NAME_NORMALIZED,
                )
            };
            if chars == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        buffer.truncate(chars as usize);
        if buffer.last() != Some(&(b'\\' as u16)) {
            buffer.push(b'\\' as u16);
        }
        buffer.extend(name.encode_wide());
        buffer.push(0);
        Ok(buffer)
    }

    fn enable_rename_privileges() {
        let _ = crate::util::secure_file::enable_windows_privilege(windows::core::w!(
            "SeBackupPrivilege"
        ));
        let _ = crate::util::secure_file::enable_windows_privilege(windows::core::w!(
            "SeRestorePrivilege"
        ));
    }

    fn rename_retained_handle_no_replace(
        source: &File,
        target_parent: &File,
        target_name: &OsStr,
    ) -> io::Result<()> {
        enable_rename_privileges();
        let mut last_error = None;
        for attempt in 0..5_u32 {
            match rename_retained_handle_no_replace_once(source, target_parent, target_name) {
                Ok(()) => return Ok(()),
                Err(error) if destination_name_exists(target_parent, target_name) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "no-replace rename collided",
                    ));
                }
                Err(error) if rename_access_denied(&error) && attempt + 1 < 5 => {
                    last_error = Some(error);
                    std::thread::sleep(std::time::Duration::from_millis(10 << attempt));
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "no-replace rename failed after retries",
            )
        }))
    }

    fn rename_access_denied(error: &io::Error) -> bool {
        error.kind() == io::ErrorKind::PermissionDenied
            || error.raw_os_error() == Some(5)
            || error.to_string().contains("0xc0000022")
    }

    fn rename_retained_handle_no_replace_once(
        source: &File,
        target_parent: &File,
        target_name: &OsStr,
    ) -> io::Result<()> {
        if destination_name_exists(target_parent, target_name) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "no-replace rename collided",
            ));
        }

        let mut relative: Vec<u16> = target_name.encode_wide().collect();
        relative.push(0);
        let mut relative_storage = allocate_rename_storage(&relative)?;
        let relative_len = fill_rename_info(
            &mut relative_storage,
            &relative,
            HANDLE(target_parent.as_raw_handle()),
            FILE_RENAME_POSIX_REPLACE,
            false,
        );
        let mut status_block = IoStatusBlock {
            status_or_pointer: 0,
            information: 0,
        };
        let posix = unsafe {
            NtSetInformationFile(
                HANDLE(source.as_raw_handle()),
                &mut status_block,
                relative_storage.as_ptr().cast(),
                relative_len,
                FILE_RENAME_INFORMATION_EX,
            )
        };
        if posix >= 0 {
            return Ok(());
        }
        if nt_status_is_no_replace_collision(posix)
            || destination_name_exists(target_parent, target_name)
        {
            return Err(map_nt_no_replace_error(posix, target_parent, target_name));
        }

        if let Ok(extended) = destination_extended_dos_path(target_parent, target_name) {
            let mut extended_storage = allocate_rename_storage(&extended)?;
            let extended_len = fill_rename_info(
                &mut extended_storage,
                &extended,
                HANDLE::default(),
                FILE_RENAME_POSIX_REPLACE,
                false,
            );
            if unsafe {
                SetFileInformationByHandle(
                    HANDLE(source.as_raw_handle()),
                    FileRenameInfoEx,
                    extended_storage.as_ptr().cast(),
                    extended_len,
                )
            }
            .is_ok()
            {
                return Ok(());
            }
            if destination_name_exists(target_parent, target_name) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "no-replace rename collided",
                ));
            }
        }

        let classic = fill_rename_info(
            &mut relative_storage,
            &relative,
            HANDLE(target_parent.as_raw_handle()),
            0,
            false,
        );
        if unsafe {
            SetFileInformationByHandle(
                HANDLE(source.as_raw_handle()),
                windows::Win32::Storage::FileSystem::FileRenameInfo,
                relative_storage.as_ptr().cast(),
                classic,
            )
        }
        .is_ok()
        {
            return Ok(());
        }

        let status = unsafe {
            NtSetInformationFile(
                HANDLE(source.as_raw_handle()),
                &mut status_block,
                relative_storage.as_ptr().cast(),
                classic,
                FILE_RENAME_INFORMATION,
            )
        };
        if status >= 0 {
            Ok(())
        } else {
            let mut fs_rename_err = None;
            if let (Ok(source_path_raw), Ok(target_path_raw)) = (
                file_extended_dos_path(source),
                destination_extended_dos_path(target_parent, target_name),
            ) {
                let mut s_slice = &source_path_raw[..source_path_raw.len().saturating_sub(1)];
                let mut t_slice = &target_path_raw[..target_path_raw.len().saturating_sub(1)];
                if s_slice.starts_with(&[b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16]) {
                    s_slice = &s_slice[4..];
                }
                if t_slice.starts_with(&[b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16]) {
                    t_slice = &t_slice[4..];
                }
                let source_path = std::ffi::OsString::from_wide(s_slice);
                let target_path = std::ffi::OsString::from_wide(t_slice);
                match std::fs::rename(&source_path, &target_path) {
                    Ok(()) => return Ok(()),
                    Err(e) => {
                        fs_rename_err = Some(format!("std::fs::rename({source_path:?}, {target_path:?}) err={e:?}"));
                    }
                }
            }
            let mut err = map_nt_no_replace_error(status, target_parent, target_name);
            if let Some(msg) = fs_rename_err {
                err = io::Error::new(err.kind(), format!("{err}; {msg}"));
            }
            Err(err)
        }
    }

    fn file_extended_dos_path(file: &File) -> io::Result<Vec<u16>> {
        let mut buffer = vec![0_u16; 512];
        let mut chars = unsafe {
            GetFinalPathNameByHandleW(
                HANDLE(file.as_raw_handle()),
                &mut buffer,
                FILE_NAME_NORMALIZED,
            )
        };
        if chars == 0 {
            return Err(io::Error::last_os_error());
        }
        if chars as usize > buffer.len() {
            buffer.resize(chars as usize, 0);
            chars = unsafe {
                GetFinalPathNameByHandleW(
                    HANDLE(file.as_raw_handle()),
                    &mut buffer,
                    FILE_NAME_NORMALIZED,
                )
            };
            if chars == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        buffer.truncate(chars as usize);
        buffer.push(0);
        Ok(buffer)
    }

    pub(super) fn nt_status_is_no_replace_collision(status: i32) -> bool {
        const ERROR_FILE_EXISTS: u32 = 80;
        const ERROR_DIR_NOT_EMPTY: u32 = 145;
        const ERROR_ALREADY_EXISTS: u32 = 183;
        const ERROR_OBJECT_NAME_COLLISION: u32 = 698;
        if matches!(
            status,
            STATUS_OBJECT_NAME_COLLISION | STATUS_DIRECTORY_NOT_EMPTY
        ) {
            return true;
        }
        matches!(
            unsafe { RtlNtStatusToDosError(status) },
            ERROR_FILE_EXISTS
                | ERROR_DIR_NOT_EMPTY
                | ERROR_ALREADY_EXISTS
                | ERROR_OBJECT_NAME_COLLISION
        )
    }

    fn destination_name_exists(parent: &File, name: &OsStr) -> bool {
        match open_relative(
            parent,
            name,
            FILE_OPEN,
            0,
            (FILE_READ_ATTRIBUTES | SYNCHRONIZE).0,
            None,
        ) {
            Ok(_) => true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(_) => match destination_extended_dos_path(parent, name) {
                Ok(wide) => {
                    let path = std::ffi::OsString::from_wide(&wide[..wide.len().saturating_sub(1)]);
                    std::path::Path::new(&path).try_exists().unwrap_or(true)
                }
                // Occupied names we cannot resolve still block no-replace.
                Err(_) => true,
            },
        }
    }

    fn map_nt_no_replace_error(
        status: i32,
        target_parent: &File,
        target_name: &OsStr,
    ) -> io::Error {
        // A collision NTSTATUS is enough. If Windows instead returns an
        // unhelpful status, the no-replace contract is still "dest already
        // occupies that name".
        if nt_status_is_no_replace_collision(status)
            || destination_name_exists(target_parent, target_name)
        {
            return io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("no-replace rename collided (ntstatus={status:#010x})"),
            );
        }
        let win32 = unsafe { RtlNtStatusToDosError(status) };
        io::Error::new(
            io::ErrorKind::Other,
            format!("no-replace rename failed (ntstatus={status:#010x}, win32={win32})"),
        )
    }

    pub(super) fn rename_retained_child_dir_no_replace(
        source_parent: &File,
        source_name: &OsStr,
        retained_child: &File,
        target_parent: &File,
        target_name: &OsStr,
    ) -> io::Result<()> {
        match rename_retained_handle_no_replace(retained_child, target_parent, target_name) {
            Ok(()) => Ok(()),
            Err(e) if rename_access_denied(&e) => {
                if let Ok(source) = open_relative(
                    source_parent,
                    source_name,
                    FILE_OPEN,
                    FILE_DIRECTORY_FILE,
                    (DELETE | FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES | FILE_LIST_DIRECTORY | FILE_TRAVERSE | SYNCHRONIZE).0,
                    None,
                ) {
                    if rename_retained_handle_no_replace(&source, target_parent, target_name).is_ok() {
                        return Ok(());
                    }
                }
                Err(e)
            }
            Err(e) => Err(e),
        }
    }
}

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
    fn owner_only_lock_file_is_private_read_write_and_retained_parent_anchored() {
        use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};

        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        let retained_path = temp.path().join("retained");
        let outside = temp.path().join("outside");
        std::fs::create_dir(&root_path).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let root = AnchoredDirectory::open_root(&root_path).unwrap();

        std::fs::rename(&root_path, &retained_path).unwrap();
        std::fs::write(retained_path.join("session.lock"), b"").unwrap();
        std::fs::set_permissions(
            retained_path.join("session.lock"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        symlink(&outside, &root_path).unwrap();

        let mut lock = root
            .open_or_create_owner_only_child_file(OsStr::new("session.lock"))
            .unwrap();
        lock.write_all(b"lock").unwrap();
        lock.seek(SeekFrom::Start(0)).unwrap();
        let mut contents = Vec::new();
        lock.read_to_end(&mut contents).unwrap();
        assert_eq!(contents, b"lock");
        let metadata = lock.metadata().unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(metadata.nlink(), 1);
        assert!(retained_path.join("session.lock").is_file());
        assert!(!outside.join("session.lock").exists());
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
    #[ignore = "spawned by owner_only_lock_file_creation_ignores_permissive_umask"]
    fn subprocess_owner_only_lock_file_under_umask_zero() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let Ok(root_path) = std::env::var("GROK_TEST_OWNER_ONLY_LOCK_ROOT") else {
            return;
        };
        unsafe {
            libc::umask(0);
        }
        let root = AnchoredDirectory::open_root(std::path::Path::new(&root_path)).unwrap();
        let lock = root
            .open_or_create_owner_only_child_file(OsStr::new("session.lock"))
            .unwrap();
        let metadata = lock.metadata().unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
        assert_eq!(metadata.nlink(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn owner_only_lock_file_creation_ignores_permissive_umask() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        std::fs::create_dir(&root_path).unwrap();

        #[allow(clippy::disallowed_methods)] // isolated native umask probe
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "--nocapture",
                "util::anchored_directory::tests::subprocess_owner_only_lock_file_under_umask_zero",
            ])
            .env("GROK_TEST_OWNER_ONLY_LOCK_ROOT", &root_path)
            .status()
            .expect("spawn owner-only lock subprocess");
        assert!(status.success(), "umask subprocess failed: {status}");
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

        std::fs::create_dir(source_path.join("fresh")).unwrap();
        source
            .rename_child_dir_no_replace(OsStr::new("fresh"), &target, OsStr::new("published"))
            .unwrap();
        assert!(!source_path.join("fresh").exists());
        assert!(target_path.join("published").is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn retained_child_rename_remove_files_and_identity_are_anchored() {
        use std::io::{Read as _, Write as _};

        let temp = tempfile::tempdir().unwrap();
        let source_path = temp.path().join("source");
        let target_path = temp.path().join("target");
        std::fs::create_dir(&source_path).unwrap();
        std::fs::create_dir(&target_path).unwrap();
        let source = AnchoredDirectory::open_root(&source_path).unwrap();
        let target = AnchoredDirectory::open_root(&target_path).unwrap();
        let child = source.create_child_dir(OsStr::new("stage")).unwrap();
        let reopened = source.open_child_dir(OsStr::new("stage")).unwrap();
        assert!(child.same_identity(&reopened).unwrap());
        assert!(!child.same_identity(&target).unwrap());

        let mut marker = child
            .create_child_file_new(OsStr::new(".unpublished"))
            .unwrap();
        marker.write_all(b"marker").unwrap();
        drop(marker);
        assert_eq!(
            child
                .create_child_file_new(OsStr::new(".unpublished"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        let mut marker = child.open_child_file(OsStr::new(".unpublished")).unwrap();
        let mut contents = Vec::new();
        marker.read_to_end(&mut contents).unwrap();
        assert_eq!(contents, b"marker");
        drop(marker);
        child.remove_marker(OsStr::new(".unpublished")).unwrap();

        let published = child
            .rename_self_no_replace(&target, OsStr::new("published"))
            .unwrap();
        assert!(!source_path.join("stage").exists());
        let public_path = target.open_child_dir(OsStr::new("published")).unwrap();
        assert!(published.same_identity(&public_path).unwrap());
        drop(public_path);
        published.remove_self().unwrap();
        assert!(!target_path.join("published").exists());
    }

    #[test]
    fn failed_retained_rename_returns_the_live_source_anchor() {
        let temp = tempfile::tempdir().unwrap();
        let source_path = temp.path().join("source-retained-error");
        let target_path = temp.path().join("target-retained-error");
        std::fs::create_dir(&source_path).unwrap();
        std::fs::create_dir(&target_path).unwrap();
        let source = AnchoredDirectory::open_root(&source_path).unwrap();
        let target = AnchoredDirectory::open_root(&target_path).unwrap();
        let retained = source.create_child_dir(OsStr::new("stage")).unwrap();
        target.create_child_dir(OsStr::new("winner")).unwrap();

        let failure = retained
            .try_rename_self_no_replace(&target, OsStr::new("winner"))
            .unwrap_err();
        assert_eq!(failure.error.kind(), io::ErrorKind::AlreadyExists);
        let reopened = source.open_child_dir(OsStr::new("stage")).unwrap();
        assert!(failure.source.same_identity(&reopened).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn retained_child_remove_rejects_replaced_source_identity() {
        let temp = tempfile::tempdir().unwrap();
        let parent_path = temp.path().join("parent");
        std::fs::create_dir(&parent_path).unwrap();
        let parent = AnchoredDirectory::open_root(&parent_path).unwrap();
        let retained = parent.create_child_dir(OsStr::new("child")).unwrap();
        std::fs::rename(parent_path.join("child"), parent_path.join("detached")).unwrap();
        std::fs::create_dir(parent_path.join("child")).unwrap();

        assert_eq!(
            retained.remove_self().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(parent_path.join("child").is_dir());
        assert!(parent_path.join("detached").is_dir());

        let retained = parent.create_child_dir(OsStr::new("rename-child")).unwrap();
        std::fs::rename(
            parent_path.join("rename-child"),
            parent_path.join("rename-detached"),
        )
        .unwrap();
        std::fs::create_dir(parent_path.join("rename-child")).unwrap();
        let target_path = temp.path().join("target");
        std::fs::create_dir(&target_path).unwrap();
        let target = AnchoredDirectory::open_root(&target_path).unwrap();
        assert_eq!(
            retained
                .try_rename_self_no_replace(&target, OsStr::new("published"))
                .unwrap_err()
                .error
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(parent_path.join("rename-child").is_dir());
        assert!(parent_path.join("rename-detached").is_dir());
        assert!(!target_path.join("published").exists());
    }

    #[cfg(unix)]
    #[test]
    fn retained_tree_removal_does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let parent_path = temp.path().join("parent");
        let outside_path = temp.path().join("outside");
        std::fs::create_dir(&parent_path).unwrap();
        std::fs::create_dir(&outside_path).unwrap();
        std::fs::write(outside_path.join("sentinel"), b"preserve").unwrap();
        let parent = AnchoredDirectory::open_root(&parent_path).unwrap();
        let tree = parent.create_child_dir(OsStr::new("tree")).unwrap();
        let nested = tree.create_child_dir(OsStr::new("nested")).unwrap();
        std::fs::write(parent_path.join("tree/nested/data"), b"data").unwrap();
        drop(nested);
        symlink(&outside_path, parent_path.join("tree/link")).unwrap();

        let first = tree.child_names().unwrap();
        let second = tree.child_names().unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(first.len(), second.len(), "enumeration must be repeatable");
        tree.remove_tree_self().unwrap();

        assert!(!parent_path.join("tree").exists());
        assert_eq!(
            std::fs::read(outside_path.join("sentinel")).unwrap(),
            b"preserve"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_retained_handle_survives_root_rename_and_replacement_by_identity() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        let moved_path = temp.path().join("moved");
        std::fs::create_dir(&root_path).unwrap();
        let root = AnchoredDirectory::open_root(&root_path).unwrap();
        root.ensure_owner_only().unwrap();
        std::fs::rename(&root_path, &moved_path).unwrap();
        std::fs::create_dir(&root_path).unwrap();
        let replacement = AnchoredDirectory::open_root(&root_path).unwrap();
        let moved = AnchoredDirectory::open_root(&moved_path).unwrap();
        assert!(root.same_identity(&moved).unwrap());
        assert!(!root.same_identity(&replacement).unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn windows_no_replace_collision_ntstatus_is_already_exists() {
        assert!(platform::nt_status_is_no_replace_collision(
            0xC000_0035_u32 as i32
        ));
        assert!(platform::nt_status_is_no_replace_collision(
            0xC000_0101_u32 as i32
        ));
        assert!(!platform::nt_status_is_no_replace_collision(
            0xC000_000D_u32 as i32
        ));
    }

    #[cfg(windows)]
    #[test]
    fn windows_child_creation_and_no_replace_are_handle_relative() {
        let temp = tempfile::tempdir().unwrap();
        let source_path = temp.path().join("source");
        let target_path = temp.path().join("target");
        std::fs::create_dir(&source_path).unwrap();
        std::fs::create_dir(&target_path).unwrap();
        let source = AnchoredDirectory::open_root(&source_path).unwrap();
        let target = AnchoredDirectory::open_root(&target_path).unwrap();
        source.create_child_dir(OsStr::new("session")).unwrap();
        target.create_child_dir(OsStr::new("session")).unwrap();
        std::fs::write(source_path.join("marker"), b"marker").unwrap();
        source.remove_marker(OsStr::new("marker")).unwrap();
        assert!(!source_path.join("marker").exists());
        let collision = source
            .rename_child_dir_no_replace(OsStr::new("session"), &target, OsStr::new("session"))
            .unwrap_err();
        assert_eq!(
            collision.kind(),
            io::ErrorKind::AlreadyExists,
            "no-replace collision should be AlreadyExists: {collision}"
        );
        assert!(source.open_child_dir(OsStr::new("session")).is_ok());
        assert!(target.open_child_dir(OsStr::new("session")).is_ok());

        let retained = source.create_child_dir(OsStr::new("retained")).unwrap();
        let reopened = source.open_child_dir(OsStr::new("retained")).unwrap();
        assert!(retained.same_identity(&reopened).unwrap());
        drop(reopened);
        let published = retained
            .rename_self_no_replace(&target, OsStr::new("published"))
            .unwrap();
        let public_path = target.open_child_dir(OsStr::new("published")).unwrap();
        assert!(published.same_identity(&public_path).unwrap());
        drop(public_path);
        published.remove_self().unwrap();
        assert!(!target_path.join("published").exists());

        use std::io::{Read as _, Write as _};
        let mut marker = source.create_child_file_new(OsStr::new("marker")).unwrap();
        marker.write_all(b"marker").unwrap();
        drop(marker);
        assert!(source.create_child_file_new(OsStr::new("marker")).is_err());
        let mut marker = source.open_child_file(OsStr::new("marker")).unwrap();
        let mut contents = Vec::new();
        marker.read_to_end(&mut contents).unwrap();
        assert_eq!(contents, b"marker");
    }

    #[cfg(windows)]
    #[test]
    fn windows_open_root_and_child_reject_reparse_points() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        let root_path = temp.path().join("root");
        let junction = root_path.join("junction");
        std::fs::create_dir(&real).unwrap();
        std::fs::create_dir(&root_path).unwrap();
        let mut command = std::process::Command::new("cmd");
        command
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&real);
        xai_tty_utils::detach_std_command(&mut command);
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mklink failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        assert!(AnchoredDirectory::open_root(&junction).is_err());
        let root = AnchoredDirectory::open_root(&root_path).unwrap();
        assert!(root.open_child_dir(OsStr::new("junction")).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_owner_only_lock_file_blocks_rename_and_delete_while_held() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        std::fs::create_dir(&root_path).unwrap();
        let root = AnchoredDirectory::open_root(&root_path).unwrap();
        let lock = root
            .open_or_create_owner_only_child_file(OsStr::new("session.lock"))
            .unwrap();

        let lock_path = root_path.join("session.lock");
        let renamed_path = root_path.join("renamed.lock");
        assert!(std::fs::rename(&lock_path, &renamed_path).is_err());
        assert!(std::fs::remove_file(&lock_path).is_err());
        drop(lock);
        std::fs::rename(&lock_path, &renamed_path).unwrap();
        std::fs::remove_file(renamed_path).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_owner_only_lock_file_rejects_reparse_leaf_and_has_protected_user_dacl() {
        use std::os::windows::fs::symlink_file;

        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        let outside = temp.path().join("outside");
        std::fs::create_dir(&root_path).unwrap();
        std::fs::write(&outside, b"sentinel").unwrap();
        let root = AnchoredDirectory::open_root(&root_path).unwrap();
        // Native file symlinks require Developer Mode or SeCreateSymbolicLink
        // privilege on some Windows runners. Keep the DACL assertion below
        // unconditional, and exercise the reparse-leaf rejection whenever the
        // host can create the fixture rather than failing the whole lane for a
        // missing test-host privilege.
        if symlink_file(&outside, root_path.join("symlink.lock")).is_ok() {
            assert!(
                root.open_or_create_owner_only_child_file(OsStr::new("symlink.lock"))
                    .is_err(),
                "a reparse-point lock leaf must be rejected"
            );
            assert_eq!(std::fs::read(&outside).unwrap(), b"sentinel");
        }

        let lock = root
            .open_or_create_owner_only_child_file(OsStr::new("session.lock"))
            .unwrap();
        assert!(
            platform::owner_only_lock_file_security_is_current_user_protected(&lock).unwrap(),
            "new lock leaf must have a protected current-user DACL with no foreign allow ACEs"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_retained_tree_removal_does_not_follow_junctions() {
        let temp = tempfile::tempdir().unwrap();
        let parent_path = temp.path().join("parent");
        let outside_path = temp.path().join("outside");
        std::fs::create_dir(&parent_path).unwrap();
        std::fs::create_dir(&outside_path).unwrap();
        std::fs::write(outside_path.join("sentinel"), b"preserve").unwrap();
        let parent = AnchoredDirectory::open_root(&parent_path).unwrap();
        let tree = parent.create_child_dir(OsStr::new("tree")).unwrap();
        tree.create_child_dir(OsStr::new("nested")).unwrap();
        std::fs::write(
            parent_path.join("tree").join("nested").join("data"),
            b"data",
        )
        .unwrap();
        // `join("tree/junction")` keeps a `/junction` component on Windows;
        // mklink then treats `/junction` as a switch ("Parameter format not
        // correct - \"junction\"").
        let junction = parent_path.join("tree").join("junc");
        let mut command = std::process::Command::new("cmd");
        command
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&outside_path);
        xai_tty_utils::detach_std_command(&mut command);
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mklink failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        assert_eq!(
            tree.child_names()
                .expect("listing a tree that contains a junction")
                .len(),
            2
        );
        assert_eq!(
            tree.child_names()
                .expect("junction listing must be repeatable")
                .len(),
            2
        );
        tree.remove_tree_self()
            .expect("remove_tree_self must delete the junction leaf without walking it");

        assert!(!parent_path.join("tree").exists());
        assert_eq!(
            std::fs::read(outside_path.join("sentinel")).unwrap(),
            b"preserve"
        );
    }

    #[test]
    fn reproduce_staging_to_sessions_rename() {
        let temp = tempfile::tempdir().unwrap();
        let root = AnchoredDirectory::open_root(temp.path()).unwrap();
        let private = root.create_child_dir(OsStr::new(".private")).unwrap();
        let staging = private.create_child_dir(OsStr::new("session-staging")).unwrap();
        let container = staging.create_child_dir(OsStr::new("session-container")).unwrap();
        let session = container.create_child_dir(OsStr::new("session-id")).unwrap();
        let marker = session.create_child_file_new(OsStr::new(".unpublished")).unwrap();
        drop(marker);
        let f1 = session.create_child_file_new(OsStr::new("summary.json")).unwrap();
        drop(f1);
        let f2 = session.create_child_file_new(OsStr::new("prompt_context.json")).unwrap();
        drop(f2);
        let f3 = session.create_child_file_new(OsStr::new("system_prompt.txt")).unwrap();
        drop(f3);
        let f4 = session.create_child_file_new(OsStr::new("chat_history.jsonl")).unwrap();
        drop(f4);
        
        let session_path = temp.path().join(".private").join("session-staging").join("session-container").join("session-id");
        let mut opts = std::fs::OpenOptions::new();
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;
            opts.share_mode(0x0000_0007);
        }
        let events_f = opts.create(true).append(true).open(session_path.join("events.jsonl")).unwrap();
        
        drop(events_f);
        drop(session);
        let sessions = root.create_child_dir(OsStr::new("sessions")).unwrap();
        let published_parent = container
            .try_rename_self_no_replace(&sessions, OsStr::new("cwd"))
            .unwrap();
        let session = published_parent.open_child_dir(OsStr::new("session-id")).unwrap();
        assert!(session.child_names().unwrap().contains(&OsString::from(".unpublished")));
    }
}
