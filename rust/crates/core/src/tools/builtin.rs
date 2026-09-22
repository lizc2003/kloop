//! Every built-in tool, once.
//!
//! One tool used to be seven parallel `match`es — definition, surface gate,
//! concurrency safety, permission read-only, dispatch, panel title, TUI row —
//! and only dispatch failed loudly when one of them was forgotten; the other
//! six degraded in silence (a tool quietly serialized, a read-only call quietly
//! prompting, a row quietly rendered as raw JSON). [`Builtin`] is the one place:
//! adding a variant makes the compiler name every decision that has to be made
//! about it.
//!
//! The one deliberate exception is the TUI row ([`kloop_tui::toolrow`]): turning
//! an input into `Read src/main.rs` is display logic that must also work for
//! MCP tools, so it stays a name match — with a guard test over
//! [`builtin_tool_names`] so a new variant still cannot be forgotten there.

use std::time::Duration;

use kloop_protocol::ToolDef;
use serde_json::Value;
use serde_json::json;

use super::BASH_TIMEOUT_GRACE;
use super::READ_ONLY_TOOL_TIMEOUT;
use super::agent_message;
use super::codemode;
use super::plan_mode;
use super::question;
use super::scheduler;
use super::skill;
use super::todo;
use super::tool_search;
use super::workflow;
use super::worktree_tool;
use crate::config::SurfaceCapabilities;
use crate::shell_programs::ShellPrograms;
use crate::tools::MAX_DISPLAY_DESCRIPTION_CHARS;

/// Every built-in tool. Ordered as the model sees them: the catalog block in
/// [`super::builtin_defs`] order, then the deferral pair, then the
/// surface-gated depth-0 block, then `skill` — which is also the order
/// [`ALL`] is walked to build a request's tool array, so **reshuffling these
/// variants changes the provider request bytes and invalidates the prompt
/// cache**. Add at the end of the block a tool belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Builtin {
    Bash,
    BashOutput,
    StopBash,
    PowerShell,
    ReadFile,
    WriteFile,
    EditFile,
    NotebookEdit,
    Grep,
    Glob,
    TodoWrite,
    SendMessage,
    ListAgents,
    RunAgent,
    WaitForActivity,
    StopAgent,
    ToolSearch,
    CallTool,
    RunProgram,
    StopProgram,
    CronCreate,
    CronDelete,
    CronList,
    ScheduleWakeup,
    AskUserQuestion,
    EnterPlanMode,
    ExitPlanMode,
    Workflow,
    StopWorkflow,
    EnterWorktree,
    ExitWorktree,
    Skill,
}

/// Every variant, in wire order. Hand-written because Rust cannot enumerate an
/// enum; [`builtin_all_is_complete`] pins its length against the variant count
/// so a new variant that is not listed here fails the build's tests instead of
/// silently never being offered.
///
/// [`builtin_all_is_complete`]: tests::builtin_all_is_complete
pub(crate) const ALL: &[Builtin] = &[
    Builtin::Bash,
    Builtin::BashOutput,
    Builtin::StopBash,
    Builtin::PowerShell,
    Builtin::ReadFile,
    Builtin::WriteFile,
    Builtin::EditFile,
    Builtin::NotebookEdit,
    Builtin::Grep,
    Builtin::Glob,
    Builtin::TodoWrite,
    Builtin::SendMessage,
    Builtin::ListAgents,
    Builtin::RunAgent,
    Builtin::WaitForActivity,
    Builtin::StopAgent,
    Builtin::ToolSearch,
    Builtin::CallTool,
    Builtin::RunProgram,
    Builtin::StopProgram,
    Builtin::CronCreate,
    Builtin::CronDelete,
    Builtin::CronList,
    Builtin::ScheduleWakeup,
    Builtin::AskUserQuestion,
    Builtin::EnterPlanMode,
    Builtin::ExitPlanMode,
    Builtin::Workflow,
    Builtin::StopWorkflow,
    Builtin::EnterWorktree,
    Builtin::ExitWorktree,
    Builtin::Skill,
];

/// Which front-end capability a surface-gated tool rides on. Mirrors the fields
/// of [`SurfaceCapabilities`] so the gate is data a variant can name, not a
/// branch each catalog builder has to remember to write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SurfaceGate {
    Program,
    Scheduler,
    Questions,
    PlanControl,
    Workflow,
    Worktree,
}

impl SurfaceGate {
    fn enabled(self, surface: SurfaceCapabilities) -> bool {
        match self {
            Self::Program => surface.program,
            Self::Scheduler => surface.scheduler,
            Self::Questions => surface.questions,
            Self::PlanControl => surface.plan_control,
            Self::Workflow => surface.workflow,
            Self::Worktree => surface.worktree,
        }
    }

    /// The [`SurfaceCapabilities`] field this gate reads, spelled as the config
    /// spells it: a rejected call names what would have to be turned on.
    fn field(self) -> &'static str {
        match self {
            Self::Program => "program",
            Self::Scheduler => "scheduler",
            Self::Questions => "questions",
            Self::PlanControl => "plan_control",
            Self::Workflow => "workflow",
            Self::Worktree => "worktree",
        }
    }
}

/// Why a request does not offer a built-in. Carries what the message has to
/// name, so the door that rejects a call says which condition failed rather
/// than one flat "unavailable".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unoffered {
    /// A root-owned session control a sub-agent must not re-enter.
    RootOnly,
    /// The front-end did not enable the capability this tool rides on.
    Surface(SurfaceGate),
    /// This host resolved no interpreter for that shell.
    Shell(ShellKind),
}

impl Unoffered {
    /// The tool_result text a call to `name` is rejected with.
    pub(crate) fn message(self, name: &str) -> String {
        match self {
            Self::RootOnly => format!("tool '{name}' is only available to the root agent"),
            Self::Surface(gate) => {
                let field = gate.field();
                format!(
                    "tool '{name}' is unavailable because this session's front-end does not enable the '{field}' surface"
                )
            }
            Self::Shell(ShellKind::Bash) => format!(
                "tool '{name}' is unavailable because no validated Git for Windows Bash was resolved for this session"
            ),
            Self::Shell(ShellKind::PowerShell) => format!(
                "tool '{name}' is unavailable because no trusted PowerShell executable was resolved for this session"
            ),
        }
    }
}

/// Where a built-in's definition is emitted, and what has to hold for it to be
/// offered at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Gate {
    /// The built-in catalog, at every depth.
    Always,
    /// The built-in catalog, depth 0 only: root-owned session controls a
    /// sub-agent must not re-enter.
    Depth0,
    /// The built-in catalog at every depth, but only where the host actually
    /// has that shell.
    Shell(ShellKind),
    /// Appended at depth 0 when the front-end enables that capability. Not
    /// counted toward the defer threshold and absent from run_program's
    /// TypeScript API, because it may not be sent at all.
    Surface(SurfaceGate),
    /// Owned by whoever owns the condition, not by the catalog builders:
    /// `tool_search`/`call_tool` appear only once source definitions are
    /// deferred, and `skill` only once a model-invocable skill is loaded.
    Elsewhere,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShellKind {
    Bash,
    PowerShell,
}

/// Everything a definition needs beyond its own constant text. Only
/// `run_program` reads these — its generated TypeScript API lists the catalogs
/// a program may call — so the catalog block builds its defs with
/// `DefCx::default()`.
#[derive(Default)]
pub(crate) struct DefCx<'a> {
    pub(crate) builtins: &'a [ToolDef],
    pub(crate) inline_sources: &'a [ToolDef],
    pub(crate) deferred_sources: &'a [ToolDef],
}

impl Builtin {
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        // Spelled out rather than derived from `ALL` + `name()` so the lookup
        // is a jump table, not a scan: it runs on every tool call, and on every
        // name an MCP server offers.
        Some(match name {
            "bash" => Self::Bash,
            "bash_output" => Self::BashOutput,
            "stop_bash" => Self::StopBash,
            "powershell" => Self::PowerShell,
            "read_file" => Self::ReadFile,
            "write_file" => Self::WriteFile,
            "edit_file" => Self::EditFile,
            "notebook_edit" => Self::NotebookEdit,
            "grep" => Self::Grep,
            "glob" => Self::Glob,
            "todo_write" => Self::TodoWrite,
            "send_message" => Self::SendMessage,
            "list_agents" => Self::ListAgents,
            "run_agent" => Self::RunAgent,
            "wait_for_activity" => Self::WaitForActivity,
            "stop_agent" => Self::StopAgent,
            "tool_search" => Self::ToolSearch,
            "call_tool" => Self::CallTool,
            "run_program" => Self::RunProgram,
            "stop_program" => Self::StopProgram,
            "cron_create" => Self::CronCreate,
            "cron_delete" => Self::CronDelete,
            "cron_list" => Self::CronList,
            "schedule_wakeup" => Self::ScheduleWakeup,
            "ask_user_question" => Self::AskUserQuestion,
            "enter_plan_mode" => Self::EnterPlanMode,
            "exit_plan_mode" => Self::ExitPlanMode,
            "workflow" => Self::Workflow,
            "stop_workflow" => Self::StopWorkflow,
            "enter_worktree" => Self::EnterWorktree,
            "exit_worktree" => Self::ExitWorktree,
            "skill" => Self::Skill,
            _ => return None,
        })
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::BashOutput => "bash_output",
            Self::StopBash => "stop_bash",
            Self::PowerShell => "powershell",
            Self::ReadFile => "read_file",
            Self::WriteFile => "write_file",
            Self::EditFile => "edit_file",
            Self::NotebookEdit => "notebook_edit",
            Self::Grep => "grep",
            Self::Glob => "glob",
            Self::TodoWrite => "todo_write",
            Self::SendMessage => "send_message",
            Self::ListAgents => "list_agents",
            Self::RunAgent => "run_agent",
            Self::WaitForActivity => "wait_for_activity",
            Self::StopAgent => "stop_agent",
            Self::ToolSearch => "tool_search",
            Self::CallTool => "call_tool",
            Self::RunProgram => "run_program",
            Self::StopProgram => "stop_program",
            Self::CronCreate => "cron_create",
            Self::CronDelete => "cron_delete",
            Self::CronList => "cron_list",
            Self::ScheduleWakeup => "schedule_wakeup",
            Self::AskUserQuestion => "ask_user_question",
            Self::EnterPlanMode => "enter_plan_mode",
            Self::ExitPlanMode => "exit_plan_mode",
            Self::Workflow => "workflow",
            Self::StopWorkflow => "stop_workflow",
            Self::EnterWorktree => "enter_worktree",
            Self::ExitWorktree => "exit_worktree",
            Self::Skill => "skill",
        }
    }

    pub(crate) fn gate(self) -> Gate {
        match self {
            Self::Bash | Self::BashOutput | Self::StopBash => Gate::Shell(ShellKind::Bash),
            Self::PowerShell => Gate::Shell(ShellKind::PowerShell),
            Self::ReadFile
            | Self::WriteFile
            | Self::EditFile
            | Self::NotebookEdit
            | Self::Grep
            | Self::Glob => Gate::Always,
            // The session todo list is root-owned even though child Configs
            // retain the same Arc.
            Self::TodoWrite => Gate::Depth0,
            // Local Agent mailbox tools remain available at every depth.
            Self::SendMessage | Self::ListAgents => Gate::Always,
            // Sub-agents cannot spawn further sub-agents, so the whole
            // background-agent surface is root-only.
            Self::RunAgent | Self::WaitForActivity | Self::StopAgent => Gate::Depth0,
            Self::ToolSearch | Self::CallTool | Self::Skill => Gate::Elsewhere,
            // stop_program rides run_program's flag: without that tool there is
            // no program-N to stop.
            Self::RunProgram | Self::StopProgram => Gate::Surface(SurfaceGate::Program),
            Self::CronCreate | Self::CronDelete | Self::CronList | Self::ScheduleWakeup => {
                Gate::Surface(SurfaceGate::Scheduler)
            }
            Self::AskUserQuestion => Gate::Surface(SurfaceGate::Questions),
            // Both plan controls are advertised together so the request's tool
            // array stays byte-stable while the session mode changes.
            Self::EnterPlanMode | Self::ExitPlanMode => Gate::Surface(SurfaceGate::PlanControl),
            Self::Workflow | Self::StopWorkflow => Gate::Surface(SurfaceGate::Workflow),
            Self::EnterWorktree | Self::ExitWorktree => Gate::Surface(SurfaceGate::Worktree),
        }
    }

    /// Whether this tool belongs in the built-in catalog at `depth` on this
    /// host — the block `run_program` derives its TypeScript API from, and the
    /// block the defer threshold counts.
    pub(crate) fn in_catalog(self, depth: u8, shell_programs: &ShellPrograms) -> bool {
        match self.gate() {
            Gate::Always => true,
            Gate::Depth0 => depth == 0,
            Gate::Shell(ShellKind::Bash) => shell_programs.bash_available(),
            Gate::Shell(ShellKind::PowerShell) => {
                cfg!(windows) && shell_programs.powershell_available()
            }
            Gate::Surface(_) | Gate::Elsewhere => false,
        }
    }

    /// Whether this tool is appended to a depth-0 request for `surface`.
    pub(crate) fn in_surface(self, surface: SurfaceCapabilities) -> bool {
        matches!(self.gate(), Gate::Surface(gate) if gate.enabled(surface))
    }

    /// Why this request does not offer `self`, or `None` when it does.
    ///
    /// Defined as the complement of the two blocks [`super::all_tool_defs`]
    /// emits — the catalog, then the depth-0 surface block — so the door that
    /// rejects a call reads the same table the tool array was built from and
    /// the two cannot drift. Absent from the array is not the same as
    /// uncallable: stale context from before a compaction, a resumed rollout
    /// and a forged call all arrive at dispatch without ever passing a builder.
    ///
    /// [`Gate::Elsewhere`] answers `None` because this table does not own its
    /// condition: `tool_search`/`call_tool` ride on deferral and `skill` on
    /// what is loaded, each re-checked by its own owner.
    pub(crate) fn unoffered(
        self,
        depth: u8,
        surface: SurfaceCapabilities,
        shell_programs: &ShellPrograms,
    ) -> Option<Unoffered> {
        if self.in_catalog(depth, shell_programs) || (depth == 0 && self.in_surface(surface)) {
            return None;
        }
        Some(match self.gate() {
            Gate::Depth0 => Unoffered::RootOnly,
            Gate::Shell(kind) => Unoffered::Shell(kind),
            // A sub-agent is never sent the surface block at all, whatever its
            // Config happens to say — naming the depth tells it more than
            // naming a capability its parent's front-end may well have.
            Gate::Surface(_) if depth > 0 => Unoffered::RootOnly,
            Gate::Surface(gate) => Unoffered::Surface(gate),
            Gate::Always | Gate::Elsewhere => return None,
        })
    }

    /// The human name for the action, for the approval panel's header row.
    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::Bash => "Bash command",
            Self::BashOutput => "Background shell output",
            Self::StopBash => "Stop background shell",
            Self::PowerShell => "PowerShell command",
            Self::ReadFile => "Read file",
            Self::WriteFile => "Write file",
            Self::EditFile => "Edit file",
            Self::NotebookEdit => "Edit notebook",
            Self::Grep => "Search file contents",
            Self::Glob => "Find files by name",
            Self::TodoWrite => "Write todo list",
            Self::SendMessage => "Message an agent",
            Self::ListAgents => "List agents",
            Self::RunAgent => "Run sub-agent",
            Self::WaitForActivity => "Wait for background activity",
            Self::StopAgent => "Stop agent",
            Self::ToolSearch => "Search tools",
            Self::CallTool => "Call a deferred tool",
            Self::RunProgram => "Run program",
            Self::StopProgram => "Stop program",
            Self::CronCreate => "Create scheduled agent",
            Self::CronDelete => "Delete scheduled agent",
            Self::CronList => "List scheduled agents",
            Self::ScheduleWakeup => "Schedule a wake-up",
            Self::AskUserQuestion => "Ask a question",
            Self::EnterPlanMode => "Enter plan mode",
            Self::ExitPlanMode => "Exit plan mode",
            Self::Workflow => "Run workflow",
            Self::StopWorkflow => "Stop workflow",
            Self::EnterWorktree => "Enter worktree",
            Self::ExitWorktree => "Exit worktree",
            Self::Skill => "Load skill",
        }
    }
}

impl Builtin {
    /// Whether this call may run in the same concurrent batch as its
    /// neighbours. Read-only tools always may; `bash` may only when every
    /// parsed argv is a known read-only command — the same decomposition the
    /// permission gate uses, because safe-to-parallelize and safe-to-run are
    /// two verdicts over one analysis.
    /// The wall-clock budget for one call, or `None` for a tool that has no
    /// useful bound.
    ///
    /// The budget is declared by the tool, not by a name table in the
    /// dispatcher — deepseek-harness's `timeout-policy` note is explicit about
    /// why ("超时放在工具定义上……消除了拼错名称导致策略不生效的问题"), and it is the
    /// same reason [`Self::concurrency_safe`] lives here. The default is
    /// conservative for the same reason it is there: a tool waiting on a person
    /// or running a whole sub-agent has no deadline anyone can pick for it, and
    /// a wrong one turns working behaviour into an error.
    ///
    /// Cooperative, and only cooperative: reaching the budget cancels the
    /// call's token and waits for it to settle. Nothing here kills anything, so
    /// a tool that ignores cancellation is bounded by this in name only — which
    /// is what the model is told when it fires.
    pub(crate) fn timeout(self, input: &Value) -> Option<Duration> {
        match self {
            // Self-bounded, and its own bound is the real one: it kills the
            // process tree, which a cancellation cannot. The outer deadline
            // sits strictly above the model's `timeout_ms` so it can only ever
            // catch a `bash` that failed to stop itself.
            Self::Bash => Some(super::bash::foreground_timeout(input) + BASH_TIMEOUT_GRACE),
            // Self-bounded the same way, through the same executor.
            Self::PowerShell => None,
            // Read-only and local. See [`READ_ONLY_TOOL_TIMEOUT`]: this is a
            // floor under a wedged mount, not a budget real work runs into.
            Self::ReadFile | Self::Grep | Self::Glob => Some(READ_ONLY_TOOL_TIMEOUT),
            // Waiting is the job. A deadline here is not a guard, it is a bug:
            // a sub-agent, a program, a workflow and a question to the human all
            // legitimately outlast any number this file could name, and
            // `wait_for_activity` and `bash_output` carry their own `timeout_ms`.
            Self::RunAgent
            | Self::RunProgram
            | Self::Workflow
            | Self::WaitForActivity
            | Self::BashOutput
            | Self::AskUserQuestion
            | Self::EnterPlanMode
            | Self::ExitPlanMode => None,
            // Local state, and mutations that hold a path lock or shell out to
            // git. None of them has ever hung, and none of them has a number
            // anyone could defend; the conservative default applies.
            Self::WriteFile
            | Self::EditFile
            | Self::NotebookEdit
            | Self::StopBash
            | Self::StopAgent
            | Self::StopProgram
            | Self::StopWorkflow
            | Self::SendMessage
            | Self::ListAgents
            | Self::TodoWrite
            | Self::CronCreate
            | Self::CronDelete
            | Self::CronList
            | Self::ScheduleWakeup
            | Self::ToolSearch
            | Self::CallTool
            | Self::EnterWorktree
            | Self::ExitWorktree
            | Self::Skill => None,
        }
    }

    pub(crate) fn concurrency_safe(self, input: &Value) -> bool {
        match self {
            Self::ReadFile | Self::Grep | Self::Glob => true,
            // These inspect or signal resources already created by a gated call.
            Self::BashOutput | Self::StopBash => true,
            // tool_search grows the capability store. Keep it as an ordering
            // barrier so a following read-only deferred call deterministically
            // observes the receipt while a preceding call deterministically
            // remains locked.
            Self::ToolSearch => false,
            // Only malformed envelopes stay a call_tool by the time anything
            // classifies them; a well-formed one was rewritten to the inner
            // tool at dispatch entry and is judged as that tool.
            Self::CallTool => false,
            // skill only reads a skill file and returns its expanded body —
            // pure, no shared-state races (side effects come from tools the
            // returned instructions later prompt, gated individually).
            Self::Skill => true,
            Self::Bash => {
                input["command"]
                    .as_str()
                    .is_some_and(|cmd| match crate::shell::analyze_bash(cmd) {
                        crate::shell::BashAnalysis::Commands(cmds) => {
                            !cmds.is_empty()
                                && cmds.iter().all(|c| crate::shell::argv_is_readonly(c))
                        }
                        crate::shell::BashAnalysis::Opaque => false,
                    })
            }
            Self::AskUserQuestion | Self::CronList | Self::ListAgents => true,
            Self::SendMessage
            | Self::CronCreate
            | Self::CronDelete
            | Self::ScheduleWakeup
            | Self::TodoWrite => false,
            // Consecutive run_agent calls may run in parallel; child tool calls
            // are still gated independently. The two code executors say the
            // same thing for the same reason: the program itself touches
            // nothing, and every `tools.<name>()` and `agent()` call it makes
            // re-enters the whole gate.
            Self::RunAgent | Self::Workflow | Self::RunProgram => true,
            // Resource-specific stops only signal an owned cancellation token.
            // wait_for_activity blocks, so it must run alone.
            Self::StopAgent | Self::StopProgram | Self::StopWorkflow => true,
            Self::WaitForActivity => false,
            Self::PowerShell | Self::WriteFile | Self::EditFile | Self::NotebookEdit => false,
            // Session state changes, serialized on purpose: two of these in one
            // batch would race over which mode or worktree the rest of the
            // round runs in. Written down rather than left to fall into a
            // conservative default.
            Self::EnterPlanMode | Self::ExitPlanMode | Self::EnterWorktree | Self::ExitWorktree => {
                false
            }
        }
    }

    /// Whether this call is read-only in the permission sense: nothing on the
    /// user's system to sign off on, so plan mode admits it and the gate does
    /// not prompt. Two tools answer from the call itself rather than the name.
    pub(crate) fn readonly(self, call: &crate::permissions::CallFacts) -> bool {
        match self {
            Self::ReadFile | Self::Grep | Self::Glob => true,
            // bash_output reads registry state; stop_bash only signals
            // processes the agent itself started via bash — neither can
            // touch anything the original bash call wasn't already gated on.
            Self::BashOutput | Self::StopBash => true,
            // run_agent itself touches nothing; every child tool call passes
            // through this same gate.
            Self::RunAgent | Self::SendMessage | Self::ListAgents => true,
            // Creating a managed tree and keeping one are session controls. An
            // existing-path Enter and remove action are intercepted as hazards
            // before this verdict; remove is also mutating for the plan-mode gate.
            Self::EnterWorktree => true,
            Self::ExitWorktree => !call.removing_worktree,
            // exit_plan_mode only shows the plan and flips the session mode —
            // no system side effect. Read-only here so it passes the plan-mode
            // gate and does its own approval (Permissions::confirm_exit_plan).
            Self::AskUserQuestion | Self::EnterPlanMode | Self::ExitPlanMode => true,
            // Waiting only blocks; resource-specific stops only signal owned
            // cancellation tokens. None bypasses the stopped work's own gates.
            Self::WaitForActivity | Self::StopAgent | Self::StopProgram | Self::StopWorkflow => {
                true
            }
            // run_program (code-mode) itself touches nothing; every tools.<name>()
            // and agent() call the program makes re-enters this same gate.
            Self::RunProgram | Self::Workflow => true,
            // tool_search only reads tool definitions and marks them
            // unlocked; the unlocked tool's own calls still pass this gate.
            Self::ToolSearch | Self::CronList => true,
            // Only a malformed envelope is still a call_tool here; a well-formed
            // one is judged as the tool it wraps.
            Self::CallTool => false,
            // The todo list mutates only session memory — nothing on the
            // user's system to sign off on. Dispatcher concurrency still
            // serializes the write independently of this permission fact.
            Self::TodoWrite => true,
            // skill only loads a local skill file's instructions into the
            // conversation — no system side effect (cc never prompts to
            // activate one); tools those instructions later prompt are gated
            // on their own.
            Self::Skill => true,
            // Writing a cron entry or a wake-up arms future unattended work.
            // The scheduler gate below (`Permissions` layer 6) is what decides
            // whether these prompt; they are not read-only.
            Self::CronCreate | Self::CronDelete | Self::ScheduleWakeup => false,
            Self::PowerShell => false,
            Self::Bash => call.bash_is_readonly(),
            Self::WriteFile | Self::EditFile | Self::NotebookEdit => false,
        }
    }

    /// The definition sent to the model.
    pub(crate) fn def(self, cx: &DefCx<'_>) -> ToolDef {
        match self {
            Self::Bash => bash_def(),
            Self::BashOutput => bash_output_def(),
            Self::StopBash => stop_bash_def(),
            Self::PowerShell => powershell_def(),
            Self::ReadFile => read_file_def(),
            Self::WriteFile => write_file_def(),
            Self::EditFile => edit_file_def(),
            Self::NotebookEdit => notebook_edit_def(),
            Self::Grep => grep_def(),
            Self::Glob => glob_def(),
            Self::TodoWrite => todo::todo_write_def(),
            Self::SendMessage => agent_message::send_message_def(),
            Self::ListAgents => agent_message::list_agents_def(),
            Self::RunAgent => run_agent_def(),
            Self::WaitForActivity => wait_for_activity_def(),
            Self::StopAgent => stop_agent_def(),
            Self::ToolSearch => tool_search::tool_search_def(),
            Self::CallTool => tool_search::call_tool_def(),
            Self::RunProgram => {
                codemode::run_program_def(cx.builtins, cx.inline_sources, cx.deferred_sources)
            }
            Self::StopProgram => stop_program_def(),
            Self::CronCreate => scheduler::cron_create_def(),
            Self::CronDelete => scheduler::cron_delete_def(),
            Self::CronList => scheduler::cron_list_def(),
            Self::ScheduleWakeup => scheduler::schedule_wakeup_def(),
            Self::AskUserQuestion => question::ask_user_question_def(),
            Self::EnterPlanMode => plan_mode::enter_plan_mode_def(),
            Self::ExitPlanMode => plan_mode::exit_plan_mode_def(),
            Self::Workflow => workflow::workflow_def(),
            Self::StopWorkflow => workflow::stop_workflow_def(),
            Self::EnterWorktree => worktree_tool::enter_worktree_def(),
            Self::ExitWorktree => worktree_tool::exit_worktree_def(),
            Self::Skill => skill::skill_tool_def(),
        }
    }
}

/// The POSIX definition, patched on Windows: there the shell is Git for
/// Windows' `bash.exe` and there is no filesystem/network sandbox to opt out of.
fn bash_def() -> ToolDef {
    let def =
    ToolDef {
        name: "bash".into(),
        description: "Run a shell command with `sh -lc`. Prefer the dedicated tools over shell equivalents: grep (not grep/rg), glob (not find), read_file (not cat/head/tail), edit_file (not sed); reserve bash for real shell work like builds, tests, installs, and git. stdout and stderr are merged; a non-zero exit status is appended. Default timeout 60s. For long-running commands (dev servers, watches, slow builds) set background=true instead of appending '&'. A background call returns a bg-N id and output file; inspect it with bash_output and stop it with stop_bash. When OS sandboxing is active, commands run with file writes limited to the workspace and temp directories and no network beyond this machine — a loopback listener (a test server) works, so do not disable the sandbox for one; a failure that looks sandbox-caused is annotated in the result.".into(),
        schema: json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "The command to run"},
                "description": {"type": ["string", "null"], "minLength": 1, "maxLength": MAX_DISPLAY_DESCRIPTION_CHARS, "pattern": ".*\\S.*", "description": "Optional short, single-line display label. It never changes the command or result."},
                "timeout_ms": {"type": "integer", "description": "Timeout in milliseconds (default 60000); ignored when background=true"},
                "background": {"type": "boolean", "description": "Run in the background: return immediately with a bg-N id and output file path (default false)"},
                "disable_sandbox": {"type": "boolean", "description": "Run without the OS sandbox. Only set this after a command failed from sandbox restrictions (writes outside the workspace, network access) and that access is genuinely needed — never preemptively; the unsandboxed run requires user approval."}
            },
            "required": ["command"],
            "additionalProperties": false
        }),
    };
    #[cfg(windows)]
    let def = {
        let mut def = def;
        def.description = "Run a command with the validated Git for Windows `bash.exe -lc`. stdout and stderr are merged; a non-zero exit status is appended. Default timeout 60s. For long-running commands set background=true. Windows Job Object containment owns the full process tree; filesystem/network sandboxing is not implemented. Prefer forward slashes inside Bash commands.".into();
        def.schema["properties"]
            .as_object_mut()
            .expect("bash properties are an object")
            .remove("disable_sandbox");
        def
    };
    def
}

fn bash_output_def() -> ToolDef {
    ToolDef {
        name: "bash_output".into(),
        description: "Retrieve the status and output of a background bash command. Blocks until it finishes by default (up to timeout_ms); pass block=false to peek without waiting. Returns the tail of the output; read the output file for the rest.".into(),
        schema: json!({
            "type": "object",
            "properties": {
                "bash_id": {"type": "string", "description": "ID from a background bash call, e.g. bg-1"},
                "block": {"type": "boolean", "description": "Wait for completion (default true)"},
                "timeout_ms": {"type": "integer", "description": "Max wait when blocking (default 30000, max 600000)"}
            },
            "required": ["bash_id"],
            "additionalProperties": false
        }),
    }
}

fn stop_bash_def() -> ToolDef {
    ToolDef {
        name: "stop_bash".into(),
        description: "Stop a running background bash command by its bg-N id; terminates the whole owned process tree. Any other resource id is rejected with a directed correction.".into(),
        schema: json!({
            "type": "object",
            "properties": {
                "bash_id": {"type": "string", "description": "ID from a background bash call, e.g. bg-1"}
            },
            "required": ["bash_id"],
            "additionalProperties": false
        }),
    }
}

fn powershell_def() -> ToolDef {
    ToolDef {
        name: "powershell".into(),
        description: "Run a foreground PowerShell command on native Windows with a fixed non-interactive, no-profile encoded invocation. PowerShell is treated as opaque and normally requires approval. Default timeout 60s; background execution and Windows shell sandboxing are unavailable.".into(),
        schema: json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "The original PowerShell script to run"},
                "timeout_ms": {"type": "integer", "description": "Timeout in milliseconds (default 60000)"}
            },
            "required": ["command"],
            "additionalProperties": false
        }),
    }
}

fn read_file_def() -> ToolDef {
    ToolDef {
        name: "read_file".into(),
        description: "Read a file. Raw input is limited to 5 MiB for text and images, or 10 MiB for lowercase `.ipynb` notebooks. Text files return numbered lines formatted as `{n}\\t{line}` with a bounded character budget; use offset/limit to page. Jupyter notebooks return cell-aware `<cell id=\"…\">` content and code outputs, including image blocks. Empty files and offsets past EOF return explicit warnings. Image files (png, jpeg, gif, webp) are returned as an image you can see — offset/limit do not apply. PDFs return an explicit unsupported error.".into(),
        schema: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "offset": {"type": "integer", "minimum": 0, "description": "Line number to start from (default 1; 0 is accepted for compatibility)"},
                "limit": {"type": "integer", "minimum": 0, "description": "Max lines to return (omit or pass 0 for all lines within the character budget)"}
            },
            "required": ["path"]
        }),
    }
}

fn write_file_def() -> ToolDef {
    ToolDef {
        name: "write_file".into(),
        description: "Write the provided full content to a file. A new file safely creates missing parent directories only after approval. Overwriting an existing file requires a complete, fresh read in this session, then replaces it atomically with the provided content exactly as given.".into(),
        schema: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string"}
            },
            "required": ["path", "content"]
        }),
    }
}

fn edit_file_def() -> ToolDef {
    ToolDef {
        name: "edit_file".into(),
        description: "Replace exact old_string matches with new_string in an existing UTF-8 file of at most 5 MiB. The file must have been read in this session — any range qualifies; if it changed since that read the edit still applies and the result says so. Raw matches take priority; when none exist, LF old_string may match CRLF text without normalizing untouched bytes. Fails if old_string is absent or matches more than once without replace_all. Never creates a missing file or parent directory.".into(),
        schema: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "old_string": {"type": "string"},
                "new_string": {"type": "string"},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false)"}
            },
            "required": ["path", "old_string", "new_string"]
        }),
    }
}

fn notebook_edit_def() -> ToolDef {
    ToolDef {
        name: "notebook_edit".into(),
        description: "Replaces, inserts, or deletes a single cell in a Jupyter notebook (.ipynb file).\n\nUsage:\n- You must use the read_file tool on the notebook in this conversation before editing — this tool will fail otherwise.\n- `notebook_path` must be an absolute path.\n- `cell_id` is the `id` attribute shown in the read_file tool's `<cell id=\"...\">` output. It is required for `replace` and `delete`.\n- `edit_mode` defaults to `replace`. Use `insert` to add a new cell after the cell with the given `cell_id` (or at the beginning of the notebook if `cell_id` is omitted) — `cell_type` is required when inserting. Use `delete` to remove the cell.".into(),
        schema: json!({
            "type": "object",
            "properties": {
                "notebook_path": {
                    "type": "string",
                    "description": "The absolute path to the Jupyter notebook file to edit (must be absolute, not relative)"
                },
                "cell_id": {
                    "type": "string",
                    "description": "The ID of the cell to edit. When inserting a new cell, the new cell will be inserted after the cell with this ID, or at the beginning if not specified."
                },
                "new_source": {
                    "type": "string",
                    "description": "The new source for the cell"
                },
                "cell_type": {
                    "type": "string",
                    "enum": ["code", "markdown"],
                    "description": "The type of the cell (code or markdown). If not specified, it defaults to the current cell type. If using edit_mode=insert, this is required."
                },
                "edit_mode": {
                    "type": "string",
                    "enum": ["replace", "insert", "delete"],
                    "description": "The type of edit to make (replace, insert, delete). Defaults to replace."
                }
            },
            "required": ["notebook_path", "new_source"],
            "additionalProperties": false
        }),
    }
}

fn grep_def() -> ToolDef {
    ToolDef {
        name: "grep".into(),
        description: "Search file contents with a regular expression (ripgrep-style; Rust regex syntax, no backreferences/lookaround). Respects .gitignore, searches hidden files, skips binary files; prefer this over grep/rg in bash. Results cap at head_limit; page with offset.".into(),
        schema: json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Regular expression to search for"},
                "path": {"type": "string", "description": "File or directory to search (default: current directory)"},
                "glob": {"type": "string", "description": "Filter files with a glob, e.g. \"*.rs\" or \"*.{ts,tsx}\""},
                "type": {"type": "string", "description": "Filter by file type, e.g. rust, js, py, go"},
                "output_mode": {"type": "string", "enum": ["files_with_matches", "content", "count"], "description": "files_with_matches: file paths newest-first (default); content: matching lines as path:line:text; count: per-file match counts"},
                "-i": {"type": "boolean", "description": "Case-insensitive (default false)"},
                "-n": {"type": "boolean", "description": "Show line numbers in content mode (default true)"},
                "-A": {"type": "integer", "minimum": 0, "description": "Lines shown after each match (content mode only)"},
                "-B": {"type": "integer", "minimum": 0, "description": "Lines shown before each match (content mode only)"},
                "-C": {"type": "integer", "minimum": 0, "description": "Lines shown before and after each match (content mode only; overrides -A/-B)"},
                "-o": {"type": "boolean", "description": "Print only matched non-empty text (content mode only; default false)"},
                "head_limit": {"type": "integer", "minimum": 0, "description": "Max results returned (default 250, 0 = unlimited)"},
                "offset": {"type": "integer", "minimum": 0, "description": "Skip this many results before head_limit applies (default 0)"},
                "multiline": {"type": "boolean", "description": "Patterns may span lines and . matches newlines (default false)"}
            },
            "required": ["pattern"]
        }),
    }
}

fn glob_def() -> ToolDef {
    ToolDef {
        name: "glob".into(),
        description: "Find files by glob pattern, e.g. \"**/*.rs\", \"src/*.ts\" or \"*.{js,json}\" (gitignore-style: a bare name matches at any depth). Respects .gitignore. Returns paths sorted by modification time, newest first, capped at 100.".into(),
        schema: json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Glob pattern to match file paths against"},
                "path": {"type": "string", "description": "Directory to search in (default: current directory)"}
            },
            "required": ["pattern"]
        }),
    }
}

fn run_agent_def() -> ToolDef {
    ToolDef {
        name: "run_agent".into(),
        description: "Run one open-ended sub-agent with a fresh history on a self-contained prompt. Dispatch one only when the user, an AGENTS.md file, or a skill asks for delegation, and give it the part you are not doing yourself: a sub-agent that restates your own task costs several times what doing it yourself costs and returns little you would not have found. Use bash for fixed tool/code batching (a shell one-liner or `python3 -c` beats a wrapper), and Workflow only when the user explicitly requested multi-agent orchestration. By default this blocks and returns the final text; while main is synchronously waiting it has no model round in which to call send_message, so use background=true when main must send follow-up instructions during the run. Consecutive run_agent calls in one model response run in parallel. Set background=true to return immediately with an agent-N id and receive a bounded result preview later as an inbox message (oversized success text is saved to a file whose path the preview names). Optional description is display-only and falls back to a prompt preview. Background results are delivered automatically; call wait_for_activity once only when you truly need to block for any activity, never as an output/status polling loop. Stop Agent only with that agent-N id. Background work is session-scoped, not durable across session shutdown. Sub-agents cannot spawn further sub-agents. Pass agent_type for a configured specialized agent; omit it for the general-purpose agent. Model-generated text is not deterministic and runtime gates still enforce tools, permissions, sandbox, and result limits.".into(),
        schema: json!({
            "type": "object",
            "properties": {
                "description": {"type": ["string", "null"], "minLength": 1, "maxLength": MAX_DISPLAY_DESCRIPTION_CHARS, "pattern": ".*\\S.*", "description": "Optional short, single-line display label. It never changes the prompt or result."},
                "prompt": {"type": "string", "description": "Complete standalone work description"},
                "agent_type": {"type": ["string", "null"], "minLength": 1, "description": "Name of a configured agent type; omit for a general-purpose sub-agent"},
                "model": {"type": ["string", "null"], "minLength": 1, "pattern": ".*\\S.*", "description": "Optional model override on this sub-agent's inherited frozen provider; it must be in that provider's model allowlist"},
                "background": {"type": "boolean", "description": "Return an agent-N id immediately and deliver the result later (default false)"},
                "isolation": {"type": "string", "enum": ["shared", "worktree"], "description": "shared (default) uses the current workspace; worktree gives the agent a private git worktree"}
            },
            "required": ["prompt"],
            "additionalProperties": false
        }),
    }
}

fn wait_for_activity_def() -> ToolDef {
    ToolDef {
        name: "wait_for_activity".into(),
        description: "Wait once for any background shell, Agent, Program, or Workflow activity when the caller truly needs to block. Pending inbox input also wakes it. Takes no resource ID and never reads or drains a result; results are delivered automatically at the next step/final/idle boundary even if this tool is never called. A timeout is not a background failure and consumes nothing. Do not call repeatedly as a status/output polling loop.".into(),
        schema: json!({
            "type": "object",
            "properties": {
                "timeout_ms": {"type": "integer", "description": "Max wait (default 30000, min 10000, max 3600000)"}
            },
            "additionalProperties": false
        }),
    }
}

fn stop_agent_def() -> ToolDef {
    ToolDef {
        name: "stop_agent".into(),
        description: "Stop a running background agent by its agent-N id. It ends without reporting a result. Use stop_program for program-N, stop_workflow for workflow-N, or stop_bash for bg-N.".into(),
        schema: json!({
            "type": "object",
            "properties": {
                "agent_id": {"type": "string", "description": "The agent-N id from run_agent with background=true"}
            },
            "required": ["agent_id"],
            "additionalProperties": false
        }),
    }
}

/// Kept on `run_program`'s surface flag: without that tool there is no
/// `program-N` to stop.
fn stop_program_def() -> ToolDef {
    ToolDef {
        name: "stop_program".into(),
        description: "Stop a running background code-mode program by its program-N id. It ends without reporting a result. Use stop_agent for agent-N, stop_workflow for workflow-N, or stop_bash for bg-N; this is not a durable run_id.".into(),
        schema: json!({
            "type": "object",
            "properties": {
                "program_id": {"type": "string", "description": "The program-N id from run_program with background=true"}
            },
            "required": ["program_id"],
            "additionalProperties": false
        }),
    }
}

/// Every built-in tool name, in wire order. Public so a front-end's display
/// layer can assert it renders all of them without the enum leaving this crate.
pub fn builtin_tool_names() -> Vec<&'static str> {
    ALL.iter().map(|builtin| builtin.name()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ALL` is the one hand-written list in this file, and everything that
    /// walks the catalog walks it — so a variant missing from it is a tool that
    /// silently stops being offered. The length assertion is the only thing
    /// that can catch that: bump it deliberately when adding a tool.
    #[test]
    fn builtin_all_is_complete() {
        assert_eq!(ALL.len(), 32);
        let mut names: Vec<&str> = ALL.iter().map(|builtin| builtin.name()).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "duplicate built-in name");
        for builtin in ALL {
            assert_eq!(
                Builtin::from_name(builtin.name()),
                Some(*builtin),
                "{} does not round-trip",
                builtin.name()
            );
        }
    }

    /// Each definition carries the name its variant answers to; a mismatch
    /// would route the model's call to a different tool than the one it read
    /// the schema of.
    #[test]
    fn every_definition_names_its_own_variant() {
        let cx = DefCx::default();
        for builtin in ALL {
            assert_eq!(builtin.def(&cx).name, builtin.name());
        }
    }

    /// `description` is one contract, not three: every tool that takes a
    /// display label advertises the same constraints the shared validator
    /// enforces, and only the prose naming that tool's own effects differs.
    #[test]
    fn display_description_is_one_schema_contract() {
        let cx = DefCx::default();
        let constraints = |builtin: Builtin| {
            let mut field = builtin.def(&cx).schema["properties"]["description"].clone();
            field
                .as_object_mut()
                .expect("a display description field is an object")
                .remove("description");
            field
        };
        assert_eq!(
            constraints(Builtin::Bash),
            json!({
                "type": ["string", "null"],
                "minLength": 1,
                "maxLength": MAX_DISPLAY_DESCRIPTION_CHARS,
                "pattern": ".*\\S.*"
            })
        );
        assert_eq!(constraints(Builtin::Bash), constraints(Builtin::RunAgent));
        assert_eq!(constraints(Builtin::Bash), constraints(Builtin::RunProgram));
    }

    /// The panel falls back to a tool's own name for anything it does not
    /// know — right for an MCP tool the user named themselves, wrong for a
    /// built-in, whose raw name is an implementation detail.
    #[test]
    fn every_builtin_has_a_human_title() {
        for builtin in ALL {
            assert_ne!(
                builtin.title(),
                builtin.name(),
                "{} shows its raw name in the approval panel",
                builtin.name()
            );
        }
    }
}
