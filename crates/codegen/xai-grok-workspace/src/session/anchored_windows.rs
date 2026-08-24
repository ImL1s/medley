//! Windows handle-relative directory operations for [`super::AnchoredDirectory`].

use super::*;
use std::mem::{MaybeUninit, offset_of, size_of};
use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::windows::fs::OpenOptionsExt as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT, SetSecurityInfo};
use windows::Win32::Security::GetTokenInformation;
use windows::Win32::Security::{
    ACL, ACL_REVISION, AddAccessAllowedAceEx, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION,
    EqualSid, GetLengthSid, InitializeAcl, InitializeSecurityDescriptor, OBJECT_INHERIT_ACE,
    OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    SE_DACL_PROTECTED, SECURITY_DESCRIPTOR, SetSecurityDescriptorControl,
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

fn enable_privilege(name: windows::core::PCWSTR) -> io::Result<()> {
    use windows::Win32::Foundation::LUID;
    use windows::Win32::Security::{
        AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED,
        TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES,
    };

    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
        .map_err(io::Error::other)?;
        struct Token(HANDLE);
        impl Drop for Token {
            fn drop(&mut self) {
                let _ = unsafe { CloseHandle(self.0) };
            }
        }
        let token = Token(token);
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
        | FILE_TRAVERSE
        | FILE_WRITE_DATA
        | FILE_APPEND_DATA
        | DELETE
        | windows::Win32::Storage::FileSystem::READ_CONTROL
        | windows::Win32::Storage::FileSystem::WRITE_DAC
        | SYNCHRONIZE)
        .0;
    let reduced = (FILE_LIST_DIRECTORY
        | FILE_READ_ATTRIBUTES
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
    if let Err(error) = ensure_owner_only_regular_file(&file) {
        let _ = delete_by_handle(&file);
        return Err(error);
    }
    Ok(file)
}

fn ensure_owner_only_regular_file(file: &File) -> io::Result<()> {
    let security = OwnerOnlySecurity::new_with_inheritance(windows::Win32::Security::ACE_FLAGS(0))?;
    let _ = enable_privilege(windows::core::w!("SeTakeOwnershipPrivilege"));
    let _ = enable_privilege(windows::core::w!("SeRestorePrivilege"));
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
    let share_access = (FILE_SHARE_READ | FILE_SHARE_WRITE).0;
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

pub(super) fn create_child_dir(parent: &File, name: &OsStr) -> io::Result<File> {
    let mut security = OwnerOnlySecurity::new()?;
    let file = open_relative(
        parent,
        name,
        FILE_CREATE,
        FILE_DIRECTORY_FILE,
        (FILE_LIST_DIRECTORY
            | FILE_READ_ATTRIBUTES
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

    fn new_with_inheritance(inheritance: windows::Win32::Security::ACE_FLAGS) -> io::Result<Self> {
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
            let mut token_info = vec![0_usize; (required as usize).div_ceil(size_of::<usize>())];
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
            SetSecurityDescriptorControl(descriptor_pointer, SE_DACL_PROTECTED, SE_DACL_PROTECTED)
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
    let _ = enable_privilege(windows::core::w!("SeTakeOwnershipPrivilege"));
    let _ = enable_privilege(windows::core::w!("SeRestorePrivilege"));
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

pub(super) fn is_owner_only(file: &File) -> io::Result<bool> {
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

fn rename_retained_handle_no_replace(
    source: &File,
    target_parent: &File,
    target_name: &OsStr,
) -> io::Result<()> {
    let _ = enable_privilege(windows::core::w!("SeBackupPrivilege"));
    let _ = enable_privilege(windows::core::w!("SeRestorePrivilege"));
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
        Err(map_nt_no_replace_error(status, target_parent, target_name))
    }
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
            Err(_) => true,
        },
    }
}

fn map_nt_no_replace_error(status: i32, target_parent: &File, target_name: &OsStr) -> io::Error {
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
    _source_parent: &File,
    _source_name: &OsStr,
    retained_child: &File,
    target_parent: &File,
    target_name: &OsStr,
) -> io::Result<()> {
    rename_retained_handle_no_replace(retained_child, target_parent, target_name)
}
