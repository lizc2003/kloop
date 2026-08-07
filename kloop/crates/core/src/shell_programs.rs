use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;

use anyhow::bail;
use anyhow::Context as _;
use anyhow::Result;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellFlavor {
    PosixSh,
    GitBash,
    PowerShell7,
    WindowsPowerShell,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellProgram {
    pub executable: PathBuf,
    pub flavor: ShellFlavor,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellPrograms {
    pub bash: Option<ShellProgram>,
    pub powershell: Option<ShellProgram>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShellOverrides {
    pub bash: Option<PathBuf>,
    pub powershell: Option<PathBuf>,
}

#[derive(Clone, Debug, Default)]
pub struct ShellDiscoveryEnv {
    pub path: Option<OsString>,
    pub program_files: Option<PathBuf>,
    pub program_files_x86: Option<PathBuf>,
    pub local_app_data: Option<PathBuf>,
    pub powershell_msix_roots: Vec<PathBuf>,
    pub system_root: Option<PathBuf>,
}

// The Windows branch is structurally derivable, but the Unix branch intentionally
// resolves to a native POSIX shell instead of an empty catalog.
#[cfg_attr(windows, allow(clippy::derivable_impls))]
impl Default for ShellPrograms {
    fn default() -> Self {
        #[cfg(windows)]
        {
            Self {
                bash: None,
                powershell: None,
            }
        }
        #[cfg(not(windows))]
        {
            Self::native_posix()
        }
    }
}

impl ShellPrograms {
    pub fn native_posix() -> Self {
        Self {
            bash: Some(ShellProgram {
                executable: PathBuf::from("/bin/sh"),
                flavor: ShellFlavor::PosixSh,
            }),
            powershell: None,
        }
    }

    #[doc(hidden)]
    pub fn test_fixture() -> Self {
        #[cfg(windows)]
        {
            Self {
                bash: Some(ShellProgram {
                    executable: PathBuf::from(r"C:\Program Files\Git\bin\bash.exe"),
                    flavor: ShellFlavor::GitBash,
                }),
                powershell: Some(ShellProgram {
                    executable: PathBuf::from(
                        r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
                    ),
                    flavor: ShellFlavor::WindowsPowerShell,
                }),
            }
        }
        #[cfg(not(windows))]
        {
            Self::native_posix()
        }
    }

    pub fn bash_available(&self) -> bool {
        self.bash.is_some()
    }

    pub fn powershell_available(&self) -> bool {
        self.powershell.is_some()
    }
}

pub fn resolve_shell_programs(overrides: ShellOverrides) -> Result<(ShellPrograms, Vec<String>)> {
    #[cfg(windows)]
    {
        discover_windows(overrides, &ShellDiscoveryEnv::capture())
    }
    #[cfg(not(windows))]
    {
        if overrides.bash.is_some() || overrides.powershell.is_some() {
            bail!("[shells] executable overrides are only supported on native Windows");
        }
        let executable = if is_wsl() {
            canonical_posix_executable(Path::new("/bin/bash"), "WSL Bash")?
        } else {
            resolve_posix_sh(std::env::var_os("PATH").as_deref(), Path::new("/bin/sh"))?
        };
        Ok((
            ShellPrograms {
                bash: Some(ShellProgram {
                    executable,
                    flavor: ShellFlavor::PosixSh,
                }),
                powershell: None,
            },
            Vec::new(),
        ))
    }
}

impl ShellDiscoveryEnv {
    #[cfg(windows)]
    fn capture() -> Self {
        Self {
            path: std::env::var_os("PATH"),
            program_files: std::env::var_os("ProgramFiles").map(PathBuf::from),
            program_files_x86: std::env::var_os("ProgramFiles(x86)").map(PathBuf::from),
            local_app_data: std::env::var_os("LocalAppData").map(PathBuf::from),
            powershell_msix_roots: discover_powershell_msix_roots(),
            system_root: std::env::var_os("SystemRoot").map(PathBuf::from),
        }
    }
}

#[cfg(windows)]
fn discover_powershell_msix_roots() -> Vec<PathBuf> {
    // Exact package families bind both product name and Microsoft's publisher ID;
    // the user-writable app-execution alias directory is not a trust root.
    const OFFICIAL_FAMILIES: [&str; 2] = [
        "Microsoft.PowerShell_8wekyb3d8bbwe",
        "Microsoft.PowerShell-LTS_8wekyb3d8bbwe",
    ];

    let mut roots = Vec::new();
    for family in OFFICIAL_FAMILIES {
        if let Ok(paths) = package_roots_for_family(family) {
            for path in paths {
                push_unique(&mut roots, path);
            }
        }
    }
    roots
}

#[cfg(all(test, windows))]
pub(crate) fn official_msix_powershell_for_test() -> Option<ShellProgram> {
    discover_powershell(&ShellDiscoveryEnv {
        powershell_msix_roots: discover_powershell_msix_roots(),
        ..Default::default()
    })
}

#[cfg(windows)]
fn package_roots_for_family(family: &str) -> std::io::Result<Vec<PathBuf>> {
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::ffi::OsStringExt as _;

    use windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
    use windows_sys::Win32::Foundation::ERROR_NOT_FOUND;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::Storage::Packaging::Appx::GetPackagePathByFullName;
    use windows_sys::Win32::Storage::Packaging::Appx::GetPackagesByPackageFamily;

    let family: Vec<u16> = Path::new(family)
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let mut count = 0u32;
    let mut buffer_len = 0u32;
    let result = unsafe {
        GetPackagesByPackageFamily(
            family.as_ptr(),
            &mut count,
            std::ptr::null_mut(),
            &mut buffer_len,
            std::ptr::null_mut(),
        )
    };
    if result == ERROR_NOT_FOUND || (result == ERROR_SUCCESS && count == 0) {
        return Ok(Vec::new());
    }
    if result != ERROR_INSUFFICIENT_BUFFER {
        return Err(std::io::Error::from_raw_os_error(result as i32));
    }

    let mut names = vec![std::ptr::null_mut(); count as usize];
    let mut buffer = vec![0u16; buffer_len as usize];
    let result = unsafe {
        GetPackagesByPackageFamily(
            family.as_ptr(),
            &mut count,
            names.as_mut_ptr(),
            &mut buffer_len,
            buffer.as_mut_ptr(),
        )
    };
    if result != ERROR_SUCCESS {
        return Err(std::io::Error::from_raw_os_error(result as i32));
    }

    let base = buffer.as_ptr() as usize;
    let end = base + buffer.len() * std::mem::size_of::<u16>();
    let mut roots = Vec::new();
    for name in names.into_iter().take(count as usize) {
        let address = name as usize;
        if address < base || address >= end || !(address - base).is_multiple_of(2) {
            return Err(std::io::Error::other(
                "Windows package API returned an invalid package-name pointer",
            ));
        }
        let offset = (address - base) / std::mem::size_of::<u16>();
        let tail = &buffer[offset..];
        let length = tail.iter().position(|unit| *unit == 0).ok_or_else(|| {
            std::io::Error::other("Windows package API returned an unterminated package name")
        })?;
        let mut full_name = tail[..length].to_vec();
        full_name.push(0);

        let mut path_len = 0u32;
        let result = unsafe {
            GetPackagePathByFullName(full_name.as_ptr(), &mut path_len, std::ptr::null_mut())
        };
        if result != ERROR_INSUFFICIENT_BUFFER {
            continue;
        }
        let mut path = vec![0u16; path_len as usize];
        let result = unsafe {
            GetPackagePathByFullName(full_name.as_ptr(), &mut path_len, path.as_mut_ptr())
        };
        if result != ERROR_SUCCESS {
            continue;
        }
        if path.last() == Some(&0) {
            path.pop();
        }
        push_unique(&mut roots, PathBuf::from(OsString::from_wide(&path)));
    }
    Ok(roots)
}

#[cfg(not(windows))]
fn resolve_posix_sh(path: Option<&std::ffi::OsStr>, fallback: &Path) -> Result<PathBuf> {
    if let Some(resolved) = path
        .into_iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join("sh"))
        .find_map(|candidate| {
            is_posix_executable(&candidate)
                .then(|| candidate.canonicalize().ok())
                .flatten()
        })
    {
        return Ok(resolved);
    }
    canonical_posix_executable(fallback, "POSIX shell").with_context(|| {
        format!(
            "no executable sh was found on PATH and fallback '{}' is unavailable",
            fallback.display()
        )
    })
}

#[cfg(not(windows))]
fn canonical_posix_executable(path: &Path, label: &str) -> Result<PathBuf> {
    if !is_posix_executable(path) {
        bail!(
            "{label} '{}' is not an executable regular file",
            path.display()
        );
    }
    path.canonicalize()
        .with_context(|| format!("cannot canonicalize {label} '{}'", path.display()))
}

#[cfg(not(windows))]
fn is_posix_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    path.metadata().is_ok_and(|metadata| {
        metadata.file_type().is_file() && metadata.permissions().mode() & 0o111 != 0
    })
}

#[cfg(all(target_os = "linux", not(windows)))]
fn is_wsl() -> bool {
    std::env::var_os("WSL_INTEROP").is_some()
        || std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .is_ok_and(|release| release.to_ascii_lowercase().contains("microsoft"))
}

#[cfg(not(target_os = "linux"))]
#[cfg(not(windows))]
fn is_wsl() -> bool {
    false
}

pub fn discover_windows(
    overrides: ShellOverrides,
    env: &ShellDiscoveryEnv,
) -> Result<(ShellPrograms, Vec<String>)> {
    let bash = match overrides.bash {
        Some(path) => Some(
            validate_git_bash(&path)
                .with_context(|| format!("invalid [shells].bash '{}':", path.display()))?,
        ),
        None => discover_git_bash(env),
    };
    let powershell = match overrides.powershell {
        Some(path) => Some(
            validate_powershell(&path)
                .with_context(|| format!("invalid [shells].powershell '{}':", path.display()))?,
        ),
        None => discover_powershell(env),
    };
    let mut warnings = Vec::new();
    if bash.is_none() {
        warnings.push(
            "Git for Windows Bash was not found; bash, bash_output and stop_bash are unavailable. Install Git for Windows or set [shells].bash to its bin\\bash.exe."
                .to_string(),
        );
    }
    if powershell.is_none() {
        warnings.push(
            "PowerShell was not found in a trusted standard location; the powershell tool is unavailable. Set [shells].powershell to pwsh.exe or Windows PowerShell."
                .to_string(),
        );
    }
    Ok((ShellPrograms { bash, powershell }, warnings))
}

fn discover_git_bash(env: &ShellDiscoveryEnv) -> Option<ShellProgram> {
    let mut roots = Vec::new();
    if let Some(path) = &env.path {
        for dir in std::env::split_paths(path) {
            let git = dir.join("git.exe");
            if git.is_file() {
                if let Some(root) = git_install_root(&git) {
                    push_unique(&mut roots, root);
                }
            }
        }
    }
    for root in [
        env.program_files.as_ref().map(|path| path.join("Git")),
        env.program_files_x86.as_ref().map(|path| path.join("Git")),
        env.local_app_data
            .as_ref()
            .map(|path| path.join("Programs").join("Git")),
    ]
    .into_iter()
    .flatten()
    {
        push_unique(&mut roots, root);
    }
    roots
        .into_iter()
        .filter_map(|root| validate_git_root(&root).ok())
        .next()
}

fn git_install_root(git: &Path) -> Option<PathBuf> {
    let parent = git.parent()?;
    let name = parent.file_name()?.to_string_lossy();
    if name.eq_ignore_ascii_case("cmd") || name.eq_ignore_ascii_case("bin") {
        parent.parent().map(Path::to_path_buf)
    } else {
        None
    }
}

fn validate_git_bash(path: &Path) -> Result<ShellProgram> {
    validate_absolute_regular(path, "Git Bash")?;
    let file = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("Git Bash path has no Unicode file name")?;
    if !file.eq_ignore_ascii_case("bash.exe") {
        bail!("expected executable name bash.exe");
    }
    let bin = path.parent().context("Git Bash path has no parent")?;
    if !bin
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("bin"))
    {
        bail!("expected Git for Windows bin\\bash.exe layout");
    }
    validate_git_root(bin.parent().context("Git Bash path has no install root")?)
}

fn validate_git_root(root: &Path) -> Result<ShellProgram> {
    let bash = root.join("bin").join("bash.exe");
    let git = root.join("cmd").join("git.exe");
    let runtime = root.join("usr").join("bin").join("msys-2.0.dll");
    for (label, path) in [
        ("bash.exe", &bash),
        ("git.exe", &git),
        ("MSYS runtime", &runtime),
    ] {
        let regular =
            std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file());
        if !regular {
            bail!(
                "Git for Windows layout is missing {label} at {}",
                path.display()
            );
        }
    }
    Ok(ShellProgram {
        executable: bash
            .canonicalize()
            .context("cannot canonicalize Git Bash executable")?,
        flavor: ShellFlavor::GitBash,
    })
}

#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, Debug, PartialEq, Eq)]
enum PowerShellVersionProbe {
    Version(Vec<u32>),
    MissingResource,
    Invalid,
}

fn discover_powershell(env: &ShellDiscoveryEnv) -> Option<ShellProgram> {
    discover_powershell_with_version_probe(env, powershell_executable_version)
}

fn discover_powershell_with_version_probe(
    env: &ShellDiscoveryEnv,
    version_probe: impl Fn(&Path) -> PowerShellVersionProbe,
) -> Option<ShellProgram> {
    let mut candidates = Vec::new();
    for (base, local_install) in [
        env.program_files.as_ref().map(|path| (path, false)),
        env.local_app_data.as_ref().map(|path| (path, true)),
    ]
    .into_iter()
    .flatten()
    {
        let root = if local_install {
            base.join("Microsoft").join("PowerShell")
        } else {
            base.join("PowerShell")
        };
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if powershell_major(&name) != Some(7) {
                    continue;
                }
                let path = entry.path().join("pwsh.exe");
                let version = match version_probe(&path) {
                    PowerShellVersionProbe::Version(version) if version.first() == Some(&7) => {
                        Some(version)
                    }
                    PowerShellVersionProbe::MissingResource => Some(powershell_version_key(&name)),
                    PowerShellVersionProbe::Version(_) | PowerShellVersionProbe::Invalid => None,
                };
                if let Some(version) = version {
                    candidates.push((version, path));
                }
            }
        }
    }
    for root in &env.powershell_msix_roots {
        let path = root.join("pwsh.exe");
        let version = match version_probe(&path) {
            PowerShellVersionProbe::Version(version) if version.first() == Some(&7) => {
                Some(version)
            }
            PowerShellVersionProbe::MissingResource => powershell_msix_version_key(root),
            PowerShellVersionProbe::Version(_) | PowerShellVersionProbe::Invalid => None,
        };
        if let Some(version) = version {
            candidates.push((version, path));
        }
    }
    candidates.sort_by(|left, right| right.0.cmp(&left.0));
    if let Some(program) = candidates
        .into_iter()
        .filter_map(|(_, path)| validate_powershell(&path).ok())
        .next()
    {
        return Some(program);
    }
    let desktop = env
        .system_root
        .as_ref()?
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    validate_powershell(&desktop).ok()
}

fn validate_powershell(path: &Path) -> Result<ShellProgram> {
    validate_absolute_regular(path, "PowerShell")?;
    let file = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("PowerShell path has no Unicode file name")?;
    let flavor = if file.eq_ignore_ascii_case("pwsh.exe") {
        ShellFlavor::PowerShell7
    } else if file.eq_ignore_ascii_case("powershell.exe") {
        ShellFlavor::WindowsPowerShell
    } else {
        bail!("expected executable name pwsh.exe or powershell.exe");
    };
    Ok(ShellProgram {
        executable: path
            .canonicalize()
            .context("cannot canonicalize PowerShell executable")?,
        flavor,
    })
}

fn validate_absolute_regular(path: &Path, label: &str) -> Result<()> {
    if !path.is_absolute() {
        bail!("{label} path must be absolute and cannot contain arguments");
    }
    let metadata =
        std::fs::symlink_metadata(path).with_context(|| format!("cannot inspect {label}"))?;
    if !metadata.file_type().is_file() {
        bail!("{label} path must name a regular executable file");
    }
    Ok(())
}

#[cfg(windows)]
fn powershell_executable_version(path: &Path) -> PowerShellVersionProbe {
    use std::os::windows::ffi::OsStrExt as _;

    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::Foundation::ERROR_RESOURCE_DATA_NOT_FOUND;
    use windows_sys::Win32::Foundation::ERROR_RESOURCE_LANG_NOT_FOUND;
    use windows_sys::Win32::Foundation::ERROR_RESOURCE_NAME_NOT_FOUND;
    use windows_sys::Win32::Foundation::ERROR_RESOURCE_TYPE_NOT_FOUND;
    use windows_sys::Win32::Storage::FileSystem::GetFileVersionInfoSizeW;
    use windows_sys::Win32::Storage::FileSystem::GetFileVersionInfoW;
    use windows_sys::Win32::Storage::FileSystem::VerQueryValueW;
    use windows_sys::Win32::Storage::FileSystem::VS_FIXEDFILEINFO;

    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut ignored = 0u32;
    let size = unsafe { GetFileVersionInfoSizeW(path.as_ptr(), &mut ignored) };
    if size == 0 {
        return match unsafe { GetLastError() } {
            ERROR_RESOURCE_DATA_NOT_FOUND
            | ERROR_RESOURCE_LANG_NOT_FOUND
            | ERROR_RESOURCE_NAME_NOT_FOUND
            | ERROR_RESOURCE_TYPE_NOT_FOUND => PowerShellVersionProbe::MissingResource,
            _ => PowerShellVersionProbe::Invalid,
        };
    }
    let words = (size as usize).div_ceil(std::mem::size_of::<usize>());
    let mut data = vec![0usize; words];
    if unsafe { GetFileVersionInfoW(path.as_ptr(), 0, size, data.as_mut_ptr().cast()) } == 0 {
        return PowerShellVersionProbe::Invalid;
    }
    let query = [b'\\' as u16, 0];
    let mut value = std::ptr::null_mut();
    let mut value_len = 0u32;
    if unsafe {
        VerQueryValueW(
            data.as_ptr().cast(),
            query.as_ptr(),
            &mut value,
            &mut value_len,
        )
    } == 0
        || value_len < std::mem::size_of::<VS_FIXEDFILEINFO>() as u32
    {
        return PowerShellVersionProbe::Invalid;
    }
    let info = unsafe { std::ptr::read_unaligned(value.cast::<VS_FIXEDFILEINFO>()) };
    if info.dwSignature != 0xFEEF04BD {
        return PowerShellVersionProbe::Invalid;
    }
    PowerShellVersionProbe::Version(vec![
        info.dwFileVersionMS >> 16,
        info.dwFileVersionMS & 0xffff,
        info.dwFileVersionLS >> 16,
        info.dwFileVersionLS & 0xffff,
    ])
}

#[cfg(not(windows))]
fn powershell_executable_version(_path: &Path) -> PowerShellVersionProbe {
    PowerShellVersionProbe::MissingResource
}

fn powershell_msix_version_key(root: &Path) -> Option<Vec<u32>> {
    let full_name = root.file_name()?.to_str()?;
    let version = full_name.split('_').nth(1)?;
    (powershell_major(version) == Some(7)).then(|| powershell_version_key(version))
}

fn powershell_major(name: &str) -> Option<u32> {
    name.split(['.', '-']).next()?.parse().ok()
}

fn powershell_version_key(name: &str) -> Vec<u32> {
    name.split(['.', '-'])
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}

fn push_unique(paths: &mut Vec<PathBuf>, candidate: PathBuf) {
    if !paths.iter().any(|path| path == &candidate) {
        paths.push(candidate);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    static TEST_DIR_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "kloop-shell-programs-{tag}-{}-{}",
                std::process::id(),
                TEST_DIR_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"fixture").unwrap();
    }

    fn fake_git(root: &Path) -> PathBuf {
        let bash = root.join("bin").join("bash.exe");
        touch(&bash);
        touch(&root.join("cmd").join("git.exe"));
        touch(&root.join("usr").join("bin").join("msys-2.0.dll"));
        bash
    }

    #[test]
    fn git_bash_requires_the_complete_git_for_windows_layout() {
        let temp = TestDir::new("fixture");
        let root = temp.path().join("Git");
        let bash = fake_git(&root);
        let program = validate_git_bash(&bash).unwrap();
        assert_eq!(program.flavor, ShellFlavor::GitBash);
        assert_eq!(program.executable, bash.canonicalize().unwrap());

        std::fs::remove_file(root.join("usr/bin/msys-2.0.dll")).unwrap();
        let error = validate_git_bash(&bash).unwrap_err().to_string();
        assert!(error.contains("MSYS runtime"), "{error}");
    }

    #[test]
    fn discovery_accepts_validated_git_from_path_but_not_fake_bash() {
        let temp = TestDir::new("fixture");
        let root = temp.path().join("PortableGit");
        fake_git(&root);
        let fake = temp.path().join("fake");
        touch(&fake.join("bash.exe"));
        let env = ShellDiscoveryEnv {
            path: Some(std::env::join_paths([fake, root.join("cmd")]).unwrap()),
            ..Default::default()
        };
        let (programs, _) = discover_windows(ShellOverrides::default(), &env).unwrap();
        assert_eq!(programs.bash.unwrap().flavor, ShellFlavor::GitBash);
    }

    #[test]
    fn explicit_shell_paths_reject_embedded_arguments_and_wrong_flavor() {
        let relative = PathBuf::from("pwsh.exe -NoProfile");
        let error = validate_powershell(&relative).unwrap_err().to_string();
        assert!(error.contains("absolute"), "{error}");

        let temp = TestDir::new("fixture");
        let wrong = temp.path().join("cmd.exe");
        touch(&wrong);
        let error = validate_powershell(&wrong).unwrap_err().to_string();
        assert!(error.contains("pwsh.exe or powershell.exe"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn explicit_shell_paths_reject_symlink_executables() {
        let temp = TestDir::new("symlink");
        let target = temp.path().join("real/pwsh.exe");
        touch(&target);
        let alias = temp.path().join("alias/pwsh.exe");
        std::fs::create_dir_all(alias.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &alias).unwrap();
        let error = validate_powershell(&alias).unwrap_err().to_string();
        assert!(error.contains("regular executable"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn posix_shell_search_skips_non_executable_candidates() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = TestDir::new("posix-executable");
        let first = temp.path().join("first/sh");
        let second = temp.path().join("second/sh");
        touch(&first);
        touch(&second);
        std::fs::set_permissions(&first, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(&second, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path =
            std::env::join_paths([first.parent().unwrap(), second.parent().unwrap()]).unwrap();
        let resolved = resolve_posix_sh(Some(&path), &temp.path().join("missing-sh")).unwrap();
        assert_eq!(resolved, second.canonicalize().unwrap());

        let first_only = std::env::join_paths([first.parent().unwrap()]).unwrap();
        let error = resolve_posix_sh(Some(&first_only), &temp.path().join("missing-sh"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("no executable sh"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn posix_shell_search_validates_and_canonicalizes_symlink_targets() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = TestDir::new("posix-symlink");
        let target = temp.path().join("real-shell");
        touch(&target);
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        let alias = temp.path().join("bin/sh");
        std::fs::create_dir_all(alias.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &alias).unwrap();
        let path = std::env::join_paths([alias.parent().unwrap()]).unwrap();
        assert_eq!(
            resolve_posix_sh(Some(&path), &temp.path().join("missing-sh")).unwrap(),
            target.canonicalize().unwrap()
        );

        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = resolve_posix_sh(Some(&path), &temp.path().join("missing-sh"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("no executable sh"), "{error}");
    }

    #[test]
    fn powershell_discovery_prefers_highest_standard_v7_then_desktop() {
        let temp = TestDir::new("powershell");
        let program_files = temp.path().join("Program Files");
        let older = program_files.join("PowerShell/7.4.2/pwsh.exe");
        let newer = program_files.join("PowerShell/7.10.0/pwsh.exe");
        touch(&older);
        touch(&newer);
        let system_root = temp.path().join("Windows");
        touch(&system_root.join("System32/WindowsPowerShell/v1.0/powershell.exe"));
        let env = ShellDiscoveryEnv {
            program_files: Some(program_files),
            system_root: Some(system_root.clone()),
            ..Default::default()
        };
        let (programs, _) = discover_windows(ShellOverrides::default(), &env).unwrap();
        assert_eq!(
            programs.powershell.unwrap().executable,
            newer.canonicalize().unwrap()
        );

        std::fs::remove_file(&newer).unwrap();
        std::fs::remove_file(&older).unwrap();
        let (programs, _) = discover_windows(ShellOverrides::default(), &env).unwrap();
        assert_eq!(
            programs.powershell.unwrap().flavor,
            ShellFlavor::WindowsPowerShell
        );
    }

    #[test]
    fn powershell_discovery_prefers_newer_trusted_msix_package_root() {
        let temp = TestDir::new("powershell-msix");
        let program_files = temp.path().join("Program Files");
        let msi = program_files.join("PowerShell/7.10.0/pwsh.exe");
        touch(&msi);
        let msix_root = temp
            .path()
            .join("Microsoft.PowerShell_7.11.2.0_x64__8wekyb3d8bbwe");
        let msix = msix_root.join("pwsh.exe");
        touch(&msix);
        let env = ShellDiscoveryEnv {
            program_files: Some(program_files),
            powershell_msix_roots: vec![msix_root],
            ..Default::default()
        };
        let (programs, _) = discover_windows(ShellOverrides::default(), &env).unwrap();
        assert_eq!(
            programs.powershell.unwrap().executable,
            msix.canonicalize().unwrap()
        );
    }

    #[test]
    fn executable_version_beats_fixed_msi_directory_name_when_ranking_msix() {
        let temp = TestDir::new("powershell-msi-version");
        let program_files = temp.path().join("Program Files");
        let msi = program_files.join("PowerShell/7/pwsh.exe");
        touch(&msi);
        let msix_root = temp
            .path()
            .join("Microsoft.PowerShell_7.6.0.0_x64__8wekyb3d8bbwe");
        touch(&msix_root.join("pwsh.exe"));
        let env = ShellDiscoveryEnv {
            program_files: Some(program_files),
            powershell_msix_roots: vec![msix_root],
            ..Default::default()
        };

        let program = discover_powershell_with_version_probe(&env, |path| {
            if path == msi.as_path() {
                PowerShellVersionProbe::Version(vec![7, 10, 0, 0])
            } else {
                PowerShellVersionProbe::MissingResource
            }
        })
        .unwrap();
        assert_eq!(program.executable, msi.canonicalize().unwrap());
    }

    #[test]
    fn powershell_version_probe_distinguishes_non_v7_missing_and_invalid() {
        let temp = TestDir::new("powershell-probe-states");
        let program_files = temp.path().join("Program Files");
        let msi = program_files.join("PowerShell/7/pwsh.exe");
        touch(&msi);
        let msix_root = temp
            .path()
            .join("Microsoft.PowerShell_7.11.0.0_x64__8wekyb3d8bbwe");
        let msix = msix_root.join("pwsh.exe");
        touch(&msix);
        let system_root = temp.path().join("Windows");
        let desktop = system_root.join("System32/WindowsPowerShell/v1.0/powershell.exe");
        touch(&desktop);
        let env = ShellDiscoveryEnv {
            program_files: Some(program_files),
            powershell_msix_roots: vec![msix_root],
            system_root: Some(system_root),
            ..Default::default()
        };

        for probe in [
            PowerShellVersionProbe::Version(vec![8, 0, 0, 0]),
            PowerShellVersionProbe::Invalid,
        ] {
            let program = discover_powershell_with_version_probe(&env, |_| probe.clone()).unwrap();
            assert_eq!(program.executable, desktop.canonicalize().unwrap());
            assert_eq!(program.flavor, ShellFlavor::WindowsPowerShell);
        }

        let program = discover_powershell_with_version_probe(&env, |_| {
            PowerShellVersionProbe::MissingResource
        })
        .unwrap();
        assert_eq!(program.executable, msix.canonicalize().unwrap());
        assert_eq!(program.flavor, ShellFlavor::PowerShell7);
    }

    #[test]
    fn missing_git_bash_produces_one_actionable_warning() {
        let (programs, warnings) =
            discover_windows(ShellOverrides::default(), &ShellDiscoveryEnv::default()).unwrap();
        assert!(programs.bash.is_none());
        assert_eq!(
            warnings
                .iter()
                .filter(|warning| warning.contains("Git for Windows Bash"))
                .count(),
            1
        );
        assert!(warnings[0].contains("[shells].bash"));
    }

    #[test]
    fn powershell_versions_sort_numerically() {
        let mut versions = ["7.4.2", "7.10.0", "7.3.9"];
        versions.sort_by_key(|version| powershell_version_key(version));
        assert_eq!(versions, ["7.3.9", "7.4.2", "7.10.0"]);
    }
}
