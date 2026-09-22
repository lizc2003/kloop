//! Handle-relative private storage for windows.
//!
//! Components are opened through `NtCreateFile` with `OBJ_DONT_REPARSE`, so a
//! junction or symlink planted along the walk is refused rather than followed.

use std::ffi::OsStr;
use std::io::Write as _;
use std::path::Path;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::bail;

use super::read_opened_private_file;
use super::validate_component;
use super::validate_private_file;

pub(super) fn read_private_string(path: &Path, label: &str) -> Result<Option<String>> {
    let parent = path
        .parent()
        .with_context(|| format!("{label} has no parent directory"))?;
    let Some(dir) = open_directory_path(parent, label)? else {
        return Ok(None);
    };
    read_private_string_at(&dir, private_file_name(path, label)?, label)
}

pub(super) fn write_private_atomic(path: &Path, label: &str, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("{label} has no parent directory"))?;
    let anchor_path = parent
        .parent()
        .with_context(|| format!("directory for {label} has no parent"))?;
    let parent_name = parent
        .file_name()
        .with_context(|| format!("directory for {label} has no name"))?;
    let anchor = open_directory_path(anchor_path, label)?
        .with_context(|| format!("cannot open parent directory for {label}"))?;
    let dir = ensure_private_dir_at(&anchor, parent_name, label)?;
    write_private_atomic_at(&dir, private_file_name(path, label)?, label, bytes)
}

/// Windows carries no mode bits: a file inherits the ACL of the directory it
/// was created in, so nothing is left to re-check once the open itself refused
/// reparse points.
pub(super) fn validate_private_permissions(
    _metadata: &std::fs::Metadata,
    _label: &str,
) -> Result<()> {
    Ok(())
}

fn private_file_name<'a>(path: &'a Path, label: &str) -> Result<&'a OsStr> {
    path.file_name()
        .with_context(|| format!("{label} has no file name"))
}

fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt as _;

    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

#[derive(Clone, Copy)]
enum OpenResultKind {
    Opened,
    Missing,
    Exists,
}

struct OpenResult {
    kind: OpenResultKind,
    file: Option<std::fs::File>,
}

fn validate_opened_file(file: &std::fs::File, label: &str, directory: bool) -> Result<()> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle;

    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        GetFileInformationByHandle(
            file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE,
            &mut info,
        )
    };
    if ok == 0 {
        bail!("cannot inspect {label}");
    }
    let attributes = info.dwFileAttributes;
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        bail!("{label} must not be a symlink or reparse point");
    }
    let is_directory = attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    if is_directory != directory {
        if directory {
            bail!("directory for {label} must be a regular directory, not a symlink");
        }
        bail!("{label} must be a regular file");
    }
    Ok(())
}

fn open_directory_path(path: &Path, label: &str) -> Result<Option<std::fs::File>> {
    use std::os::windows::io::FromRawHandle as _;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::CreateFileW;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
    use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;
    use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
    use windows_sys::Win32::Storage::FileSystem::OPEN_EXISTING;

    let path = wide_path(path);
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            0,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        bail!("cannot open directory for {label}");
    }
    let file = unsafe { std::fs::File::from_raw_handle(handle as _) };
    validate_opened_file(&file, label, true)?;
    Ok(Some(file))
}

fn open_relative(
    parent: &std::fs::File,
    name: &OsStr,
    label: &str,
    directory: bool,
    desired_access: u32,
    disposition: u32,
) -> Result<OpenResult> {
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::io::AsRawHandle as _;
    use std::os::windows::io::FromRawHandle as _;
    use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows_sys::Wdk::Storage::FileSystem::FILE_DIRECTORY_FILE;
    use windows_sys::Wdk::Storage::FileSystem::FILE_NON_DIRECTORY_FILE;
    use windows_sys::Wdk::Storage::FileSystem::FILE_OPEN_REPARSE_POINT;
    use windows_sys::Wdk::Storage::FileSystem::FILE_SYNCHRONOUS_IO_NONALERT;
    use windows_sys::Wdk::Storage::FileSystem::NtCreateFile;
    use windows_sys::Win32::Foundation::STATUS_OBJECT_NAME_COLLISION;
    use windows_sys::Win32::Foundation::STATUS_OBJECT_NAME_NOT_FOUND;
    use windows_sys::Win32::Foundation::STATUS_OBJECT_PATH_NOT_FOUND;
    use windows_sys::Win32::Foundation::STATUS_REPARSE_POINT_ENCOUNTERED;
    use windows_sys::Win32::Foundation::UNICODE_STRING;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
    use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
    use windows_sys::Win32::System::Kernel::OBJ_CASE_INSENSITIVE;
    use windows_sys::Win32::System::Kernel::OBJ_DONT_REPARSE;

    validate_component(name, label)?;
    let mut wide: Vec<u16> = name.encode_wide().collect();
    let byte_len = wide
        .len()
        .checked_mul(2)
        .and_then(|len| u16::try_from(len).ok())
        .with_context(|| format!("private path component is too long for {label}"))?;
    let unicode = UNICODE_STRING {
        Length: byte_len,
        MaximumLength: byte_len,
        Buffer: wide.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE,
        ObjectName: &unicode,
        Attributes: (OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE) as u32,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut io_status: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
    let mut handle = 0;
    let create_options = FILE_OPEN_REPARSE_POINT
        | FILE_SYNCHRONOUS_IO_NONALERT
        | if directory {
            FILE_DIRECTORY_FILE
        } else {
            FILE_NON_DIRECTORY_FILE
        };
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            desired_access | SYNCHRONIZE,
            &attributes,
            &mut io_status,
            std::ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            disposition,
            create_options,
            std::ptr::null(),
            0,
        )
    };
    if status == STATUS_OBJECT_NAME_NOT_FOUND || status == STATUS_OBJECT_PATH_NOT_FOUND {
        return Ok(OpenResult {
            kind: OpenResultKind::Missing,
            file: None,
        });
    }
    if status == STATUS_OBJECT_NAME_COLLISION {
        return Ok(OpenResult {
            kind: OpenResultKind::Exists,
            file: None,
        });
    }
    if status == STATUS_REPARSE_POINT_ENCOUNTERED {
        bail!("{label} must not be a symlink or reparse point");
    }
    if status < 0 {
        bail!("cannot open {label}");
    }
    let file = unsafe { std::fs::File::from_raw_handle(handle as _) };
    validate_opened_file(&file, label, directory)?;
    Ok(OpenResult {
        kind: OpenResultKind::Opened,
        file: Some(file),
    })
}

fn sync_directory_file(dir: &std::fs::File, label: &str) -> Result<()> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::FlushFileBuffers;

    let synced =
        unsafe { FlushFileBuffers(dir.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE) };
    if synced == 0 {
        bail!("cannot sync directory for {label}");
    }
    Ok(())
}

fn open_private_dir_at(
    parent: &std::fs::File,
    name: &OsStr,
    label: &str,
) -> Result<Option<std::fs::File>> {
    use windows_sys::Wdk::Storage::FileSystem::FILE_OPEN;
    use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;
    use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE;

    let opened = open_relative(
        parent,
        name,
        label,
        true,
        FILE_GENERIC_READ | FILE_GENERIC_WRITE,
        FILE_OPEN,
    )?;
    Ok(opened.file)
}

fn ensure_private_dir_at(
    parent: &std::fs::File,
    name: &OsStr,
    label: &str,
) -> Result<std::fs::File> {
    use windows_sys::Wdk::Storage::FileSystem::FILE_OPEN_IF;
    use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;
    use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE;

    let opened = open_relative(
        parent,
        name,
        label,
        true,
        FILE_GENERIC_READ | FILE_GENERIC_WRITE,
        FILE_OPEN_IF,
    )?;
    let file = opened
        .file
        .with_context(|| format!("cannot open directory for {label}"))?;
    sync_directory_file(parent, label)?;
    Ok(file)
}

pub(super) fn read_private_string_at(
    dir: &std::fs::File,
    name: &OsStr,
    label: &str,
) -> Result<Option<String>> {
    use windows_sys::Wdk::Storage::FileSystem::FILE_OPEN;
    use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;

    let opened = open_relative(dir, name, label, false, FILE_GENERIC_READ, FILE_OPEN)?;
    match opened.file {
        Some(file) => read_opened_private_file(file, label).map(Some),
        None => Ok(None),
    }
}

fn rename_private_file(
    file: &std::fs::File,
    dir: &std::fs::File,
    name: &OsStr,
    label: &str,
) -> Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::FILE_RENAME_INFO_0;
    use windows_sys::Win32::Storage::FileSystem::FileRenameInfo;
    use windows_sys::Win32::Storage::FileSystem::SetFileInformationByHandle;

    #[repr(C)]
    struct RenameInfo {
        anonymous: FILE_RENAME_INFO_0,
        root_directory: HANDLE,
        file_name_length: u32,
        file_name: [u16; 255],
    }

    validate_component(name, label)?;
    let wide: Vec<u16> = name.encode_wide().collect();
    if wide.len() > 255 {
        bail!("private path component is too long for {label}");
    }
    let mut info = RenameInfo {
        anonymous: FILE_RENAME_INFO_0 { ReplaceIfExists: 1 },
        root_directory: dir.as_raw_handle() as HANDLE,
        file_name_length: (wide.len() * 2) as u32,
        file_name: [0; 255],
    };
    info.file_name[..wide.len()].copy_from_slice(&wide);
    let renamed = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle() as HANDLE,
            FileRenameInfo,
            (&info as *const RenameInfo).cast(),
            std::mem::size_of::<RenameInfo>() as u32,
        )
    };
    if renamed == 0 {
        bail!("cannot replace {label}");
    }
    Ok(())
}

fn delete_private_file(file: &std::fs::File) {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::FILE_DISPOSITION_INFO;
    use windows_sys::Win32::Storage::FileSystem::FileDispositionInfo;
    use windows_sys::Win32::Storage::FileSystem::SetFileInformationByHandle;

    let disposition = FILE_DISPOSITION_INFO { DeleteFile: 1 };
    let _ = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE,
            FileDispositionInfo,
            (&disposition as *const FILE_DISPOSITION_INFO).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };
}

pub(super) fn write_private_atomic_at(
    dir: &std::fs::File,
    name: &OsStr,
    label: &str,
    bytes: &[u8],
) -> Result<()> {
    use windows_sys::Wdk::Storage::FileSystem::FILE_CREATE;
    use windows_sys::Win32::Storage::FileSystem::DELETE;
    use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE;

    if let Some(existing) = read_private_string_at(dir, name, label)? {
        drop(existing);
    }
    let stem = name.to_string_lossy();
    for suffix in 0..100_u8 {
        let temp = format!(".{stem}.tmp-{}-{suffix}", std::process::id());
        let opened = open_relative(
            dir,
            OsStr::new(&temp),
            label,
            false,
            FILE_GENERIC_WRITE | DELETE,
            FILE_CREATE,
        )?;
        if matches!(opened.kind, OpenResultKind::Exists) {
            continue;
        }
        let mut file = opened
            .file
            .with_context(|| format!("cannot create temporary {label}"))?;
        let result = (|| -> Result<()> {
            file.write_all(bytes)
                .with_context(|| format!("cannot write temporary {label}"))?;
            file.sync_all()
                .with_context(|| format!("cannot sync temporary {label}"))?;
            rename_private_file(&file, dir, name, label)?;
            sync_directory_file(dir, label)
        })();
        if result.is_err() {
            delete_private_file(&file);
        }
        return result;
    }
    bail!("cannot create temporary {label}")
}

fn open_private_lock_at(dir: &std::fs::File, name: &OsStr, label: &str) -> Result<std::fs::File> {
    use windows_sys::Wdk::Storage::FileSystem::FILE_OPEN_IF;
    use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;
    use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE;

    let opened = open_relative(
        dir,
        name,
        label,
        false,
        FILE_GENERIC_READ | FILE_GENERIC_WRITE,
        FILE_OPEN_IF,
    )?;
    let file = opened
        .file
        .with_context(|| format!("cannot open {label}"))?;
    validate_private_file(&file, label)?;
    Ok(file)
}

pub(super) struct Dir {
    file: std::fs::File,
}

impl Dir {
    pub(super) fn open(root: &Path, components: &[&OsStr], label: &str) -> Result<Option<Self>> {
        let Some(mut current) = open_directory_path(root, label)? else {
            return Ok(None);
        };
        for component in components {
            let Some(next) = open_private_dir_at(&current, component, label)? else {
                return Ok(None);
            };
            current = next;
        }
        Ok(Some(Self { file: current }))
    }

    pub(super) fn ensure(root: &Path, components: &[&OsStr], label: &str) -> Result<Self> {
        let parent = root
            .parent()
            .with_context(|| format!("directory for {label} has no parent"))?;
        let name = root
            .file_name()
            .with_context(|| format!("directory for {label} has no name"))?;
        let parent = open_directory_path(parent, label)?
            .with_context(|| format!("cannot open parent directory for {label}"))?;
        let mut current = ensure_private_dir_at(&parent, name, label)?;
        for component in components {
            current = ensure_private_dir_at(&current, component, label)?;
        }
        Ok(Self { file: current })
    }

    pub(super) fn read_string(&self, name: &OsStr, label: &str) -> Result<Option<String>> {
        read_private_string_at(&self.file, name, label)
    }

    pub(super) fn write_atomic(&self, name: &OsStr, label: &str, bytes: &[u8]) -> Result<()> {
        write_private_atomic_at(&self.file, name, label, bytes)
    }

    pub(super) fn open_lock_file(&self, name: &OsStr, label: &str) -> Result<std::fs::File> {
        open_private_lock_at(&self.file, name, label)
    }
}

pub(super) fn lock_file(file: &std::fs::File) -> Result<()> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::LOCKFILE_EXCLUSIVE_LOCK;
    use windows_sys::Win32::Storage::FileSystem::LockFileEx;
    use windows_sys::Win32::System::IO::OVERLAPPED;

    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    let result = unsafe {
        LockFileEx(
            file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE,
            LOCKFILE_EXCLUSIVE_LOCK,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error()).context("lock private store");
    }
    Ok(())
}

pub(super) fn unlock_file(file: &std::fs::File) -> Result<()> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;
    use windows_sys::Win32::System::IO::OVERLAPPED;

    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    let result = unsafe {
        UnlockFileEx(
            file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error()).context("unlock private store");
    }
    Ok(())
}
