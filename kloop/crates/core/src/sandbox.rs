//! OS sandbox for bash execution (plan 19 slice 1: macOS seatbelt).
//!
//! The shape is where cc and codex independently converged: a deny-by-default
//! seatbelt profile with full-disk read, a write allow-list (cwd + tmp), and
//! network off unless configured. The policy → argv transform is a pure
//! function (codex's `sandboxing` crate discipline) so tests can assert the
//! exact profile on any platform; only [`availability`] and the actual spawn
//! are platform-dependent. Linux (bwrap + seccomp — landlock is legacy
//! upstream) and Windows are future slices behind the same seam: the CLI
//! degrades to unsandboxed execution wherever [`availability`] says no.

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

/// One directory the sandboxed command may write, minus its protected
/// subpaths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WritableRoot {
    pub root: PathBuf,
    /// Kept read-only inside a writable root: privilege-escalation surfaces,
    /// not ordinary data. cc's granularity for `.git` (hooks and config run
    /// code; the rest stays writable so `git commit` works in the sandbox —
    /// codex protects all of `.git` and pays for it with escalations), plus
    /// all of `.kloop` (permission rules live there).
    pub read_only_subpaths: Vec<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxPolicy {
    pub writable_roots: Vec<WritableRoot>,
    pub allow_network: bool,
    /// The sandbox/approval coupling knob (cc's autoAllowBashIfSandboxed,
    /// default on): a bash call this sandbox will contain skips the asking
    /// layers of the permission gate — deny rules, safety checks and
    /// explicit ask rules still run first. `[sandbox] auto_allow = false`
    /// reverts to slice-1 behavior (sandbox as pure containment, asking
    /// unchanged).
    pub auto_allow: bool,
}

impl SandboxPolicy {
    /// The workspace-write policy both references converged on: writable =
    /// cwd + `/tmp` + `$TMPDIR` + configured extras, everything else
    /// read-only, network per config. Roots are canonicalized (macOS `/tmp`
    /// is a symlink to `/private/tmp` and seatbelt matches resolved paths);
    /// the literal spelling is kept too when it differs, so both ways of
    /// naming the path match.
    pub fn workspace(cwd: &Path, extra_roots: &[PathBuf], allow_network: bool) -> Self {
        let mut roots: Vec<PathBuf> = Vec::new();
        let mut push = |path: &Path| {
            for p in [
                path.to_path_buf(),
                std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()),
            ] {
                if !roots.contains(&p) {
                    roots.push(p);
                }
            }
        };
        push(cwd);
        let tmp = Path::new("/tmp");
        if tmp.is_dir() {
            push(tmp);
        }
        if let Some(tmpdir) = std::env::var_os("TMPDIR").filter(|v| !v.is_empty()) {
            push(Path::new(&tmpdir));
        }
        for extra in extra_roots {
            push(extra);
        }
        SandboxPolicy {
            writable_roots: roots
                .into_iter()
                .map(|root| WritableRoot {
                    read_only_subpaths: protected_subpaths(&root),
                    root,
                })
                .collect(),
            allow_network,
            auto_allow: true,
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
    let file_write = format!("(allow file-write*\n{}\n)", write_parts.join("\n"));
    // Reads are full-disk in this policy shape; network is denied by the base
    // policy's (deny default) unless allow rules are appended.
    let mut sections = vec![
        SEATBELT_BASE_POLICY.to_string(),
        "; kloop dynamic policy: full-disk read, allow-listed writes".to_string(),
        "(allow file-read*)".to_string(),
        file_write,
    ];
    if policy.allow_network {
        sections.push(format!(
            "(allow network-outbound)\n(allow network-inbound)\n{SEATBELT_NETWORK_POLICY}"
        ));
    }
    (sections.join("\n"), params)
}

/// argv for running `sh -lc <shell_command>` under seatbelt: the wrapper is a
/// command-line prefix, exactly how the unsandboxed spawn runs it otherwise.
pub fn seatbelt_command(policy: &SandboxPolicy, shell_command: &str) -> (String, Vec<String>) {
    let (profile, params) = seatbelt_profile(policy);
    let mut args = vec!["-p".to_string(), profile];
    args.extend(
        params
            .into_iter()
            .map(|(key, value)| format!("-D{key}={}", value.display())),
    );
    args.push("--".into());
    args.extend(["sh".into(), "-lc".into(), shell_command.into()]);
    (SEATBELT_EXE.into(), args)
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
            allow_network,
            auto_allow: true,
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
                read_only_subpaths: vec![PathBuf::from("/work/proj/.kloop")],
            }],
            false,
        );
        let (profile, params) = seatbelt_profile(&policy);
        let expected_dynamic = "; kloop dynamic policy: full-disk read, allow-listed writes\n\
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
    fn network_section_only_when_allowed() {
        let root = || {
            vec![WritableRoot {
                root: PathBuf::from("/w"),
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
                read_only_subpaths: vec![],
            }],
            false,
        );
        let (program, args) = seatbelt_command(&policy, "echo hi");
        assert_eq!(program, "/usr/bin/sandbox-exec");
        assert_eq!(args[0], "-p");
        assert_eq!(args[1], seatbelt_profile(&policy).0);
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
