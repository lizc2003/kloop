//! 询问记录（plan 283 阶段 0）：权限层每问一次人，或者本该问却被记忆答掉，都留一条，
//! 由本轮写进会话文件。只用于事后统计“问了多少次、为什么问、记忆有没有起作用”，
//! 不参与任何判定，也不进对话。

use serde::Deserialize;
use serde::Serialize;

use super::ApprovalScope;

/// One question the permission layer put to a person, or would have put had a
/// remembered answer not covered it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRecord {
    pub door: ApprovalDoor,
    pub tool: String,
    pub reason: AskReason,
    /// What the prompt showed as the thing acted on, clipped like the prompt.
    pub detail: String,
    /// The session-cache keys this answer is (or would be) remembered under.
    /// Empty when nothing about the call could be remembered.
    pub keys: Vec<String>,
    /// The durable rules a project-scoped answer would write.
    pub rules: Vec<String>,
    /// The scopes offered; empty when nobody was asked.
    pub offered: Vec<ApprovalScope>,
    pub answer: ApprovalAnswer,
}

/// Which of the two questions this was.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDoor {
    /// The permission gate, before the call ran.
    Gate,
    /// After the OS sandbox denied a contained run: run it uncontained?
    Escalation,
}

/// Why the question came up. The classes are the ones plan 283 has to count
/// apart: leaving the sandbox, and everything else that still asks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AskReason {
    /// A safety check flagged the call (destructive command, `.git` write…).
    Hazard { tag: String },
    /// An explicit `ask` rule matched.
    AskRule,
    /// The model asked to run bash without the OS sandbox. Only in files
    /// written before plan 283 stage 1 removed `disable_sandbox`; kept so
    /// those files still read.
    NoSandbox,
    /// The sandbox would contain this bash call, but the parse could not vouch
    /// for all of it, so the sandbox auto-allow did not apply.
    IncompleteParse,
    /// Bash (or PowerShell) with no sandbox auto-allow to stand in for the
    /// human: no policy, the platform has none, or `trust_sandboxed = false`.
    Uncontained,
    /// A file tool whose target is outside the working directory.
    OutsideWorkspace,
    /// Any other tool the gate cannot vouch for on its own (MCP, web…).
    Tool,
    /// The OS sandbox denied a contained run; `denial` is the class label.
    SandboxDenied { denial: String },
    /// A remembered escalation took the call out of the sandbox before it
    /// ran, sparing the question a denial would have asked; nothing was
    /// denied this time.
    EscalatedBeforeRun,
}

/// What came of the question.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovalAnswer {
    /// A remembered answer covered the call; nobody was asked.
    Remembered {
        by: MemorySource,
    },
    Allowed {
        scope: ApprovalScope,
    },
    Denied,
    Stopped,
    /// There was nobody to ask (headless), so the call was refused.
    NoApprover,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemorySource {
    /// A durable project rule.
    Rule,
    /// This workspace session's cache.
    Session,
}
