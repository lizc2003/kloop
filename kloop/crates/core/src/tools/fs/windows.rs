use std::ffi::OsStr;
use std::mem::offset_of;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::AsRawHandle as _;
use std::os::windows::io::FromRawHandle as _;
use std::ptr;

use anyhow::bail;
use anyhow::Context as _;
use anyhow::Result;
use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::NtCreateFile;
use windows_sys::Wdk::Storage::FileSystem::NtSetInformationFile;
use windows_sys::Wdk::Storage::FileSystem::FILE_CREATE;
use windows_sys::Wdk::Storage::FileSystem::FILE_DIRECTORY_FILE;
use windows_sys::Wdk::Storage::FileSystem::FILE_NON_DIRECTORY_FILE;
use windows_sys::Wdk::Storage::FileSystem::FILE_OPEN;
use windows_sys::Wdk::Storage::FileSystem::FILE_OPEN_IF;
use windows_sys::Wdk::Storage::FileSystem::FILE_OPEN_REPARSE_POINT;
use windows_sys::Wdk::Storage::FileSystem::FILE_SYNCHRONOUS_IO_NONALERT;
use windows_sys::Win32::Foundation::RtlNtStatusToDosError;
use windows_sys::Win32::Foundation::GENERIC_READ;
use windows_sys::Win32::Foundation::GENERIC_WRITE;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Foundation::UNICODE_STRING;
use windows_sys::Win32::Storage::FileSystem::CreateFileW;
use windows_sys::Win32::Storage::FileSystem::FileAttributeTagInfo;
use windows_sys::Win32::Storage::FileSystem::FileDispositionInfo;
use windows_sys::Win32::Storage::FileSystem::FileIdInfo;
use windows_sys::Win32::Storage::FileSystem::FileStandardInfo;
use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandleEx;
use windows_sys::Win32::Storage::FileSystem::SetFileInformationByHandle;
use windows_sys::Win32::Storage::FileSystem::DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_ACCESS_RIGHTS;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_TAG_INFO;
use windows_sys::Win32::Storage::FileSystem::FILE_DISPOSITION_INFO;
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
use windows_sys::Win32::Storage::FileSystem::FILE_ID_INFO;
use windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::FILE_RENAME_INFO;
use windows_sys::Win32::Storage::FileSystem::FILE_RENAME_INFO_0;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_MODE;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
use windows_sys::Win32::Storage::FileSystem::FILE_STANDARD_INFO;
use windows_sys::Win32::Storage::FileSystem::OPEN_EXISTING;
use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
use windows_sys::Win32::System::Kernel::OBJ_CASE_INSENSITIVE;
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

use crate::file_state::FileIdentity;

const FILE_OPENED_RESULT: usize = 1;
const FILE_CREATED_RESULT: usize = 2;
const FILE_RENAME_INFORMATION_EX_CLASS: i32 = 65;
const FILE_RENAME_FLAG_REPLACE_IF_EXISTS: u32 = 0x0000_0001;

#[derive(Clone, Copy)]
enum ExpectedKind {
    Directory,
    RegularFile,
}

pub(super) fn open_directory_absolute(path: &std::path::Path) -> Result<std::fs::File> {
    let mut path: Vec<u16> = path.as_os_str().encode_wide().collect();
    path.push(0);
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            0,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error()).context("cannot open directory handle");
    }
    let file = unsafe { file_from_handle(handle) };
    validate_handle(&file, ExpectedKind::Directory)?;
    Ok(file)
}

pub(super) fn open_child_regular_file(
    parent: &std::fs::File,
    name: &OsStr,
) -> Result<Option<std::fs::File>> {
    let opened = match nt_open_relative(
        parent,
        name,
        GENERIC_READ | SYNCHRONIZE,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        FILE_OPEN,
        FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        FILE_ATTRIBUTE_NORMAL,
    ) {
        Ok(opened) => opened,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("cannot open child file"),
    };
    validate_handle(&opened.0, ExpectedKind::RegularFile)?;
    if opened.1 != FILE_OPENED_RESULT {
        bail!("unexpected result while opening child file");
    }
    Ok(Some(opened.0))
}

pub(super) fn open_or_create_child_directory(
    parent: &std::fs::File,
    name: &OsStr,
) -> Result<(std::fs::File, bool)> {
    let primary = nt_open_relative(
        parent,
        name,
        FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        FILE_OPEN_IF,
        FILE_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        FILE_ATTRIBUTE_DIRECTORY,
    );
    let (directory, information) = match primary {
        Ok(opened) => opened,
        Err(error)
            if error.raw_os_error().is_some_and(|code| {
                code == windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED as i32
                    || code == windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION as i32
            }) =>
        {
            let opened = nt_open_relative(
                parent,
                name,
                FILE_READ_ATTRIBUTES | SYNCHRONIZE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                FILE_OPEN,
                FILE_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
                FILE_ATTRIBUTE_DIRECTORY,
            )?;
            if opened.1 != FILE_OPENED_RESULT {
                bail!("directory fallback unexpectedly created a path component");
            }
            opened
        }
        Err(error) => return Err(error).context("cannot open or create child directory"),
    };
    validate_handle(&directory, ExpectedKind::Directory)?;
    let created = match information {
        FILE_CREATED_RESULT => true,
        FILE_OPENED_RESULT => false,
        other => bail!("unexpected directory open result {other}"),
    };
    Ok((directory, created))
}

pub(super) fn create_temp_file(parent: &std::fs::File, name: &OsStr) -> Result<std::fs::File> {
    let (file, information) = nt_open_relative(
        parent,
        name,
        GENERIC_READ | GENERIC_WRITE | SYNCHRONIZE | DELETE,
        FILE_SHARE_READ | FILE_SHARE_DELETE,
        FILE_CREATE,
        FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        FILE_ATTRIBUTE_NORMAL,
    )?;
    if information != FILE_CREATED_RESULT {
        bail!("temporary file was not created exclusively");
    }
    validate_handle(&file, ExpectedKind::RegularFile)?;
    Ok(file)
}

pub(super) fn rename_file_relative(
    file: &std::fs::File,
    parent: &std::fs::File,
    name: &OsStr,
    replace_existing: bool,
) -> Result<()> {
    let name: Vec<u16> = name.encode_wide().collect();
    let name_bytes = name
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .context("target name is too long")?;
    let name_bytes_u32 = u32::try_from(name_bytes).context("target name is too long")?;
    let total = std::mem::size_of::<FILE_RENAME_INFO>()
        .checked_add(name_bytes)
        .context("rename buffer is too large")?;
    let words = total.div_ceil(std::mem::size_of::<usize>());
    let mut storage = vec![0usize; words];
    let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    unsafe {
        (*info).Anonymous = FILE_RENAME_INFO_0 {
            Flags: if replace_existing {
                FILE_RENAME_FLAG_REPLACE_IF_EXISTS
            } else {
                0
            },
        };
        (*info).RootDirectory = raw_handle(parent);
        (*info).FileNameLength = name_bytes_u32;
        ptr::copy_nonoverlapping(
            name.as_ptr().cast::<u8>(),
            (info.cast::<u8>()).add(offset_of!(FILE_RENAME_INFO, FileName)),
            name_bytes,
        );
        let mut status: IO_STATUS_BLOCK = std::mem::zeroed();
        let result = NtSetInformationFile(
            raw_handle(file),
            &mut status,
            info.cast(),
            u32::try_from(total).context("rename buffer is too large")?,
            FILE_RENAME_INFORMATION_EX_CLASS,
        );
        if result < 0 {
            let code = RtlNtStatusToDosError(result);
            return Err(std::io::Error::from_raw_os_error(code as i32))
                .context("cannot rename temporary file");
        }
    }
    Ok(())
}

pub(super) fn delete_file_handle(file: &std::fs::File) -> Result<()> {
    set_delete_disposition(file)
}

pub(super) fn remove_created_directory(
    parent: &std::fs::File,
    name: &OsStr,
    expected: FileIdentity,
) -> Result<()> {
    let (named, information) = nt_open_relative(
        parent,
        name,
        FILE_READ_ATTRIBUTES | SYNCHRONIZE | DELETE,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        FILE_OPEN,
        FILE_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        FILE_ATTRIBUTE_DIRECTORY,
    )?;
    if information != FILE_OPENED_RESULT {
        bail!("created directory name no longer refers to an existing directory");
    }
    validate_handle(&named, ExpectedKind::Directory)?;
    if file_identity(&named)? != expected {
        bail!("created directory name changed before cleanup");
    }
    set_delete_disposition(&named)
}

pub(super) fn has_multiple_hard_links(file: &std::fs::File) -> Result<bool> {
    let mut info: FILE_STANDARD_INFO = unsafe { std::mem::zeroed() };
    let success = unsafe {
        GetFileInformationByHandleEx(
            raw_handle(file),
            FileStandardInfo,
            (&mut info as *mut FILE_STANDARD_INFO).cast(),
            std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
        )
    };
    if success == 0 {
        return Err(std::io::Error::last_os_error()).context("cannot query hard-link count");
    }
    Ok(info.NumberOfLinks > 1)
}

pub(super) fn file_identity(file: &std::fs::File) -> Result<FileIdentity> {
    let mut info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
    let success = unsafe {
        GetFileInformationByHandleEx(
            raw_handle(file),
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if success == 0 {
        return Err(std::io::Error::last_os_error()).context("cannot query stable file identity");
    }
    Ok(FileIdentity {
        volume: info.VolumeSerialNumber,
        file_id: info.FileId.Identifier,
    })
}

fn validate_handle(file: &std::fs::File, expected: ExpectedKind) -> Result<()> {
    let mut info: FILE_ATTRIBUTE_TAG_INFO = unsafe { std::mem::zeroed() };
    let success = unsafe {
        GetFileInformationByHandleEx(
            raw_handle(file),
            FileAttributeTagInfo,
            (&mut info as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    };
    if success == 0 {
        return Err(std::io::Error::last_os_error()).context("cannot query file attributes");
    }
    if info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        bail!("reparse points are not allowed");
    }
    let is_directory = info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    match (expected, is_directory) {
        (ExpectedKind::Directory, true) | (ExpectedKind::RegularFile, false) => {}
        (ExpectedKind::Directory, false) => bail!("path component is not a directory"),
        (ExpectedKind::RegularFile, true) => bail!("target is not a regular file"),
    }
    let _ = file_identity(file)?;
    Ok(())
}

fn set_delete_disposition(file: &std::fs::File) -> Result<()> {
    let info = FILE_DISPOSITION_INFO { DeleteFile: 1 };
    let success = unsafe {
        SetFileInformationByHandle(
            raw_handle(file),
            FileDispositionInfo,
            (&info as *const FILE_DISPOSITION_INFO).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };
    if success == 0 {
        return Err(std::io::Error::last_os_error()).context("cannot delete handle-bound path");
    }
    Ok(())
}

fn nt_open_relative(
    parent: &std::fs::File,
    name: &OsStr,
    desired_access: FILE_ACCESS_RIGHTS,
    share_access: FILE_SHARE_MODE,
    disposition: u32,
    options: u32,
    attributes: u32,
) -> std::io::Result<(std::fs::File, usize)> {
    let mut name: Vec<u16> = name.encode_wide().collect();
    let name_bytes = name
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .and_then(|length| u16::try_from(length).ok())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path component is too long",
            )
        })?;
    let unicode = UNICODE_STRING {
        Length: name_bytes,
        MaximumLength: name_bytes,
        Buffer: name.as_mut_ptr(),
    };
    let object = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: raw_handle(parent),
        ObjectName: &unicode,
        Attributes: OBJ_CASE_INSENSITIVE as u32,
        SecurityDescriptor: ptr::null(),
        SecurityQualityOfService: ptr::null(),
    };
    let mut handle: HANDLE = 0;
    let mut status: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
    let result = unsafe {
        NtCreateFile(
            &mut handle,
            desired_access,
            &object,
            &mut status,
            ptr::null(),
            attributes,
            share_access,
            disposition,
            options,
            ptr::null(),
            0,
        )
    };
    if result < 0 {
        let code = unsafe { RtlNtStatusToDosError(result) };
        return Err(std::io::Error::from_raw_os_error(code as i32));
    }
    if handle == 0 || handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::other(
            "NtCreateFile returned an invalid handle",
        ));
    }
    Ok((unsafe { file_from_handle(handle) }, status.Information))
}

fn raw_handle(file: &std::fs::File) -> HANDLE {
    file.as_raw_handle() as HANDLE
}

unsafe fn file_from_handle(handle: HANDLE) -> std::fs::File {
    std::fs::File::from_raw_handle(handle as *mut std::ffi::c_void)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let root =
            std::env::temp_dir().join(format!("kloop-windows-fs-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::canonicalize(root).unwrap()
    }

    #[test]
    fn relative_directory_temp_sharing_and_rename_use_stable_handles() {
        let root = temp_root("relative");
        let root_handle = open_directory_absolute(&root).unwrap();
        let (directory, created) =
            open_or_create_child_directory(&root_handle, OsStr::new("nested")).unwrap();
        assert!(created);
        let first_identity = file_identity(&directory).unwrap();
        let root_identity = file_identity(&root_handle).unwrap();
        assert_eq!(first_identity.volume, root_identity.volume);
        assert_ne!(first_identity.file_id, root_identity.file_id);
        let (reopened, created) =
            open_or_create_child_directory(&root_handle, OsStr::new("NESTED")).unwrap();
        assert!(!created);
        assert_eq!(file_identity(&reopened).unwrap(), first_identity);
        drop(reopened);

        let mut temp = create_temp_file(&directory, OsStr::new("temp.bin")).unwrap();
        let error = nt_open_relative(
            &directory,
            OsStr::new("temp.bin"),
            GENERIC_WRITE | SYNCHRONIZE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OPEN,
            FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            FILE_ATTRIBUTE_NORMAL,
        )
        .unwrap_err();
        assert_eq!(
            error.raw_os_error(),
            Some(windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION as i32)
        );
        temp.write_all(b"payload").unwrap();
        temp.sync_all().unwrap();
        rename_file_relative(
            &temp,
            &directory,
            OsStr::new("a"),
            /* replace_existing */ false,
        )
        .unwrap();
        assert_eq!(std::fs::read(root.join("nested/a")).unwrap(), b"payload");
        std::fs::write(root.join("nested/target.bin"), b"existing").unwrap();
        let error = rename_file_relative(
            &temp,
            &directory,
            OsStr::new("target.bin"),
            /* replace_existing */ false,
        )
        .unwrap_err();
        assert_eq!(
            std::fs::read(root.join("nested/target.bin")).unwrap(),
            b"existing"
        );
        assert!(root.join("nested/a").is_file(), "{error:#}");
        std::fs::remove_file(root.join("nested/target.bin")).unwrap();
        rename_file_relative(
            &temp,
            &directory,
            OsStr::new("target.bin"),
            /* replace_existing */ false,
        )
        .unwrap();
        let target = open_child_regular_file(&directory, OsStr::new("TARGET.BIN"))
            .unwrap()
            .unwrap();
        assert_eq!(
            std::fs::read(root.join("nested/target.bin")).unwrap(),
            b"payload"
        );
        assert_eq!(
            file_identity(&target).unwrap(),
            file_identity(&temp).unwrap()
        );
        drop(target);
        drop(temp);
        drop(directory);
        drop(root_handle);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn retargeted_directory_is_rejected_by_binding_check() {
        let root = temp_root("retarget");
        let root_handle = open_directory_absolute(&root).unwrap();
        let (directory, created) =
            open_or_create_child_directory(&root_handle, OsStr::new("nested")).unwrap();
        assert!(created);
        let nested = std::fs::canonicalize(root.join("nested")).unwrap();
        let identity = file_identity(&directory).unwrap();

        std::fs::rename(&nested, root.join("moved")).unwrap();
        std::fs::create_dir(&nested).unwrap();
        let error = super::super::verify_parent_binding(
            &directory,
            &nested,
            "write_file",
            nested.to_str().unwrap(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("changed"), "{error:#}");
        assert_eq!(file_identity(&directory).unwrap(), identity);
        assert!(root.join("nested").is_dir());
        assert!(root.join("moved").is_dir());

        drop(directory);
        drop(root_handle);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cleanup_refuses_replacement_with_a_different_identity() {
        let root = temp_root("cleanup-replacement");
        let root_handle = open_directory_absolute(&root).unwrap();
        let (directory, created) =
            open_or_create_child_directory(&root_handle, OsStr::new("nested")).unwrap();
        assert!(created);
        let identity = file_identity(&directory).unwrap();
        drop(directory);

        std::fs::rename(root.join("nested"), root.join("moved")).unwrap();
        std::fs::create_dir(root.join("nested")).unwrap();
        let error =
            remove_created_directory(&root_handle, OsStr::new("nested"), identity).unwrap_err();
        assert!(error.to_string().contains("name changed"), "{error:#}");
        assert!(root.join("nested").is_dir());
        assert!(root.join("moved").is_dir());

        std::fs::remove_dir(root.join("nested")).unwrap();
        remove_created_directory(&root_handle, OsStr::new("moved"), identity).unwrap();
        assert!(!root.join("moved").exists());
        drop(root_handle);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cleanup_refuses_nonempty_then_deletes_identity_matched_name() {
        let root = temp_root("cleanup");
        let root_handle = open_directory_absolute(&root).unwrap();
        let (directory, created) =
            open_or_create_child_directory(&root_handle, OsStr::new("nested")).unwrap();
        assert!(created);
        let identity = file_identity(&directory).unwrap();
        std::fs::write(root.join("nested/blocker"), b"keep").unwrap();
        drop(directory);
        assert!(remove_created_directory(&root_handle, OsStr::new("nested"), identity).is_err());
        assert!(root.join("nested/blocker").exists());
        std::fs::remove_file(root.join("nested/blocker")).unwrap();
        remove_created_directory(&root_handle, OsStr::new("nested"), identity).unwrap();
        assert!(!root.join("nested").exists());
        drop(root_handle);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn nested_cleanup_removes_created_chain_after_extra_handles_close() {
        let root = temp_root("nested-cleanup");
        let root_handle = open_directory_absolute(&root).unwrap();
        let (first, created) =
            open_or_create_child_directory(&root_handle, OsStr::new("a")).unwrap();
        assert!(created);
        let first_identity = file_identity(&first).unwrap();
        let first_cleanup = first.try_clone().unwrap();
        let (second, created) = open_or_create_child_directory(&first, OsStr::new("b")).unwrap();
        assert!(created);
        let second_identity = file_identity(&second).unwrap();
        let second_cleanup = second.try_clone().unwrap();

        drop(second);
        assert_eq!(file_identity(&second_cleanup).unwrap(), second_identity);
        drop(second_cleanup);
        remove_created_directory(&first, OsStr::new("b"), second_identity).unwrap();
        drop(first);
        assert_eq!(file_identity(&first_cleanup).unwrap(), first_identity);
        drop(first_cleanup);
        remove_created_directory(&root_handle, OsStr::new("a"), first_identity).unwrap();
        assert!(!root.join("a").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn directory_junction_is_rejected_as_a_reparse_point() {
        let root = temp_root("junction");
        let target = root.join("target");
        let junction = root.join("junction");
        std::fs::create_dir(&target).unwrap();
        let status = std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                junction.to_str().unwrap(),
                target.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert!(status.success(), "mklink /J must be available on NTFS CI");
        let error = open_directory_absolute(&junction).unwrap_err();
        assert!(error.to_string().contains("reparse"), "{error:#}");
        let root_handle = open_directory_absolute(&root).unwrap();
        let error =
            open_or_create_child_directory(&root_handle, OsStr::new("junction")).unwrap_err();
        assert!(error.to_string().contains("reparse"), "{error:#}");
        drop(root_handle);
        let _ = std::fs::remove_dir_all(junction);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn reparse_attribute_is_always_rejected() {
        fn allowed(attributes: u32) -> bool {
            attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0
        }
        assert!(!allowed(FILE_ATTRIBUTE_REPARSE_POINT));
        assert!(!allowed(
            FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT
        ));
        assert!(allowed(FILE_ATTRIBUTE_DIRECTORY));
    }
}
