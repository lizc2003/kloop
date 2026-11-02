//! The one way kloop runs Git.
//!
//! Every invocation names its directory with `-C` and drops the repository
//! selection variables Git inherits. kloop can start from inside a Git hook or
//! `git rebase -x`, where `GIT_DIR`/`GIT_WORK_TREE`/`GIT_INDEX_FILE` point at
//! another checkout; without this, worktree lifecycle, the opening snapshot and
//! the scheduler partition would describe that checkout instead of the session
//! cwd — and a shared `GIT_INDEX_FILE` would let a status in one worktree
//! rewrite another repository's index.
//!
//! Global and system config stay in play by default: worktree mutations are
//! the user's Git. Identity probing opts out through [`hermetic_command`].

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// Variables that make Git read a different repository than the one `-C`
/// names. They are inherited from hooks and wrappers, never part of what kloop
/// means by "the repository at this directory".
pub const REPOSITORY_SELECTION_ENV_VARS: [&str; 12] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_INDEX_FILE",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_SYSTEM",
];
#[cfg(unix)]
const NULL_CONFIG: &str = "/dev/null";
#[cfg(windows)]
const NULL_CONFIG: &str = "NUL";
#[cfg(not(any(unix, windows)))]
const NULL_CONFIG: &str = "/dev/null";

/// Git aimed at `dir`, inheriting the user's configuration.
pub fn command(dir: &Path) -> Command {
    let mut command = Command::new("git");
    configure(&mut command, dir);
    command
}

/// Git aimed at `dir`, with the user's global and system configuration
/// suppressed. Identity probing must not be steerable by the machine's config
/// (nor by a `core.worktree` pointing somewhere else).
pub fn hermetic_command(dir: &Path) -> Command {
    let mut command = command(dir);
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", NULL_CONFIG);
    command
}

pub(crate) fn tokio_command(dir: &Path) -> tokio::process::Command {
    tokio::process::Command::from(command(dir))
}

#[cfg(test)]
pub(crate) fn tokio_hermetic_command(dir: &Path) -> tokio::process::Command {
    tokio::process::Command::from(hermetic_command(dir))
}

/// The current branch at `dir`, or `None` when it is not a repository (or Git
/// is unavailable). Best effort: a display and prompt nicety, not a
/// correctness path.
pub fn branch(dir: &Path) -> Option<String> {
    let mut command = command(dir);
    command.args(["rev-parse", "--abbrev-ref", "HEAD"]);
    let output = kloop_process_spawn::output_std(&mut command).ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!branch.is_empty()).then_some(branch)
}

fn configure(command: &mut Command, dir: &Path) {
    command.arg("-C").arg(git_compatible_path(dir));
    command.env("LC_ALL", "C").env("LANG", "C");
    for name in REPOSITORY_SELECTION_ENV_VARS {
        command.env_remove(name);
    }
    #[cfg(test)]
    apply_test_environment(command, dir);
}

#[cfg(windows)]
pub(crate) fn git_compatible_path(path: &Path) -> PathBuf {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::ffi::OsStringExt as _;

    const VERBATIM: &[u16] = &[b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];
    const UNC: &[u16] = &[b'U' as u16, b'N' as u16, b'C' as u16, b'\\' as u16];

    let units: Vec<u16> = path.as_os_str().encode_wide().collect();
    if !units.starts_with(VERBATIM) {
        return path.to_path_buf();
    }
    let suffix = &units[VERBATIM.len()..];
    let compatible = if suffix.starts_with(UNC) {
        [b'\\' as u16, b'\\' as u16]
            .into_iter()
            .chain(suffix[UNC.len()..].iter().copied())
            .collect()
    } else {
        suffix.to_vec()
    };
    PathBuf::from(OsString::from_wide(&compatible))
}

#[cfg(not(windows))]
pub(crate) fn git_compatible_path(path: &Path) -> PathBuf {
    path.to_path_buf()
}

/// Tests can give a repository its own HOME and config, so a host with
/// `commit.gpgSign` or an external `core.hooksPath` cannot fail a fixture.
#[cfg(test)]
#[derive(Clone)]
struct TestGitEnvironment {
    root: PathBuf,
    home: PathBuf,
    xdg_config_home: PathBuf,
}

#[cfg(test)]
static TEST_GIT_ENVIRONMENTS: std::sync::OnceLock<std::sync::Mutex<Vec<TestGitEnvironment>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
pub(crate) struct TestGitEnvironmentGuard {
    root: PathBuf,
}

#[cfg(test)]
impl Drop for TestGitEnvironmentGuard {
    fn drop(&mut self) {
        let environments = TEST_GIT_ENVIRONMENTS.get_or_init(|| std::sync::Mutex::new(Vec::new()));
        environments
            .lock()
            .unwrap()
            .retain(|environment| environment.root != self.root);
    }
}

#[cfg(test)]
pub(crate) fn isolate_test_git_environment(
    root: &Path,
    home: &Path,
    xdg_config_home: &Path,
) -> TestGitEnvironmentGuard {
    let environments = TEST_GIT_ENVIRONMENTS.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    let mut environments = environments.lock().unwrap();
    assert!(
        environments
            .iter()
            .all(|environment| environment.root != root),
        "duplicate test Git environment for {}",
        root.display()
    );
    environments.push(TestGitEnvironment {
        root: root.to_path_buf(),
        home: home.to_path_buf(),
        xdg_config_home: xdg_config_home.to_path_buf(),
    });
    TestGitEnvironmentGuard {
        root: root.to_path_buf(),
    }
}

#[cfg(test)]
fn apply_test_environment(command: &mut Command, dir: &Path) {
    let environments = TEST_GIT_ENVIRONMENTS.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    let environment = environments
        .lock()
        .unwrap()
        .iter()
        .filter(|environment| dir.starts_with(&environment.root))
        .max_by_key(|environment| environment.root.components().count())
        .cloned();
    let Some(environment) = environment else {
        return;
    };
    let path = std::env::var_os("PATH");
    command.env_clear();
    if let Some(path) = path {
        command.env("PATH", path);
    }
    command
        .env("HOME", environment.home)
        .env("XDG_CONFIG_HOME", environment.xdg_config_home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", NULL_CONFIG)
        .env("GIT_CONFIG_SYSTEM", NULL_CONFIG)
        .env("LC_ALL", "C");
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    #[test]
    fn command_names_the_directory_and_drops_inherited_repository_selection() {
        let command = command(Path::new("/tmp/example"));
        let arguments: Vec<_> = command.get_args().collect();
        assert_eq!(arguments[0], OsStr::new("-C"));
        assert_eq!(arguments[1], OsStr::new("/tmp/example"));
        for name in REPOSITORY_SELECTION_ENV_VARS {
            assert!(
                command
                    .get_envs()
                    .any(|(key, value)| key == OsStr::new(name) && value.is_none()),
                "{name} was inherited by the Git command"
            );
        }
        let configured = |name: &str| {
            command
                .get_envs()
                .find(|(key, _)| *key == OsStr::new(name))
                .and_then(|(_, value)| value)
        };
        assert_eq!(configured("LC_ALL"), Some(OsStr::new("C")));
        assert_eq!(configured("GIT_CONFIG_GLOBAL"), None);
    }

    #[test]
    fn hermetic_command_suppresses_global_and_system_config() {
        let command = hermetic_command(Path::new("/tmp/example"));
        let configured = |name: &str| {
            command
                .get_envs()
                .find(|(key, _)| *key == OsStr::new(name))
                .and_then(|(_, value)| value)
        };
        assert_eq!(configured("GIT_CONFIG_NOSYSTEM"), Some(OsStr::new("1")));
        assert_eq!(
            configured("GIT_CONFIG_GLOBAL"),
            Some(OsStr::new(NULL_CONFIG))
        );
    }

    #[test]
    fn branch_reads_the_checkout_it_is_pointed_at() {
        let root = std::env::temp_dir().join(format!(
            "kloop-git-branch-{}-{}",
            std::process::id(),
            crate::resource_id::fresh("t").unwrap()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut init = hermetic_command(&root);
        init.args(["init", "-q", "-b", "trunk"]);
        assert!(init.status().unwrap().success());
        let mut commit = hermetic_command(&root);
        commit.args([
            "-c",
            "user.name=kloop",
            "-c",
            "user.email=kloop@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "base",
        ]);
        assert!(commit.status().unwrap().success());

        assert_eq!(branch(&root).as_deref(), Some("trunk"));
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(branch(&nested).as_deref(), Some("trunk"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
