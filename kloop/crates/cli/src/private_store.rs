//! Private application-owned file storage.
//!
//! Unix callers can anchor nested state below a private directory descriptor;
//! every component and leaf is opened without following symlinks. The simpler
//! path helpers preserve the same boundary for the global config and OAuth
//! store.

use std::ffi::OsStr;
#[cfg(not(any(unix, windows)))]
use std::fs::OpenOptions;
use std::io::Read as _;
use std::io::Write as _;
use std::path::Path;
#[cfg(not(any(unix, windows)))]
use std::path::PathBuf;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context as _;
use anyhow::Result;

pub(crate) fn read_private_string(path: &Path, label: &str) -> Result<Option<String>> {
    read_private_string_impl(path, label)
}

#[cfg(unix)]
fn read_private_string_impl(path: &Path, label: &str) -> Result<Option<String>> {
    let parent = path
        .parent()
        .with_context(|| format!("{label} has no parent directory"))?;
    let Some(dir) = open_private_dir(parent, label)? else {
        return Ok(None);
    };
    read_private_string_at(&dir, private_file_name(path, label)?, label)
}

#[cfg(windows)]
fn read_private_string_impl(path: &Path, label: &str) -> Result<Option<String>> {
    let parent = path
        .parent()
        .with_context(|| format!("{label} has no parent directory"))?;
    let Some(dir) = open_windows_directory_path(parent, label)? else {
        return Ok(None);
    };
    read_private_string_windows_at(&dir, private_file_name(path, label)?, label)
}

#[cfg(not(any(unix, windows)))]
fn read_private_string_impl(path: &Path, label: &str) -> Result<Option<String>> {
    let parent = path
        .parent()
        .with_context(|| format!("{label} has no parent directory"))?;
    if !inspect_private_dir(parent, label)? {
        return Ok(None);
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("{label} must be a regular file, not a symlink");
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => bail!("cannot inspect {label}"),
    }
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => bail!("cannot open {label}: {error}"),
    };
    read_opened_private_file(file, label).map(Some)
}

fn read_opened_private_file(mut file: std::fs::File, label: &str) -> Result<String> {
    validate_private_file(&file, label)?;
    let mut raw = String::new();
    file.read_to_string(&mut raw)
        .map_err(|_| anyhow!("cannot read {label}"))?;
    Ok(raw)
}

fn validate_private_file(file: &std::fs::File, label: &str) -> Result<()> {
    let metadata = file
        .metadata()
        .map_err(|_| anyhow!("cannot inspect {label}"))?;
    if !metadata.is_file() {
        bail!("{label} must be a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!("{label} contains credentials; restrict it to mode 0600");
        }
    }
    Ok(())
}

pub(crate) fn write_private_atomic(path: &Path, label: &str, bytes: &[u8]) -> Result<()> {
    write_private_atomic_impl(path, label, bytes)
}

#[cfg(unix)]
fn write_private_atomic_impl(path: &Path, label: &str, bytes: &[u8]) -> Result<()> {
    if let Some(existing) = read_private_string(path, label)? {
        drop(existing);
    }
    let parent = path
        .parent()
        .with_context(|| format!("{label} has no parent directory"))?;
    let dir = ensure_private_dir_open(parent, label)?;
    write_private_atomic_at(&dir, private_file_name(path, label)?, label, bytes)
}

#[cfg(windows)]
fn write_private_atomic_impl(path: &Path, label: &str, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("{label} has no parent directory"))?;
    let anchor_path = parent
        .parent()
        .with_context(|| format!("directory for {label} has no parent"))?;
    let parent_name = parent
        .file_name()
        .with_context(|| format!("directory for {label} has no name"))?;
    let anchor = open_windows_directory_path(anchor_path, label)?
        .with_context(|| format!("cannot open parent directory for {label}"))?;
    let dir = ensure_windows_private_dir_at(&anchor, parent_name, label)?;
    write_private_atomic_windows_at(&dir, private_file_name(path, label)?, label, bytes)
}

#[cfg(not(any(unix, windows)))]
fn write_private_atomic_impl(path: &Path, label: &str, bytes: &[u8]) -> Result<()> {
    if let Some(existing) = read_private_string(path, label)? {
        drop(existing);
    }
    let parent = path
        .parent()
        .with_context(|| format!("{label} has no parent directory"))?;
    ensure_private_dir(parent, label)?;
    write_private_atomic_path(parent, path.file_name().unwrap_or_default(), label, bytes)
}

#[cfg(any(unix, windows))]
fn private_file_name<'a>(path: &'a Path, label: &str) -> Result<&'a std::ffi::OsStr> {
    path.file_name()
        .with_context(|| format!("{label} has no file name"))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn private_dir_lookup_flags() -> rustix::fs::OFlags {
    rustix::fs::OFlags::PATH
}

#[cfg(target_os = "macos")]
fn private_dir_lookup_flags() -> rustix::fs::OFlags {
    rustix::fs::OFlags::from_bits_retain(libc::O_SEARCH as _)
}

#[cfg(all(
    unix,
    not(any(target_os = "linux", target_os = "android", target_os = "macos"))
))]
fn private_dir_lookup_flags() -> rustix::fs::OFlags {
    rustix::fs::OFlags::RDONLY
}

#[cfg(unix)]
fn private_dir_sync_flags() -> rustix::fs::OFlags {
    rustix::fs::OFlags::RDONLY
}

#[cfg(unix)]
fn validate_private_dir(dir: std::fs::File, label: &str) -> Result<std::fs::File> {
    use std::os::unix::fs::PermissionsExt as _;

    let metadata = dir
        .metadata()
        .map_err(|_| anyhow!("cannot inspect directory for {label}"))?;
    if !metadata.is_dir() {
        bail!("directory for {label} must be a regular directory, not a symlink");
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        bail!("directory for {label} must not be accessible by group or other");
    }
    Ok(dir)
}

#[cfg(unix)]
fn open_private_dir_with_flags(
    path: &Path,
    label: &str,
    access: rustix::fs::OFlags,
) -> Result<Option<std::fs::File>> {
    use rustix::fs::Mode;
    use rustix::fs::OFlags;

    let fd = match rustix::fs::open(
        path,
        access | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR) => {
            bail!("directory for {label} must be a regular directory, not a symlink");
        }
        Err(_) => bail!("cannot open directory for {label}"),
    };
    validate_private_dir(std::fs::File::from(fd), label).map(Some)
}

#[cfg(unix)]
fn open_private_dir(path: &Path, label: &str) -> Result<Option<std::fs::File>> {
    open_private_dir_with_flags(path, label, private_dir_lookup_flags())
}

#[cfg(unix)]
fn open_private_dir_at_with_flags(
    parent: &std::fs::File,
    name: &std::ffi::OsStr,
    label: &str,
    access: rustix::fs::OFlags,
) -> Result<Option<std::fs::File>> {
    use rustix::fs::Mode;
    use rustix::fs::OFlags;

    let fd = match rustix::fs::openat(
        parent,
        name,
        access | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR) => {
            bail!("directory for {label} must be a regular directory, not a symlink");
        }
        Err(_) => bail!("cannot open directory for {label}"),
    };
    validate_private_dir(std::fs::File::from(fd), label).map(Some)
}

#[cfg(unix)]
fn open_private_dir_at(
    parent: &std::fs::File,
    name: &std::ffi::OsStr,
    label: &str,
) -> Result<Option<std::fs::File>> {
    open_private_dir_at_with_flags(parent, name, label, private_dir_lookup_flags())
}

#[cfg(unix)]
fn open_private_dir_at_sync(
    parent: &std::fs::File,
    name: &std::ffi::OsStr,
    label: &str,
) -> Result<Option<std::fs::File>> {
    open_private_dir_at_with_flags(parent, name, label, private_dir_sync_flags())
}

#[cfg(unix)]
fn open_stable_parent_dir(path: &Path, label: &str) -> Result<std::fs::File> {
    use rustix::fs::Mode;
    use rustix::fs::OFlags;
    use std::os::unix::fs::PermissionsExt as _;

    let fd = rustix::fs::open(
        path,
        private_dir_sync_flags() | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|_| anyhow!("cannot open parent directory for {label}"))?;
    let dir = std::fs::File::from(fd);
    let metadata = dir
        .metadata()
        .map_err(|_| anyhow!("cannot inspect parent directory for {label}"))?;
    if !metadata.is_dir() {
        bail!("parent directory for {label} must be a regular directory, not a symlink");
    }
    if metadata.permissions().mode() & 0o022 != 0 {
        bail!("parent directory for {label} must not be writable by group or other");
    }
    Ok(dir)
}

#[cfg(unix)]
fn ensure_private_dir_open(path: &Path, label: &str) -> Result<std::fs::File> {
    let parent = path
        .parent()
        .with_context(|| format!("directory for {label} has no parent"))?;
    let name = private_file_name(path, label)?;
    let anchor = open_stable_parent_dir(parent, label)?;
    ensure_private_dir_at(&anchor, name, label)
}

#[cfg(unix)]
fn ensure_private_dir_at(
    parent: &std::fs::File,
    name: &std::ffi::OsStr,
    label: &str,
) -> Result<std::fs::File> {
    use rustix::fs::AtFlags;
    use rustix::fs::Mode;

    validate_component(name, label)?;
    if let Some(dir) = open_private_dir_at_sync(parent, name, label)? {
        parent
            .sync_all()
            .with_context(|| format!("cannot sync parent directory for {label}"))?;
        return Ok(dir);
    }
    let created = match rustix::fs::mkdirat(parent, name, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
        Ok(()) => true,
        Err(rustix::io::Errno::EXIST) => false,
        Err(_) => bail!("cannot create directory for {label}"),
    };
    if created {
        rustix::fs::chmodat(
            parent,
            name,
            Mode::RUSR | Mode::WUSR | Mode::XUSR,
            AtFlags::empty(),
        )
        .with_context(|| format!("cannot restrict directory for {label}"))?;
        parent
            .sync_all()
            .with_context(|| format!("cannot sync parent directory for {label}"))?;
    }
    open_private_dir_at_sync(parent, name, label)?
        .with_context(|| format!("cannot open directory for {label}"))
}

fn validate_component(name: &std::ffi::OsStr, label: &str) -> Result<()> {
    let path = Path::new(name);
    if path.components().count() != 1 || matches!(path.to_str(), Some(".") | Some("..") | Some(""))
    {
        bail!("invalid private path component for {label}");
    }
    Ok(())
}

#[cfg(unix)]
fn read_private_string_at(
    dir: &std::fs::File,
    name: &std::ffi::OsStr,
    label: &str,
) -> Result<Option<String>> {
    use rustix::fs::Mode;
    use rustix::fs::OFlags;

    validate_component(name, label)?;
    let fd = match rustix::fs::openat(
        dir,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(rustix::io::Errno::LOOP) => bail!("{label} must be a regular file, not a symlink"),
        Err(error) => bail!("cannot open {label}: {error}"),
    };
    read_opened_private_file(std::fs::File::from(fd), label).map(Some)
}

#[cfg(unix)]
fn write_private_atomic_at(
    dir: &std::fs::File,
    name: &std::ffi::OsStr,
    label: &str,
    bytes: &[u8],
) -> Result<()> {
    use rustix::fs::AtFlags;
    use rustix::fs::Mode;
    use rustix::fs::OFlags;

    validate_component(name, label)?;
    if let Some(existing) = read_private_string_at(dir, name, label)? {
        drop(existing);
    }
    let stem = name.to_string_lossy();
    for suffix in 0..100_u8 {
        let temp = format!(".{stem}.tmp-{}-{suffix}", std::process::id());
        let fd = match rustix::fs::openat(
            dir,
            temp.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        ) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::EXIST) => continue,
            Err(_) => bail!("cannot create temporary {label}"),
        };
        let mut file = std::fs::File::from(fd);
        let result = (|| -> Result<()> {
            rustix::fs::fchmod(&file, Mode::RUSR | Mode::WUSR)
                .with_context(|| format!("cannot restrict temporary {label}"))?;
            file.write_all(bytes)
                .with_context(|| format!("cannot write temporary {label}"))?;
            file.sync_all()
                .with_context(|| format!("cannot sync temporary {label}"))?;
            rustix::fs::renameat(dir, temp.as_str(), dir, name)
                .with_context(|| format!("cannot replace {label}"))?;
            dir.sync_all()
                .with_context(|| format!("cannot sync directory for {label}"))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = rustix::fs::unlinkat(dir, temp.as_str(), AtFlags::empty());
        }
        return result;
    }
    bail!("cannot create temporary {label}")
}

#[cfg(unix)]
fn open_private_lock_at(
    dir: &std::fs::File,
    name: &std::ffi::OsStr,
    label: &str,
) -> Result<std::fs::File> {
    use rustix::fs::Mode;
    use rustix::fs::OFlags;

    validate_component(name, label)?;
    let common = OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    let fd = loop {
        match rustix::fs::openat(
            dir,
            name,
            common | OFlags::CREATE | OFlags::EXCL,
            Mode::RUSR | Mode::WUSR,
        ) {
            Ok(fd) => break fd,
            Err(rustix::io::Errno::EXIST) => {
                match rustix::fs::openat(dir, name, common, Mode::empty()) {
                    Ok(fd) => break fd,
                    Err(rustix::io::Errno::NOENT) => {
                        std::thread::yield_now();
                        continue;
                    }
                    Err(rustix::io::Errno::LOOP) => {
                        bail!("{label} must be a regular file, not a symlink");
                    }
                    Err(error) => bail!("cannot open {label}: {error}"),
                }
            }
            Err(rustix::io::Errno::LOOP) => {
                bail!("{label} must be a regular file, not a symlink");
            }
            Err(error) => bail!("cannot open {label}: {error}"),
        }
    };
    let file = std::fs::File::from(fd);
    rustix::fs::fchmod(&file, Mode::RUSR | Mode::WUSR)
        .with_context(|| format!("cannot restrict {label}"))?;
    validate_private_file(&file, label)?;
    Ok(file)
}

#[cfg(not(any(unix, windows)))]
fn inspect_private_dir(path: &Path, label: &str) -> Result<bool> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => bail!("cannot inspect directory for {label}"),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("directory for {label} must be a regular directory, not a symlink");
    }
    Ok(true)
}

#[cfg(windows)]
fn windows_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt as _;

    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

#[cfg(windows)]
#[derive(Clone, Copy)]
enum WindowsOpenResultKind {
    Opened,
    Missing,
    Exists,
}

#[cfg(windows)]
struct WindowsOpenResult {
    kind: WindowsOpenResultKind,
    file: Option<std::fs::File>,
}

#[cfg(windows)]
fn validate_windows_opened_file(file: &std::fs::File, label: &str, directory: bool) -> Result<()> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle;
    use windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

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

#[cfg(windows)]
fn open_windows_directory_path(path: &Path, label: &str) -> Result<Option<std::fs::File>> {
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

    let path = windows_path(path);
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
    validate_windows_opened_file(&file, label, true)?;
    Ok(Some(file))
}

#[cfg(windows)]
fn open_windows_relative(
    parent: &std::fs::File,
    name: &OsStr,
    label: &str,
    directory: bool,
    desired_access: u32,
    disposition: u32,
) -> Result<WindowsOpenResult> {
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::io::AsRawHandle as _;
    use std::os::windows::io::FromRawHandle as _;
    use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows_sys::Wdk::Storage::FileSystem::NtCreateFile;
    use windows_sys::Wdk::Storage::FileSystem::FILE_DIRECTORY_FILE;
    use windows_sys::Wdk::Storage::FileSystem::FILE_NON_DIRECTORY_FILE;
    use windows_sys::Wdk::Storage::FileSystem::FILE_OPEN_REPARSE_POINT;
    use windows_sys::Wdk::Storage::FileSystem::FILE_SYNCHRONOUS_IO_NONALERT;
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
    use windows_sys::Win32::System::Kernel::OBJ_CASE_INSENSITIVE;
    use windows_sys::Win32::System::Kernel::OBJ_DONT_REPARSE;
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

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
        return Ok(WindowsOpenResult {
            kind: WindowsOpenResultKind::Missing,
            file: None,
        });
    }
    if status == STATUS_OBJECT_NAME_COLLISION {
        return Ok(WindowsOpenResult {
            kind: WindowsOpenResultKind::Exists,
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
    validate_windows_opened_file(&file, label, directory)?;
    Ok(WindowsOpenResult {
        kind: WindowsOpenResultKind::Opened,
        file: Some(file),
    })
}

#[cfg(windows)]
fn sync_windows_directory_file(dir: &std::fs::File, label: &str) -> Result<()> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::FlushFileBuffers;

    let synced =
        unsafe { FlushFileBuffers(dir.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE) };
    if synced == 0 {
        bail!("cannot sync directory for {label}");
    }
    Ok(())
}

#[cfg(windows)]
fn open_windows_private_dir_at(
    parent: &std::fs::File,
    name: &OsStr,
    label: &str,
) -> Result<Option<std::fs::File>> {
    use windows_sys::Wdk::Storage::FileSystem::FILE_OPEN;
    use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;
    use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE;

    let opened = open_windows_relative(
        parent,
        name,
        label,
        true,
        FILE_GENERIC_READ | FILE_GENERIC_WRITE,
        FILE_OPEN,
    )?;
    Ok(opened.file)
}

#[cfg(windows)]
fn ensure_windows_private_dir_at(
    parent: &std::fs::File,
    name: &OsStr,
    label: &str,
) -> Result<std::fs::File> {
    use windows_sys::Wdk::Storage::FileSystem::FILE_OPEN_IF;
    use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;
    use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE;

    let opened = open_windows_relative(
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
    sync_windows_directory_file(parent, label)?;
    Ok(file)
}

#[cfg(windows)]
fn read_private_string_windows_at(
    dir: &std::fs::File,
    name: &OsStr,
    label: &str,
) -> Result<Option<String>> {
    use windows_sys::Wdk::Storage::FileSystem::FILE_OPEN;
    use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;

    let opened = open_windows_relative(dir, name, label, false, FILE_GENERIC_READ, FILE_OPEN)?;
    match opened.file {
        Some(file) => read_opened_private_file(file, label).map(Some),
        None => Ok(None),
    }
}

#[cfg(windows)]
fn rename_windows_private_file(
    file: &std::fs::File,
    dir: &std::fs::File,
    name: &OsStr,
    label: &str,
) -> Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::FileRenameInfo;
    use windows_sys::Win32::Storage::FileSystem::SetFileInformationByHandle;
    use windows_sys::Win32::Storage::FileSystem::FILE_RENAME_INFO_0;

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

#[cfg(windows)]
fn delete_windows_private_file(file: &std::fs::File) {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::FileDispositionInfo;
    use windows_sys::Win32::Storage::FileSystem::SetFileInformationByHandle;
    use windows_sys::Win32::Storage::FileSystem::FILE_DISPOSITION_INFO;

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

#[cfg(windows)]
fn write_private_atomic_windows_at(
    dir: &std::fs::File,
    name: &OsStr,
    label: &str,
    bytes: &[u8],
) -> Result<()> {
    use windows_sys::Wdk::Storage::FileSystem::FILE_CREATE;
    use windows_sys::Win32::Storage::FileSystem::DELETE;
    use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE;

    if let Some(existing) = read_private_string_windows_at(dir, name, label)? {
        drop(existing);
    }
    let stem = name.to_string_lossy();
    for suffix in 0..100_u8 {
        let temp = format!(".{stem}.tmp-{}-{suffix}", std::process::id());
        let opened = open_windows_relative(
            dir,
            OsStr::new(&temp),
            label,
            false,
            FILE_GENERIC_WRITE | DELETE,
            FILE_CREATE,
        )?;
        if matches!(opened.kind, WindowsOpenResultKind::Exists) {
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
            rename_windows_private_file(&file, dir, name, label)?;
            sync_windows_directory_file(dir, label)
        })();
        if result.is_err() {
            delete_windows_private_file(&file);
        }
        return result;
    }
    bail!("cannot create temporary {label}")
}

#[cfg(windows)]
fn open_windows_private_lock_at(
    dir: &std::fs::File,
    name: &OsStr,
    label: &str,
) -> Result<std::fs::File> {
    use windows_sys::Wdk::Storage::FileSystem::FILE_OPEN_IF;
    use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;
    use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE;

    let opened = open_windows_relative(
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

#[cfg(all(not(unix), not(windows)))]
fn sync_private_directory(path: &Path, label: &str) -> Result<()> {
    std::fs::File::open(path)
        .with_context(|| format!("cannot open parent directory for {label}"))?
        .sync_all()
        .with_context(|| format!("cannot sync parent directory for {label}"))
}

#[cfg(all(not(unix), not(windows)))]
fn replace_private_path(temp: &Path, path: &Path, label: &str) -> Result<()> {
    std::fs::rename(temp, path).with_context(|| format!("cannot replace {label}"))
}

#[cfg(not(any(unix, windows)))]
fn ensure_private_dir(path: &Path, label: &str) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("directory for {label} has no parent"))?;
    if inspect_private_dir(path, label)? {
        return sync_private_directory(parent, label);
    }
    std::fs::create_dir(path).with_context(|| format!("cannot create directory for {label}"))?;
    sync_private_directory(parent, label)
}

#[cfg(not(any(unix, windows)))]
fn write_private_atomic_path(
    parent: &Path,
    name: &std::ffi::OsStr,
    label: &str,
    bytes: &[u8],
) -> Result<()> {
    validate_component(name, label)?;
    let path = parent.join(name);
    if let Some(existing) = read_private_string(&path, label)? {
        drop(existing);
    }
    let stem = name.to_string_lossy();
    for suffix in 0..100_u8 {
        let temp = parent.join(format!(".{stem}.tmp-{}-{suffix}", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        match options.open(&temp) {
            Ok(mut file) => {
                let result = (|| -> Result<()> {
                    file.write_all(bytes)
                        .with_context(|| format!("cannot write temporary {label}"))?;
                    file.sync_all()
                        .with_context(|| format!("cannot sync temporary {label}"))?;
                    replace_private_path(&temp, &path, label)?;
                    sync_private_directory(parent, label)?;
                    Ok(())
                })();
                if result.is_err() {
                    let _ = std::fs::remove_file(&temp);
                }
                return result;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => bail!("cannot create temporary {label}"),
        }
    }
    bail!("cannot create temporary {label}")
}

pub(crate) struct PrivateDir {
    #[cfg(any(unix, windows))]
    file: std::fs::File,
    #[cfg(not(any(unix, windows)))]
    path: PathBuf,
}

impl PrivateDir {
    pub(crate) fn open(root: &Path, components: &[&OsStr], label: &str) -> Result<Option<Self>> {
        #[cfg(unix)]
        {
            let Some(mut current) = open_private_dir(root, label)? else {
                return Ok(None);
            };
            for component in components {
                validate_component(component, label)?;
                let Some(next) = open_private_dir_at(&current, component, label)? else {
                    return Ok(None);
                };
                current = next;
            }
            Ok(Some(Self { file: current }))
        }
        #[cfg(windows)]
        {
            let Some(mut current) = open_windows_directory_path(root, label)? else {
                return Ok(None);
            };
            for component in components {
                let Some(next) = open_windows_private_dir_at(&current, component, label)? else {
                    return Ok(None);
                };
                current = next;
            }
            Ok(Some(Self { file: current }))
        }
        #[cfg(not(any(unix, windows)))]
        {
            if !inspect_private_dir(root, label)? {
                return Ok(None);
            }
            let mut current = root.to_path_buf();
            for component in components {
                validate_component(component, label)?;
                current.push(component);
                if !inspect_private_dir(&current, label)? {
                    return Ok(None);
                }
            }
            Ok(Some(Self { path: current }))
        }
    }

    pub(crate) fn ensure(root: &Path, components: &[&OsStr], label: &str) -> Result<Self> {
        #[cfg(unix)]
        {
            let mut current = ensure_private_dir_open(root, label)?;
            for component in components {
                current = ensure_private_dir_at(&current, component, label)?;
            }
            Ok(Self { file: current })
        }
        #[cfg(windows)]
        {
            let parent = root
                .parent()
                .with_context(|| format!("directory for {label} has no parent"))?;
            let name = root
                .file_name()
                .with_context(|| format!("directory for {label} has no name"))?;
            let parent = open_windows_directory_path(parent, label)?
                .with_context(|| format!("cannot open parent directory for {label}"))?;
            let mut current = ensure_windows_private_dir_at(&parent, name, label)?;
            for component in components {
                current = ensure_windows_private_dir_at(&current, component, label)?;
            }
            Ok(Self { file: current })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let parent = root
                .parent()
                .with_context(|| format!("directory for {label} has no parent"))?;
            if !inspect_private_dir(parent, label)? {
                bail!("cannot open parent directory for {label}");
            }
            ensure_private_dir(root, label)?;
            let mut current = root.to_path_buf();
            for component in components {
                validate_component(component, label)?;
                current.push(component);
                ensure_private_dir(&current, label)?;
            }
            Ok(Self { path: current })
        }
    }

    pub(crate) fn read_string(&self, name: &OsStr, label: &str) -> Result<Option<String>> {
        #[cfg(unix)]
        {
            read_private_string_at(&self.file, name, label)
        }
        #[cfg(windows)]
        {
            read_private_string_windows_at(&self.file, name, label)
        }
        #[cfg(not(any(unix, windows)))]
        {
            read_private_string(&self.path.join(name), label)
        }
    }

    pub(crate) fn write_atomic(&self, name: &OsStr, label: &str, bytes: &[u8]) -> Result<()> {
        #[cfg(unix)]
        {
            write_private_atomic_at(&self.file, name, label, bytes)
        }
        #[cfg(windows)]
        {
            write_private_atomic_windows_at(&self.file, name, label, bytes)
        }
        #[cfg(not(any(unix, windows)))]
        {
            write_private_atomic_path(&self.path, name, label, bytes)
        }
    }

    pub(crate) fn open_lock(&self, name: &OsStr, label: &str) -> Result<ExclusiveFileLock> {
        #[cfg(unix)]
        let file = open_private_lock_at(&self.file, name, label)?;
        #[cfg(windows)]
        let file = open_windows_private_lock_at(&self.file, name, label)?;
        #[cfg(not(any(unix, windows)))]
        let file = {
            validate_component(name, label)?;
            let path = self.path.join(name);
            match std::fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    bail!("{label} must be a regular file, not a symlink");
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => bail!("cannot inspect {label}"),
            }
            let mut options = OpenOptions::new();
            options.read(true).write(true).create(true);
            let file = options
                .open(path)
                .map_err(|_| anyhow!("cannot open {label}"))?;
            validate_private_file(&file, label)?;
            file
        };
        ExclusiveFileLock::acquire(file, label)
    }
}

pub(crate) struct ExclusiveFileLock {
    file: std::fs::File,
}

impl ExclusiveFileLock {
    fn acquire(file: std::fs::File, label: &str) -> Result<Self> {
        lock_file(&file).with_context(|| format!("cannot lock {label}"))?;
        Ok(Self { file })
    }
}

impl Drop for ExclusiveFileLock {
    fn drop(&mut self) {
        let _ = unlock_file(&self.file);
    }
}

#[cfg(unix)]
fn lock_file(file: &std::fs::File) -> Result<()> {
    rustix::fs::flock(file, rustix::fs::FlockOperation::LockExclusive).context("lock private store")
}

#[cfg(unix)]
fn unlock_file(file: &std::fs::File) -> Result<()> {
    rustix::fs::flock(file, rustix::fs::FlockOperation::Unlock).context("unlock private store")
}

#[cfg(windows)]
fn lock_file(file: &std::fs::File) -> Result<()> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::LockFileEx;
    use windows_sys::Win32::Storage::FileSystem::LOCKFILE_EXCLUSIVE_LOCK;
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

#[cfg(windows)]
fn unlock_file(file: &std::fs::File) -> Result<()> {
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

#[cfg(not(any(unix, windows)))]
fn lock_file(_file: &std::fs::File) -> Result<()> {
    bail!("private store locking is unsupported on this platform")
}

#[cfg(not(any(unix, windows)))]
fn unlock_file(_file: &std::fs::File) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::process::Command;

    use super::*;

    fn root(tag: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("kloop-private-store-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        path
    }

    #[test]
    fn nested_private_directory_round_trip() {
        let root = root("nested");
        let parent = root.parent().unwrap();
        assert!(parent.is_dir());
        let dir = PrivateDir::ensure(
            &root,
            &[
                OsStr::new("projects"),
                OsStr::new("v1"),
                OsStr::new("p1_test"),
            ],
            "test private store",
        )
        .unwrap();
        dir.write_atomic(OsStr::new("permissions.json"), "test policy", b"secret")
            .unwrap();
        assert_eq!(
            dir.read_string(OsStr::new("permissions.json"), "test policy")
                .unwrap(),
            Some("secret".into())
        );
        let reopened = PrivateDir::open(
            &root,
            &[
                OsStr::new("projects"),
                OsStr::new("v1"),
                OsStr::new("p1_test"),
            ],
            "test private store",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            reopened
                .read_string(OsStr::new("permissions.json"), "test policy")
                .unwrap(),
            Some("secret".into())
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn nested_private_directory_rejects_symlink_component_and_fifo_leaf() {
        use std::os::unix::fs::symlink;

        let root = root("unsafe");
        let dir =
            PrivateDir::ensure(&root, &[OsStr::new("projects")], "test private store").unwrap();
        let outside = root.with_file_name(format!(
            "{}-outside",
            root.file_name().unwrap().to_string_lossy()
        ));
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, root.join("projects/v1")).unwrap();
        assert!(PrivateDir::open(
            &root,
            &[OsStr::new("projects"), OsStr::new("v1")],
            "test private store"
        )
        .is_err());

        let fifo = root.join("projects/policy");
        let status = Command::new("mkfifo").arg(&fifo).status().unwrap();
        assert!(status.success());
        assert!(dir
            .read_string(OsStr::new("policy"), "test policy")
            .is_err());
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(outside);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn abrupt_process_exit_releases_descriptor_lock() {
        const CHILD_ROOT: &str = "KLOOP_PRIVATE_LOCK_EXIT_CHILD_ROOT";
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            let dir = PrivateDir::ensure(
                Path::new(&root),
                &[OsStr::new("projects")],
                "test private store",
            )
            .unwrap();
            let _lock = dir
                .open_lock(OsStr::new("policy.lock"), "test policy")
                .unwrap();
            std::process::exit(17);
        }

        let root = root("lock-exit");
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "private_store::tests::abrupt_process_exit_releases_descriptor_lock",
                "--test-threads=1",
            ])
            .env(CHILD_ROOT, &root)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(17));

        let dir = PrivateDir::open(&root, &[OsStr::new("projects")], "test private store")
            .unwrap()
            .unwrap();
        let _lock = dir
            .open_lock(OsStr::new("policy.lock"), "test policy")
            .unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn nested_private_directory_rejects_junction_component() {
        let root = root("junction");
        PrivateDir::ensure(&root, &[OsStr::new("projects")], "test private store").unwrap();
        let outside = root.with_file_name(format!(
            "{}-outside",
            root.file_name().unwrap().to_string_lossy()
        ));
        std::fs::create_dir_all(&outside).unwrap();
        let junction = root.join("projects/v1");
        let status = Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                junction.to_str().unwrap(),
                outside.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert!(status.success(), "mklink /J must be available on NTFS CI");
        let error = PrivateDir::open(
            &root,
            &[OsStr::new("projects"), OsStr::new("v1")],
            "test private store",
        )
        .err()
        .expect("junction component must be rejected");
        assert!(error.to_string().contains("reparse"), "{error:#}");

        let _ = std::fs::remove_dir_all(junction);
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(outside);
    }

    #[cfg(unix)]
    #[test]
    fn created_directories_and_files_are_private_despite_umask() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = root("mode");
        let dir =
            PrivateDir::ensure(&root, &[OsStr::new("projects")], "test private store").unwrap();
        dir.write_atomic(OsStr::new("policy"), "test policy", b"x")
            .unwrap();
        let lock = dir
            .open_lock(OsStr::new("policy.lock"), "test policy")
            .unwrap();
        assert_eq!(
            std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(root.join("projects"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(root.join("projects/policy"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(root.join("projects/policy.lock"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        drop(lock);
        let names = std::fs::read_dir(root.join("projects"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&OsStr::new("policy").to_os_string()));
        assert!(names.contains(&OsStr::new("policy.lock").to_os_string()));
        let _ = std::fs::remove_dir_all(root);
    }
}
