//! OS sandbox for bash execution (plan 19 slice 1: macOS seatbelt).
//!
//! The shape is where cc and codex independently converged: a deny-by-default
//! seatbelt profile with full-disk read, a write allow-list (cwd + tmp), and
//! network off unless configured. The policy → argv transform is a pure
//! function (codex's `sandboxing` crate discipline) so tests can assert the
//! exact profile on any platform; only [`availability`] and the actual spawn
//! are platform-dependent. Linux (bwrap + seccomp — landlock is legacy
//! upstream) and Windows are future slices: the CLI degrades to unsandboxed
//! execution wherever [`availability`] says no. This is a directory module so
//! the seatbelt `.sbpl` assets sit next to their only consumer; when the
//! platform backends land they get sibling files (`linux.rs`, `windows.rs`).
//! Note the current seam — a `sandbox-exec`/`bwrap` argv prefix — is
//! macOS/Linux-shaped; Windows (restricted token / AppContainer) has no
//! wrapper binary and will reshape it into a spawn-owning form.

use std::path::Path;
use std::path::PathBuf;

/// Hardcoded absolute path (codex does the same): resolving `sandbox-exec`
/// through PATH would let an attacker-controlled directory supply the
/// "sandbox".
pub const SEATBELT_EXE: &str = "/usr/bin/sandbox-exec";

const SEATBELT_BASE_POLICY: &str = include_str!("seatbelt_base.sbpl");
const SEATBELT_NETWORK_POLICY: &str = include_str!("seatbelt_network.sbpl");

/// Appended to a failed sandboxed bash result when the failure looks like a
/// sandbox denial — this is what teaches the model the escalation move (cc
/// keeps the equivalent guidance in its system prompt; a hint at failure time
/// needs no prompt budget until it is actually relevant).
pub const DENIAL_HINT: &str = "\n[This command ran inside kloop's sandbox (file writes limited to \
    the workspace and temp directories; network disabled) and the failure looks like a sandbox \
    denial. If the command legitimately needs the blocked access, retry with disable_sandbox: \
    true — that run requires user approval.]";

/// Prepended to an escalated (re-run without the sandbox) result so the
/// model sees the earlier sandboxed failure was resolved by escalation, not
/// left hanging.
pub const ESCALATED_PREFIX: &str = "[Re-ran without the sandbox after user approval.]\n";

/// Appended when the user was asked to escalate and declined. Unlike
/// [`DENIAL_HINT`] it must NOT invite a disable_sandbox retry — the user
/// already said no.
pub const ESCALATION_DECLINED: &str = "\n[The user declined to run this outside kloop's sandbox. \
    Do not retry with disable_sandbox; take a different approach — write within the workspace or a \
    temp directory, or ask the user how to proceed.]";

/// Why one root remains writable when the active workspace changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WritableRootOrigin {
    Workspace,
    Temporary,
    Extra,
}

/// One directory the sandboxed command may write, minus its protected
/// subpaths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WritableRoot {
    pub root: PathBuf,
    /// A canonical/literal spelling can have more than one origin. In
    /// particular, an explicitly configured old checkout must remain writable
    /// after the workspace-derived contribution is replaced.
    pub origins: Vec<WritableRootOrigin>,
    /// Kept read-only inside a writable root: privilege-escalation surfaces,
    /// not ordinary data. cc's granularity for `.git` (hooks and config run
    /// code; the rest stays writable so `git commit` works in the sandbox —
    /// codex protects all of `.git` and pays for it with escalations), plus
    /// all of `.kloop` (agent instructions/state live there).
    pub read_only_subpaths: Vec<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxPolicy {
    pub writable_roots: Vec<WritableRoot>,
    /// Credential-bearing files that sandboxed model shell commands may never
    /// read. The parent kloop process loads them before spawning a command.
    pub denied_read_paths: Vec<PathBuf>,
    /// Private application state that remains read-only even when it overlaps a
    /// workspace or an explicitly configured writable root.
    pub denied_write_paths: Vec<PathBuf>,
    pub allow_network: bool,
    /// The sandbox/approval coupling knob (cc's autoAllowBashIfSandboxed,
    /// default on): a bash call this sandbox will contain skips the asking
    /// layers of the permission gate — deny rules, safety checks and
    /// explicit ask rules still run first. `[sandbox] auto_allow = false`
    /// reverts to slice-1 behavior (sandbox as pure containment, asking
    /// unchanged).
    pub auto_allow: bool,
    /// The code-level escalation loop (codex's retry-on-denial, default
    /// on): when a contained bash command fails in a denial-shaped way, ask
    /// once and — on approval — re-run it without the sandbox, one fewer
    /// model round-trip than the disable_sandbox hint. `[sandbox] escalate =
    /// false` keeps the model-driven hint instead.
    pub escalate: bool,
}

impl SandboxPolicy {
    /// The workspace-write policy both references converged on: writable =
    /// workspace + `/tmp` + `$TMPDIR` + configured extras, everything else
    /// read-only, network per config. Roots retain their provenance so a
    /// worktree transition replaces only the workspace contribution.
    pub fn workspace(cwd: &Path, extra_roots: &[PathBuf], allow_network: bool) -> Self {
        let mut writable_roots = Vec::new();
        push_root_aliases(&mut writable_roots, cwd, WritableRootOrigin::Workspace);
        let tmp = Path::new("/tmp");
        if tmp.is_dir() {
            push_root_aliases(&mut writable_roots, tmp, WritableRootOrigin::Temporary);
        }
        if let Some(tmpdir) = std::env::var_os("TMPDIR").filter(|value| !value.is_empty()) {
            push_root_aliases(
                &mut writable_roots,
                Path::new(&tmpdir),
                WritableRootOrigin::Temporary,
            );
        }
        for extra in extra_roots {
            push_root_aliases(&mut writable_roots, extra, WritableRootOrigin::Extra);
        }
        SandboxPolicy {
            writable_roots,
            denied_read_paths: Vec::new(),
            denied_write_paths: Vec::new(),
            allow_network,
            auto_allow: true,
            escalate: true,
        }
    }

    /// Replace the workspace-derived root while preserving temporary roots and
    /// configured extras. An old workspace that was also configured explicitly
    /// remains writable through its independent `Extra` origin.
    pub fn for_workspace(&self, path: &Path) -> Self {
        let mut policy = self.clone();
        for root in &mut policy.writable_roots {
            root.origins
                .retain(|origin| *origin != WritableRootOrigin::Workspace);
        }
        policy
            .writable_roots
            .retain(|root| !root.origins.is_empty());
        push_root_aliases(
            &mut policy.writable_roots,
            path,
            WritableRootOrigin::Workspace,
        );
        policy
    }

    /// Add a file the model-facing shell must never read. Keep literal and
    /// canonical spellings: macOS aliases `/tmp` and symlinked paths otherwise
    /// leave a second name for the same credential file.
    pub fn with_denied_read_path(&self, path: &Path) -> Self {
        let mut policy = self.clone();
        push_path_aliases(&mut policy.denied_read_paths, path);
        policy
    }

    /// Add application-owned state that sandboxed model shell commands may
    /// never modify, even when the path overlaps a writable root.
    pub fn with_denied_write_path(&self, path: &Path) -> Self {
        let mut policy = self.clone();
        push_path_aliases(&mut policy.denied_write_paths, path);
        policy
    }
}

fn push_path_aliases(paths: &mut Vec<PathBuf>, path: &Path) {
    for candidate in [
        path.to_path_buf(),
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()),
    ] {
        if !paths.contains(&candidate) {
            paths.push(candidate);
        }
    }
}

fn push_root_aliases(roots: &mut Vec<WritableRoot>, path: &Path, origin: WritableRootOrigin) {
    for candidate in [
        path.to_path_buf(),
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()),
    ] {
        if let Some(existing) = roots.iter_mut().find(|root| root.root == candidate) {
            if !existing.origins.contains(&origin) {
                existing.origins.push(origin);
            }
        } else {
            roots.push(WritableRoot {
                read_only_subpaths: protected_subpaths(&candidate),
                root: candidate,
                origins: vec![origin],
            });
        }
    }
}

fn protected_subpaths(root: &Path) -> Vec<PathBuf> {
    [".git/hooks", ".git/config", ".kloop"]
        .iter()
        .map(|sub| root.join(sub))
        .collect()
}

/// The full SBPL profile plus the `-D` parameter list. Paths travel as
/// `(param "KEY")` definitions, never inlined into the profile — codex's
/// defense against paths that would otherwise need SBPL escaping.
pub fn seatbelt_profile(policy: &SandboxPolicy) -> (String, Vec<(String, PathBuf)>) {
    let mut params: Vec<(String, PathBuf)> = Vec::new();
    let mut write_parts: Vec<String> = Vec::new();
    for (i, wr) in policy.writable_roots.iter().enumerate() {
        let key = format!("WRITABLE_ROOT_{i}");
        params.push((key.clone(), wr.root.clone()));
        if wr.read_only_subpaths.is_empty() {
            write_parts.push(format!("(subpath (param \"{key}\"))"));
            continue;
        }
        let mut parts = vec![format!("(subpath (param \"{key}\"))")];
        for (j, ro) in wr.read_only_subpaths.iter().enumerate() {
            let ro_key = format!("{key}_RO_{j}");
            params.push((ro_key.clone(), ro.clone()));
            // literal AND subpath: subpath alone leaves a gap for creating
            // the protected directory itself (codex's `mkdir .codex` hole).
            parts.push(format!("(require-not (literal (param \"{ro_key}\")))"));
            parts.push(format!("(require-not (subpath (param \"{ro_key}\")))"));
        }
        write_parts.push(format!("(require-all {} )", parts.join(" ")));
    }
    let mut read_denies = Vec::new();
    for (i, path) in policy.denied_read_paths.iter().enumerate() {
        let key = format!("DENIED_READ_{i}");
        params.push((key.clone(), path.clone()));
        read_denies.push(format!(
            "(literal (param \"{key}\")) (subpath (param \"{key}\"))"
        ));
    }
    let file_read = if read_denies.is_empty() {
        "(allow file-read*)".to_string()
    } else {
        format!(
            "(allow file-read*)\n(deny file-read* {})",
            read_denies.join(" ")
        )
    };
    let mut write_denies = Vec::new();
    for (i, path) in policy.denied_write_paths.iter().enumerate() {
        let key = format!("DENIED_WRITE_{i}");
        params.push((key.clone(), path.clone()));
        write_denies.push(format!(
            "(literal (param \"{key}\")) (subpath (param \"{key}\"))"
        ));
    }
    let file_write = if write_denies.is_empty() {
        format!("(allow file-write*\n{}\n)", write_parts.join("\n"))
    } else {
        format!(
            "(allow file-write*\n{}\n)\n(deny file-write* {})",
            write_parts.join("\n"),
            write_denies.join(" ")
        )
    };
    // Reads are full-disk except explicit credential files; network is denied
    // by the base policy's (deny default) unless allow rules are appended.
    let mut sections = vec![
        SEATBELT_BASE_POLICY.to_string(),
        "; kloop dynamic policy: full-disk read minus credentials, allow-listed writes".to_string(),
        file_read,
        file_write,
    ];
    if policy.allow_network {
        sections.push(format!(
            "(allow network-outbound)\n(allow network-inbound)\n{SEATBELT_NETWORK_POLICY}"
        ));
    }
    (sections.join("\n"), params)
}

/// Wrap an already-resolved program + argv in macOS Seatbelt. Containment of
/// the process tree remains the executor's responsibility; this function only
/// adds the sandbox-exec prefix.
pub fn seatbelt_command(
    policy: &SandboxPolicy,
    program: &std::ffi::OsStr,
    command_args: &[std::ffi::OsString],
) -> (PathBuf, Vec<std::ffi::OsString>) {
    let (profile, params) = seatbelt_profile(policy);
    let mut args = vec!["-p".into(), profile.into()];
    args.extend(
        params
            .into_iter()
            .map(|(key, value)| format!("-D{key}={}", value.display()).into()),
    );
    args.push("--".into());
    args.push(program.to_os_string());
    args.extend(command_args.iter().cloned());
    (PathBuf::from(SEATBELT_EXE), args)
}

/// Err(reason) when bash cannot be sandboxed on this machine; the CLI turns
/// that into a startup warning and runs unsandboxed (fail-open like hooks:
/// the permission gate stays the enforcement layer).
pub fn availability() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        if Path::new(SEATBELT_EXE).exists() {
            Ok(())
        } else {
            Err(format!("{SEATBELT_EXE} not found"))
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err("sandboxing is only implemented on macOS so far (Linux/Windows are planned)".into())
    }
}

/// Whether a failed sandboxed command was likely stopped by the sandbox
/// rather than failing on its own — the escalation trigger. Ported from
/// codex's `is_likely_sandbox_denied`: keyword match wins, then exit codes
/// 2/126/127 (usage error / not executable / not found) are quick-rejected as
/// the command's own fault. codex's third layer — exit 128+SIGSYS — is
/// seccomp-specific and belongs to the future Linux backend.
///
/// kloop extension over the codex port: with `network_disabled`, DNS
/// failure shapes count as evidence too. Seatbelt blocks the resolver's mach
/// lookup, so tools report "could not resolve host" (curl, measured) instead
/// of any EPERM keyword — codex's list misses it and leaves the model
/// guessing about the most common network denial.
pub fn is_likely_sandbox_denied(
    exit_code: Option<i32>,
    output: &str,
    network_disabled: bool,
) -> bool {
    if exit_code == Some(0) {
        return false;
    }
    const DENIED_KEYWORDS: [&str; 7] = [
        "operation not permitted",
        "permission denied",
        "read-only file system",
        "seccomp",
        "sandbox",
        "landlock",
        "failed to write file",
    ];
    // getaddrinfo failure texts across curl / git / python / node.
    const DNS_KEYWORDS: [&str; 4] = [
        "could not resolve host",
        "name resolution",
        "nodename nor servname",
        "getaddrinfo",
    ];
    let lower = output.to_lowercase();
    if DENIED_KEYWORDS.iter().any(|k| lower.contains(k)) {
        return true;
    }
    network_disabled && DNS_KEYWORDS.iter().any(|k| lower.contains(k))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy_with(roots: Vec<WritableRoot>, allow_network: bool) -> SandboxPolicy {
        SandboxPolicy {
            writable_roots: roots,
            denied_read_paths: Vec::new(),
            denied_write_paths: Vec::new(),
            allow_network,
            auto_allow: true,
            escalate: true,
        }
    }

    /// The dynamic sections are asserted as one exact string on top of the
    /// verbatim base policy — the profile is a security artifact, so the test
    /// locks its full shape, not fragments.
    #[test]
    fn profile_is_base_plus_exact_dynamic_sections() {
        let policy = policy_with(
            vec![WritableRoot {
                root: PathBuf::from("/work/proj"),
                origins: vec![WritableRootOrigin::Workspace],
                read_only_subpaths: vec![PathBuf::from("/work/proj/.kloop")],
            }],
            false,
        );
        let (profile, params) = seatbelt_profile(&policy);
        let expected_dynamic = "; kloop dynamic policy: full-disk read minus credentials, allow-listed writes\n\
             (allow file-read*)\n\
             (allow file-write*\n\
             (require-all (subpath (param \"WRITABLE_ROOT_0\")) \
             (require-not (literal (param \"WRITABLE_ROOT_0_RO_0\"))) \
             (require-not (subpath (param \"WRITABLE_ROOT_0_RO_0\"))) )\n\
             )";
        assert_eq!(
            profile,
            format!("{SEATBELT_BASE_POLICY}\n{expected_dynamic}")
        );
        assert_eq!(
            params,
            vec![
                ("WRITABLE_ROOT_0".to_string(), PathBuf::from("/work/proj")),
                (
                    "WRITABLE_ROOT_0_RO_0".to_string(),
                    PathBuf::from("/work/proj/.kloop")
                ),
            ]
        );
        assert!(profile.starts_with("; verbatim from codex"));
        assert!(profile.contains("(deny default)"));
    }

    #[test]
    fn denied_state_tree_is_parameterized_for_reads_and_writes() {
        let private_root = Path::new("/home/u/.kloop");
        let policy = policy_with(Vec::new(), false)
            .with_denied_read_path(private_root)
            .with_denied_write_path(private_root);
        let (profile, params) = seatbelt_profile(&policy);
        assert!(profile.contains(
            "(allow file-read*)\n(deny file-read* (literal (param \"DENIED_READ_0\")) (subpath (param \"DENIED_READ_0\")))"
        ));
        assert!(profile.contains(
            "(deny file-write* (literal (param \"DENIED_WRITE_0\")) (subpath (param \"DENIED_WRITE_0\")))"
        ));
        assert_eq!(
            params,
            vec![
                ("DENIED_READ_0".to_string(), PathBuf::from("/home/u/.kloop")),
                (
                    "DENIED_WRITE_0".to_string(),
                    PathBuf::from("/home/u/.kloop")
                ),
            ]
        );
    }

    #[test]
    fn network_section_only_when_allowed() {
        let root = || {
            vec![WritableRoot {
                root: PathBuf::from("/w"),
                origins: vec![WritableRootOrigin::Workspace],
                read_only_subpaths: vec![],
            }]
        };
        let (off, _) = seatbelt_profile(&policy_with(root(), false));
        assert!(!off.contains("network-outbound"));

        let (on, _) = seatbelt_profile(&policy_with(root(), true));
        assert!(on.contains("(allow network-outbound)\n(allow network-inbound)"));
        assert!(on.contains("com.apple.SystemConfiguration.DNSConfiguration"));
        // The base policy still opens with deny default either way.
        assert!(on.contains("(deny default)"));
    }

    #[test]
    fn seatbelt_command_shape() {
        let policy = policy_with(
            vec![WritableRoot {
                root: PathBuf::from("/w"),
                origins: vec![WritableRootOrigin::Workspace],
                read_only_subpaths: vec![],
            }],
            false,
        );
        let command_args = ["-lc".into(), "echo hi".into()];
        let (program, args) = seatbelt_command(&policy, std::ffi::OsStr::new("sh"), &command_args);
        assert_eq!(program, PathBuf::from("/usr/bin/sandbox-exec"));
        assert_eq!(args[0], "-p");
        assert_eq!(args[1].to_string_lossy(), seatbelt_profile(&policy).0);
        assert_eq!(
            &args[2..],
            ["-DWRITABLE_ROOT_0=/w", "--", "sh", "-lc", "echo hi"]
        );
    }

    #[test]
    fn workspace_policy_includes_cwd_tmp_extras_with_protected_subpaths() {
        let cwd = std::env::temp_dir().join("kloop-sbx-ws");
        std::fs::create_dir_all(&cwd).unwrap();
        let extra = PathBuf::from("/opt/data");
        let policy = SandboxPolicy::workspace(&cwd, std::slice::from_ref(&extra), false);

        let roots: Vec<&Path> = policy
            .writable_roots
            .iter()
            .map(|w| w.root.as_path())
            .collect();
        assert!(roots.contains(&cwd.as_path()));
        assert!(roots.contains(&Path::new("/tmp")));
        // /tmp is a symlink on macOS; the canonical spelling rides along.
        #[cfg(target_os = "macos")]
        assert!(roots.contains(&Path::new("/private/tmp")));
        assert!(roots.contains(&extra.as_path()));
        // No duplicates even with canonical/literal overlap.
        let mut deduped = roots.clone();
        deduped.dedup();
        assert_eq!(roots.len(), deduped.len());

        // Every root protects the same escalation surfaces.
        for wr in &policy.writable_roots {
            assert_eq!(
                wr.read_only_subpaths,
                vec![
                    wr.root.join(".git/hooks"),
                    wr.root.join(".git/config"),
                    wr.root.join(".kloop"),
                ]
            );
        }
        assert!(!policy.allow_network);
    }

    /// Managed worktrees live at `<repo>/.kloop/worktrees/<name>`, i.e. inside
    /// the repo root's read-only `.kloop` subpath. Writes there stay allowed
    /// because each writable root contributes its own alternative to one
    /// `(allow file-write* ...)` disjunction: the worktree's own part grants
    /// the write, and the repo part's `require-not` constrains only itself.
    #[test]
    fn a_worktree_under_the_protected_state_dir_stays_writable() {
        let repo = PathBuf::from("/work/proj");
        let worktree = repo.join(".kloop/worktrees/wt");
        let target = worktree.join("src/main.rs");
        // Worst case: the repo is *also* an explicitly configured root, so its
        // `.kloop` read-only subpath survives the workspace swap.
        let policy = SandboxPolicy::workspace(&repo, std::slice::from_ref(&repo), false)
            .for_workspace(&worktree);

        let permits = |root: &WritableRoot| {
            target.starts_with(&root.root)
                && !root
                    .read_only_subpaths
                    .iter()
                    .any(|read_only| target.starts_with(read_only))
        };
        assert!(
            policy.writable_roots.iter().any(permits),
            "no writable root permits {}: {:?}",
            target.display(),
            policy.writable_roots
        );
        // The repo's own part still refuses it, which is why the disjunction
        // (not a narrower `.kloop` rule) is what makes this work.
        let repo_root = policy
            .writable_roots
            .iter()
            .find(|root| root.root == repo)
            .expect("configured repo root survives the swap");
        assert!(!permits(repo_root));
    }

    #[test]
    fn for_workspace_replaces_only_the_workspace_derived_root() {
        let base_root = PathBuf::from("/work/base");
        let tree = PathBuf::from("/work/tree");
        let next = PathBuf::from("/work/next");
        let extra = PathBuf::from("/opt/data");
        let base = SandboxPolicy::workspace(
            &base_root,
            std::slice::from_ref(&extra),
            /*allow_network*/ false,
        );

        let tree_policy = base.for_workspace(&tree);
        let roots = |policy: &SandboxPolicy| {
            policy
                .writable_roots
                .iter()
                .map(|root| root.root.clone())
                .collect::<Vec<_>>()
        };
        assert!(roots(&tree_policy).contains(&tree));
        assert!(!roots(&tree_policy).contains(&base_root));
        assert!(roots(&tree_policy).contains(&extra));
        assert!(roots(&tree_policy).contains(&PathBuf::from("/tmp")));
        assert_eq!(
            tree_policy
                .writable_roots
                .iter()
                .find(|root| root.root == tree)
                .unwrap()
                .read_only_subpaths,
            protected_subpaths(&tree)
        );

        let next_policy = tree_policy.for_workspace(&next);
        assert!(roots(&next_policy).contains(&next));
        assert!(!roots(&next_policy).contains(&tree));
        assert!(!roots(&next_policy).contains(&base_root));
        assert!(roots(&next_policy).contains(&extra));
        assert_eq!(next_policy.allow_network, base.allow_network);
        assert_eq!(next_policy.auto_allow, base.auto_allow);
        assert_eq!(next_policy.escalate, base.escalate);
    }

    #[test]
    fn for_workspace_preserves_an_explicit_old_workspace_root() {
        let base_root = PathBuf::from("/work/base-explicit");
        let tree = PathBuf::from("/work/tree-explicit");
        let base = SandboxPolicy::workspace(
            &base_root,
            std::slice::from_ref(&base_root),
            /*allow_network*/ false,
        );

        let policy = base.for_workspace(&tree);
        let old = policy
            .writable_roots
            .iter()
            .find(|root| root.root == base_root)
            .unwrap();
        assert_eq!(old.origins, vec![WritableRootOrigin::Extra]);
        assert!(policy.writable_roots.iter().any(|root| root.root == tree));
    }

    #[test]
    fn denial_detection_table() {
        let denied = |code, output| is_likely_sandbox_denied(code, output, false);
        // Success is never a denial, whatever the output says.
        assert!(!denied(Some(0), "Operation not permitted"));
        // Keyword hits (case-insensitive), including on quick-reject codes:
        // keywords are checked first, exactly like codex.
        assert!(denied(Some(1), "sh: /x/f.txt: Operation not permitted"));
        assert!(denied(Some(1), "curl: PERMISSION DENIED"));
        assert!(denied(Some(1), "Read-only file system"));
        assert!(denied(Some(127), "blocked by sandbox"));
        assert!(denied(None, "killed: landlock violation"));
        // Quick-reject codes without keywords: the command's own fault.
        assert!(!denied(Some(2), "usage: grep [options]"));
        assert!(!denied(Some(126), "cannot execute binary file"));
        assert!(!denied(Some(127), "command not found: foo"));
        // Plain failure, no evidence.
        assert!(!denied(Some(1), "assertion failed"));
        assert!(!denied(None, ""));

        // DNS failure shapes count only in a network-disabled sandbox (the
        // kloop extension; text measured from sandboxed curl on macOS).
        let net_off = |code, output| is_likely_sandbox_denied(code, output, true);
        assert!(net_off(
            Some(6),
            "curl: (6) Could not resolve host: example.com"
        ));
        assert!(net_off(Some(1), "Temporary failure in name resolution"));
        assert!(net_off(Some(1), "getaddrinfo ENOTFOUND example.com"));
        assert!(!denied(
            Some(6),
            "curl: (6) Could not resolve host: example.com"
        ));
        assert!(!net_off(Some(0), "Could not resolve host"));
        assert!(!net_off(Some(1), "assertion failed"));
    }

    #[test]
    fn availability_matches_platform() {
        #[cfg(target_os = "macos")]
        assert_eq!(availability(), Ok(()));
        #[cfg(not(target_os = "macos"))]
        assert!(availability().is_err());
    }
}
