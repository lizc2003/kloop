//! Path spellings shared by the CLI, permissions, and the TUI: lexical
//! normalization, `~` expansion, and the inverse contraction for display.
//!
//! These are lexical by contract — they never stat or canonicalize, because the
//! caller may be judging a path that does not exist yet. A caller that needs
//! the physical target resolves it separately.

use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

/// Resolve `.`/`..` lexically, anchoring relative paths at `cwd`.
pub fn lexically_normalize(cwd: &Path, raw: &Path) -> PathBuf {
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd.join(raw)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Expand a leading `~` or `~/…` with `home`. A bare `~` is the home
/// directory; `~user` is not a spelling kloop expands and is left alone.
pub fn expand_tilde(raw: &str, home: Option<&Path>) -> PathBuf {
    let Some(home) = home else {
        return PathBuf::from(raw);
    };
    if raw == "~" {
        home.to_path_buf()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(raw)
    }
}

/// `path` with the process home directory written as `~`, for banners and
/// listings. `$HOME` is deliberately not consulted: kloop's path decisions use
/// the process home, and a display that disagreed with them would name a
/// directory the session is not actually in.
pub fn display_with_tilde(path: &Path) -> String {
    display_with_tilde_at(path, std::env::home_dir().as_deref())
}

fn display_with_tilde_at(path: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home
        && let Ok(rest) = path.strip_prefix(home)
    {
        return if rest.as_os_str().is_empty() {
            "~".to_string()
        } else {
            format!("~/{}", rest.display())
        };
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_folds_dot_and_dotdot_without_touching_disk() {
        assert_eq!(
            lexically_normalize(Path::new("/work"), Path::new("a/./b/../c")),
            PathBuf::from("/work/a/c")
        );
        assert_eq!(
            lexically_normalize(Path::new("/work"), Path::new("/other/../abs")),
            PathBuf::from("/abs")
        );
        // A relative path with an empty anchor keeps its own shape.
        assert_eq!(
            lexically_normalize(Path::new(""), Path::new("a/../b")),
            PathBuf::from("b")
        );
    }

    #[test]
    fn tilde_expansion_uses_only_the_given_home() {
        let home = Path::new("/home/u");
        assert_eq!(expand_tilde("~", Some(home)), PathBuf::from("/home/u"));
        assert_eq!(
            expand_tilde("~/repo/file", Some(home)),
            PathBuf::from("/home/u/repo/file")
        );
        assert_eq!(
            expand_tilde("~other/file", Some(home)),
            PathBuf::from("~other/file")
        );
        assert_eq!(expand_tilde("~/repo", None), PathBuf::from("~/repo"));
    }

    #[test]
    fn display_contracts_the_process_home() {
        let Some(home) = std::env::home_dir() else {
            return;
        };
        assert_eq!(display_with_tilde(&home), "~");
        assert_eq!(display_with_tilde(&home.join("repo")), "~/repo");
        assert_eq!(
            display_with_tilde_at(&home.join("repo"), None),
            home.join("repo").display().to_string()
        );
    }
}
