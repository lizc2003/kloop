//! 权限门对“怎么执行”的判定：沙箱与凭据保护只判一次，执行层照单执行。

use super::PermissionNotice;

/// What the gate decided about how a permitted call executes. Execution reads
/// this and never judges it again: the same parse and the same sandbox facts
/// produced the permission decision, so a second judgment could only disagree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Verdict {
    pub(crate) sandbox: SandboxPlan,
    pub(crate) protection: Protection,
    /// A safe, user-visible note that came with the approval.
    pub(crate) notice: Option<PermissionNotice>,
    /// What a contained run gets beyond the workspace policy, from consents
    /// already given (plan 283). Empty unless `sandbox` is `Contained`.
    pub(crate) grants: SandboxGrants,
}

/// Capabilities added to the sandbox rather than removing it (plan 283): the
/// network, and directories to write. The two things the sandbox refuses that
/// can be granted without lifting the rest of it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SandboxGrants {
    pub(crate) network: bool,
    pub(crate) writable: Vec<std::path::PathBuf>,
}

impl SandboxGrants {
    pub(crate) fn is_empty(&self) -> bool {
        !self.network && self.writable.is_empty()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SandboxPlan {
    /// No OS sandbox is in play: no policy, or not bash.
    #[default]
    None,
    /// Run inside the workspace sandbox.
    Contained,
    /// A remembered per-command consent: skip the contained attempt entirely.
    Escalated,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Protection {
    /// No sandbox context to judge — the sandbox-blind `check` entry.
    #[default]
    Unchecked,
    /// Nothing in the command needs the sandbox to protect it.
    Clear,
    /// A literal body names a credential path; only actual containment protects
    /// it, so this call may never run uncontained.
    Required,
}
