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
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SandboxPlan {
    /// No OS sandbox is in play: `disable_sandbox`, no policy, or not bash.
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
