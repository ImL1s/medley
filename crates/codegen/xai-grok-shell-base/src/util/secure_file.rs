//! Cross-platform secure file operations.
//!
//! This module provides utilities for creating files with restrictive permissions
//! that limit access to the current user only. This is critical for storing
//! sensitive data like authentication tokens.
//!
//! ## Security Model
//!
//! - **Unix**: Files are created with mode 0o600 (owner read/write only)
//! - **Windows**: Files are created with ACLs that grant access only to the current user
//!
//! ## Encryption Consideration
//!
//! While this module restricts file access at the OS level, the token is stored in
//! plaintext. For additional security in high-risk environments, consider:
//! - Using the operating system's keychain/credential manager (e.g., macOS Keychain,
//!   Windows Credential Manager, Linux Secret Service)
//! - Encrypting the token with a key derived from system-specific entropy
//!
//! The current approach balances security with simplicity - OS file permissions
//! provide reasonable protection for most use cases, and the token is already
//! short-lived (7-30 days TTL with automatic refresh).

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

/// Creates or opens a file with secure permissions (owner read/write only).
///
/// On Unix, this sets mode 0o600. On Windows, this restricts the file's ACL
/// to grant access only to the current user.
///
/// # Arguments
/// * `path` - The path to the file to create/open
/// * `contents` - The data to write to the file
///
/// # Returns
/// An `io::Result<()>` indicating success or failure.
///
/// # Example
/// ```ignore
/// use xai_grok_shell_base::util::secure_file::write_secure_file;
///
/// let token = "secret_token";
/// write_secure_file("/path/to/auth.json", token.as_bytes())?;
/// ```
pub fn write_secure_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Create the file with secure permissions
    let mut file = open_secure_file(path)?;
    file.write_all(contents)?;
    file.flush()?;

    // Re-assert owner-only bits: `OpenOptions::mode` only applies on create,
    // so an existing world-readable file would otherwise keep open perms.
    ensure_owner_only_permissions(path)?;

    Ok(())
}

/// Opens a file for writing with secure permissions set during creation (Unix)
/// or prepares it for permission setting after creation (Windows).
///
/// Callers that write secret material should also call
/// [`ensure_owner_only_permissions`] after the write (or use
/// [`write_secure_file`]), because `mode(0o600)` only applies when the file
/// is newly created — not when truncating an existing path.
pub fn open_secure_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.truncate(true).write(true).create(true);

    #[cfg(unix)]
    {
        // Set file mode to 0o600 (owner read/write only) during creation
        options.mode(0o600);
    }

    options.open(path)
}

/// Ensure `path` is owner-read/write only (Unix `0o600` / Windows user ACL).
///
/// Best-effort on missing files (`NotFound` is ignored). Other errors
/// propagate so callers can fail closed when tightening a secret store.
///
/// Use on **load** of credential files so a hand-copied or restored
/// world-readable `auth.json` is tightened before the process continues.
pub fn ensure_owner_only_permissions(path: &Path) -> io::Result<()> {
    match ensure_owner_only_permissions_inner(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Ensure a directory is accessible only by the current user.
///
/// Unix directories require execute permission for traversal, so this uses
/// `0o700` rather than the `0o600` file mode. Windows uses the same protected
/// owner-only ACL as secure files.
pub fn ensure_owner_only_directory_permissions(path: &Path) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "owner-only directory path is not a real directory",
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "owner-only directory path is a reparse point",
            ));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "owner-only directory is not owned by the current user",
            ));
        }
        let mode = metadata.permissions().mode();
        if mode & 0o777 != 0o700 {
            let mut permissions = metadata.permissions();
            permissions.set_mode(0o700);
            std::fs::set_permissions(path, permissions)?;
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        secure_windows_directory_on_handle(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(())
    }
}

/// Ensure the object referenced by an already-open file handle is owner-only.
///
/// Strict credential writers use this after exclusive creation and before
/// writing any secret bytes. Using the handle avoids proving permissions on a
/// different object if the path is replaced between creation and validation.
pub fn ensure_owner_only_permissions_for_file(file: &File) -> io::Result<()> {
    #[cfg(unix)]
    {
        let metadata = file.metadata()?;
        let mode = metadata.permissions().mode();
        if mode & 0o777 != 0o600 {
            let mut permissions = metadata.permissions();
            permissions.set_mode(0o600);
            file.set_permissions(permissions)?;
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        set_windows_secure_permissions_for_file(file)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        Ok(())
    }
}

fn ensure_owner_only_permissions_inner(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        let metadata = std::fs::metadata(path)?;
        let mode = metadata.permissions().mode();
        if mode & 0o777 != 0o600 {
            let mut perms = metadata.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(path, perms)?;
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        set_windows_secure_permissions(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(())
    }
}

/// Sets Windows-specific secure permissions on a file.
///
/// This function modifies the file's ACL to:
/// 1. Remove inherited permissions
/// 2. Grant full control only to the current user
///
/// This is equivalent to Unix mode 0o600.
#[cfg(windows)]
pub fn set_windows_secure_permissions(path: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Security::Authorization::{SE_FILE_OBJECT, SetNamedSecurityInfoW};
    use windows::Win32::Security::{
        DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
    };
    use windows::core::PCWSTR;

    let wide_path: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    with_windows_owner_only_acl(windows::Win32::Security::ACE_FLAGS(0), |new_acl| unsafe {
        SetNamedSecurityInfoW(
            PCWSTR::from_raw(wide_path.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(new_acl),
            None,
        )
    })
}

#[cfg(windows)]
fn enable_windows_privilege(name: windows::core::PCWSTR) -> io::Result<()> {
    use windows::Win32::Foundation::LUID;
    use windows::Win32::Security::{
        AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED,
        TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token = windows::Win32::Foundation::HANDLE::default();
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
        .map_err(io::Error::other)?;
        let token = WindowsHandle(token);
        let mut luid = LUID::default();
        LookupPrivilegeValueW(None, name, &mut luid).map_err(io::Error::other)?;
        let privileges = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        AdjustTokenPrivileges(token.0, false, Some(&privileges), 0, None, None)
            .map_err(io::Error::other)?;
    }
    Ok(())
}

#[cfg(windows)]
fn secure_windows_directory_on_handle(path: &Path) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::os::windows::io::AsRawHandle as _;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Security::Authorization::{SE_FILE_OBJECT, SetSecurityInfo};
    use windows::Win32::Security::{
        CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, OBJECT_INHERIT_ACE,
        OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
    };
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, GetFileInformationByHandle, READ_CONTROL, WRITE_DAC,
    };

    // Administrators tokens have these privileges assigned but disabled.
    // Taking ownership of a GHA temp directory requires them to be enabled.
    let _ = enable_windows_privilege(windows::core::w!("SeTakeOwnershipPrivilege"));
    let _ = enable_windows_privilege(windows::core::w!("SeRestorePrivilege"));

    const WRITE_OWNER: u32 = 0x0008_0000;
    let directory = OpenOptions::new()
        .access_mode((READ_CONTROL | WRITE_DAC | FILE_READ_ATTRIBUTES).0 | WRITE_OWNER)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
        .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0)
        .open(path)?;
    let handle = HANDLE(directory.as_raw_handle());
    unsafe {
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        GetFileInformationByHandle(handle, &mut info).map_err(io::Error::other)?;
        if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "owner-only directory path is a reparse point",
            ));
        }

        let current_user = current_windows_user()?;
        let current_sid = current_user.sid();
        with_windows_owner_only_acl(CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE, |new_acl| {
            SetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION
                    | DACL_SECURITY_INFORMATION
                    | PROTECTED_DACL_SECURITY_INFORMATION,
                Some(current_sid),
                None,
                Some(new_acl),
                None,
            )
        })?;
        verify_windows_directory_acl(handle, current_sid)?;
    }
    Ok(())
}

#[cfg(windows)]
fn verify_windows_directory_acl(
    handle: windows::Win32::Foundation::HANDLE,
    current_sid: windows::Win32::Security::PSID,
) -> io::Result<()> {
    use windows::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
    use windows::Win32::Security::{
        ACCESS_ALLOWED_ACE, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
        GetSecurityDescriptorControl, OBJECT_INHERIT_ACE, PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
    };

    unsafe {
        let mut dacl = std::ptr::null_mut();
        let mut raw_descriptor = PSECURITY_DESCRIPTOR::default();
        let status = GetSecurityInfo(
            handle,
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
        let _descriptor = WindowsLocalAllocation::new(raw_descriptor.0);

        let mut control = 0u16;
        let mut revision = 0u32;
        GetSecurityDescriptorControl(raw_descriptor, &mut control, &mut revision)
            .map_err(io::Error::other)?;
        let protected = control & SE_DACL_PROTECTED.0 != 0;

        let ace_count = if dacl.is_null() { 0 } else { (*dacl).AceCount };
        let required_flags = (CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE).0 as u8;
        let mut current_user_full_control = false;
        let mut foreign_allow = false;
        for index in 0..ace_count {
            let mut raw_ace = std::ptr::null_mut();
            if GetAce(dacl, u32::from(index), &mut raw_ace).is_err() || raw_ace.is_null() {
                continue;
            }
            let ace = &*(raw_ace as *const ACCESS_ALLOWED_ACE);
            if ace.Header.AceType != 0 {
                continue;
            }
            let ace_sid =
                windows::Win32::Security::PSID(std::ptr::addr_of!(ace.SidStart).cast_mut().cast());
            let full_control = ace.Mask == 0x10000000 || ace.Mask & 0x001f01ff == 0x001f01ff;
            let inherited = ace.Header.AceFlags & required_flags == required_flags;
            if EqualSid(ace_sid, current_sid).is_ok() {
                if full_control && inherited {
                    current_user_full_control = true;
                }
            } else {
                foreign_allow = true;
            }
        }
        if !protected || !current_user_full_control || foreign_allow {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "owner-only directory ACL verification failed",
            ));
        }
    }
    Ok(())
}

#[cfg(windows)]
struct WindowsHandle(windows::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for WindowsHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
struct WindowsLocalAllocation(*mut std::ffi::c_void);

#[cfg(windows)]
impl WindowsLocalAllocation {
    fn new(pointer: *mut std::ffi::c_void) -> Self {
        Self(pointer)
    }
}

#[cfg(windows)]
impl Drop for WindowsLocalAllocation {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = windows::Win32::Foundation::LocalFree(Some(
                    windows::Win32::Foundation::HLOCAL(self.0),
                ));
            }
        }
    }
}

#[cfg(windows)]
struct WindowsCurrentUser {
    // `usize` storage provides sufficient alignment for `TOKEN_USER` and its SID.
    buffer: Vec<usize>,
}

#[cfg(windows)]
impl WindowsCurrentUser {
    fn sid(&self) -> windows::Win32::Security::PSID {
        unsafe {
            (*(self.buffer.as_ptr() as *const windows::Win32::Security::TOKEN_USER))
                .User
                .Sid
        }
    }
}

#[cfg(windows)]
fn current_windows_user() -> io::Result<WindowsCurrentUser> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TokenUser};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).map_err(io::Error::other)?;
        let token = WindowsHandle(token);
        let mut length = 0;
        let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut length);
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        let word_size = std::mem::size_of::<usize>();
        let mut buffer = vec![0usize; (length as usize).div_ceil(word_size)];
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            length,
            &mut length,
        )
        .map_err(io::Error::other)?;
        Ok(WindowsCurrentUser { buffer })
    }
}

#[cfg(windows)]
fn set_windows_secure_permissions_for_file(file: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Security::Authorization::{SE_FILE_OBJECT, SetSecurityInfo};
    use windows::Win32::Security::{
        DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
    };

    let handle = HANDLE(file.as_raw_handle());
    with_windows_owner_only_acl(windows::Win32::Security::ACE_FLAGS(0), |new_acl| unsafe {
        SetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(new_acl),
            None,
        )
    })
}

#[cfg(windows)]
fn with_windows_owner_only_acl(
    inheritance: windows::Win32::Security::ACE_FLAGS,
    apply_acl: impl FnOnce(
        *mut windows::Win32::Security::ACL,
    ) -> windows::Win32::Foundation::WIN32_ERROR,
) -> io::Result<()> {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{
        EXPLICIT_ACCESS_W, SET_ACCESS, SetEntriesInAclW, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
    };
    use windows::Win32::Security::{
        ACE_FLAGS, ACL, GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        // Get current process token
        let mut raw_token = windows::Win32::Foundation::HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token)
            .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e))?;
        let token_handle = WindowsHandle(raw_token);

        // Get token user size
        let mut return_length = 0u32;
        let _ = GetTokenInformation(token_handle.0, TokenUser, None, 0, &mut return_length);
        if return_length == 0 {
            return Err(io::Error::last_os_error());
        }

        // Get token user (current user's SID)
        let word_size = std::mem::size_of::<usize>();
        let mut token_user_buffer = vec![0usize; (return_length as usize).div_ceil(word_size)];
        GetTokenInformation(
            token_handle.0,
            TokenUser,
            Some(token_user_buffer.as_mut_ptr() as *mut _),
            return_length,
            &mut return_length,
        )
        .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e))?;

        // The TOKEN_USER structure starts with a SID_AND_ATTRIBUTES which has PSID as first field
        let token_user = &*(token_user_buffer.as_ptr() as *const TOKEN_USER);
        let user_sid = token_user.User.Sid;

        // Create explicit access entry for current user only
        // GENERIC_ALL = 0x10000000
        let explicit_access = EXPLICIT_ACCESS_W {
            grfAccessPermissions: 0x10000000, // GENERIC_ALL
            grfAccessMode: SET_ACCESS,
            grfInheritance: inheritance,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation:
                    windows::Win32::Security::Authorization::NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_USER,
                ptstrName: windows::core::PWSTR(user_sid.0 as *mut u16),
            },
        };

        // Create new ACL with only this entry
        let mut new_acl: *mut ACL = std::ptr::null_mut();
        let result = SetEntriesInAclW(Some(&[explicit_access]), None, &mut new_acl);
        if result.0 != 0 {
            return Err(io::Error::from_raw_os_error(result.0 as i32));
        }

        let result = apply_acl(new_acl);

        // Clean up
        let _ = LocalFree(Some(HLOCAL(new_acl as *mut _)));

        if result.0 != 0 {
            return Err(io::Error::from_raw_os_error(result.0 as i32));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_write_secure_file_creates_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("test_secure.txt");

        write_secure_file(&file_path, b"test content").unwrap();

        assert!(file_path.exists());
        let content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "test content");
    }

    #[test]
    fn test_write_secure_file_creates_parent_dirs() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("nested").join("dir").join("test.txt");

        write_secure_file(&file_path, b"nested content").unwrap();

        assert!(file_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn test_unix_permissions() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("test_perms.txt");

        write_secure_file(&file_path, b"secure content").unwrap();

        let metadata = fs::metadata(&file_path).unwrap();
        let mode = metadata.permissions().mode();
        // Check that only owner has read/write (0o600), ignoring file type bits
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_owner_only_tightens_world_readable_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("loose.txt");
        fs::write(&file_path, b"secret").unwrap();
        let mut loose = fs::metadata(&file_path).unwrap().permissions();
        loose.set_mode(0o644);
        fs::set_permissions(&file_path, loose).unwrap();
        assert_eq!(
            fs::metadata(&file_path).unwrap().permissions().mode() & 0o777,
            0o644
        );

        ensure_owner_only_permissions(&file_path).unwrap();
        assert_eq!(
            fs::metadata(&file_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn ensure_owner_only_for_open_file_tightens_same_object() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("loose-handle.txt");
        fs::write(&file_path, b"secret").unwrap();
        let mut loose = fs::metadata(&file_path).unwrap().permissions();
        loose.set_mode(0o644);
        fs::set_permissions(&file_path, loose).unwrap();

        let file = OpenOptions::new().write(true).open(&file_path).unwrap();
        ensure_owner_only_permissions_for_file(&file).unwrap();

        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn write_secure_file_tightens_existing_world_readable_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("existing.txt");
        fs::write(&file_path, b"old").unwrap();
        let mut loose = fs::metadata(&file_path).unwrap().permissions();
        loose.set_mode(0o666);
        fs::set_permissions(&file_path, loose).unwrap();

        write_secure_file(&file_path, b"new secret").unwrap();
        assert_eq!(
            fs::metadata(&file_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "new secret");
    }

    #[cfg(unix)]
    #[test]
    fn ensure_owner_only_directory_keeps_traversal_and_removes_group_access() {
        let temp_dir = tempfile::tempdir().unwrap();
        let directory = temp_dir.path().join("private-stage");
        fs::create_dir(&directory).unwrap();
        let mut loose = fs::metadata(&directory).unwrap().permissions();
        loose.set_mode(0o755);
        fs::set_permissions(&directory, loose).unwrap();

        ensure_owner_only_directory_permissions(&directory).unwrap();

        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::write(directory.join("probe"), b"ok").unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn ensure_owner_only_directory_rejects_symlink() {
        let temp_dir = tempfile::tempdir().unwrap();
        let target = temp_dir.path().join("target");
        let link = temp_dir.path().join("private-stage");
        fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let error = ensure_owner_only_directory_permissions(&link).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[cfg(windows)]
    fn has_single_current_user_ace(path: &Path) -> io::Result<bool> {
        use std::os::windows::fs::OpenOptionsExt as _;
        use std::os::windows::io::AsRawHandle as _;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
        use windows::Win32::Security::{
            ACCESS_ALLOWED_ACE, DACL_SECURITY_INFORMATION, EqualSid, GetAce, PSECURITY_DESCRIPTOR,
        };

        use windows::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
            FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL,
        };

        let object = OpenOptions::new()
            .access_mode(READ_CONTROL.0)
            .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
            .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0)
            .open(path)?;
        let current_user = current_windows_user()?;
        unsafe {
            let mut dacl = std::ptr::null_mut();
            let mut raw_descriptor = PSECURITY_DESCRIPTOR::default();
            let status = GetSecurityInfo(
                HANDLE(object.as_raw_handle()),
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
            let _descriptor = WindowsLocalAllocation::new(raw_descriptor.0);
            if dacl.is_null() {
                return Ok(false);
            }
            let current_sid = current_user.sid();
            let mut current_user_full_control = false;
            let mut foreign_allow = false;
            for index in 0..(*dacl).AceCount {
                let mut raw_ace = std::ptr::null_mut();
                if GetAce(dacl, u32::from(index), &mut raw_ace).is_err() || raw_ace.is_null() {
                    continue;
                }
                let ace = &*(raw_ace as *const ACCESS_ALLOWED_ACE);
                if ace.Header.AceType != 0 {
                    continue;
                }
                let ace_sid = windows::Win32::Security::PSID(
                    std::ptr::addr_of!(ace.SidStart).cast_mut().cast(),
                );
                let full_control = ace.Mask == 0x10000000 || ace.Mask & 0x001f01ff == 0x001f01ff;
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

    #[cfg(windows)]
    #[test]
    fn ensure_owner_only_directory_acl_is_inherited_by_children() {
        let temp_dir = tempfile::tempdir().unwrap();
        let directory = temp_dir.path().join("private-stage");
        fs::create_dir(&directory).unwrap();

        ensure_owner_only_directory_permissions(&directory).unwrap();
        let child_directory = directory.join("child");
        let child_file = directory.join("probe");
        fs::create_dir(&child_directory).unwrap();
        fs::write(&child_file, b"ok").unwrap();

        assert!(has_single_current_user_ace(&child_directory).unwrap());
        assert!(has_single_current_user_ace(&child_file).unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn ensure_owner_only_directory_rejects_reparse_point() {
        use std::process::Command;

        let temp_dir = tempfile::tempdir().unwrap();
        let target = temp_dir.path().join("target");
        let link = temp_dir.path().join("private-stage");
        fs::create_dir(&target).unwrap();
        // A junction is a directory reparse point and does not require the
        // developer-mode/symlink privilege that would make this test silently
        // vacuous on locked-down Windows runners.
        let mut command = Command::new("cmd");
        command.args(["/C", "mklink", "/J"]).arg(&link).arg(&target);
        xai_tty_utils::detach_std_command(&mut command);
        let output = command
            .output()
            .expect("create junction with cmd /C mklink");
        assert!(
            output.status.success(),
            "failed to create test junction: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let error = ensure_owner_only_directory_permissions(&link).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
