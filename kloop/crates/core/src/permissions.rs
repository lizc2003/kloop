//! permissions — the layered gate run before every tool execution, shaped
//! after claude-code's `hasPermissionsToUseToolInner` pipeline:
//!
//! deny rules → sensitive-read hard block → plan-mode read-only gate →
//! safety checks → ask rules → session scheduler controls → sandbox
//! auto-allow → bypass → read-only self-verdict → acceptEdits → allow rules
//! → session cache → ask the user.
//!
//! Two invariants carried over from cc: **deny always beats allow**, and
//! **safety checks (destructive commands, sensitive paths) are immune to
//! bypass mode**. A denial becomes an is_error tool_result — the model can
//! take another approach; the turn does not end.
//!
//! Plan mode ([`Mode::Plan`], cc's `plan` permission mode) sits below deny and
//! the credential-read hard block: a write/mutating call is refused outright —
//! agent explores read-only and acts only after the user approves the plan via
//! `exit_plan_mode`. It is placed above safety on purpose: a destructive
//! command in plan mode is a flat "no", not a "[destructive] approve?" prompt
//! whose yes would break the read-only promise. `exit_plan_mode` counts as
//! read-only here (it only shows the plan and flips the mode), so it passes.
//!
//! Bypass mode ([`Mode::Bypass`]) waives the rule/ask layers but not the two
//! things above it, and not a call that gave up containment itself: a bash
//! call carrying `disable_sandbox` still reaches the user, because bypass
//! trusts what the model is doing rather than its decision to remove the
//! sandbox first. Beyond the safety checks above it does NOT vet the command:
//! [`crate::shell::argv_is_dangerous`] is a blocklist by construction, holding
//! only what is irreversible and outside git's reach, so on a host with no
//! sandbox that short list is the whole command-level net. Containment is the
//! real one.
//!
//! The sandbox auto-allow layer (cc's `autoAllowBashIfSandboxed`) is the
//! sandbox/approval coupling: a bash call the OS sandbox will contain needs
//! no human sign-off — containment replaces the parse-level vetting of the
//! layers below it, opaque scripts included. Everything above it still has
//! its say: deny rules, sensitive reads, safety checks, and — deliberately
//! stricter than cc — explicit ask rules ("always confirm this" is the user's
//! automation outranks it). Dispatch feeds the verdict in per call, so an
//! escaped call (`disable_sandbox: true`) faces the gate like any other.
//!
//! Bash content decisions run on the tree-sitter analysis in [`crate::shell`]:
//! a script the parser cannot fully vouch for is *opaque* — it can never be
//! auto-approved, never match an allow rule, and never enter the session
//! cache; outside a contained sandbox it always goes to the user. Native
//! Windows PowerShell is always opaque: it never enters the Bash parser,
//! bypass still asks, and only an explicit whole-tool rule can auto-decide it.

use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::bail;
use globset::GlobMatcher;
use serde_json::Value;

use crate::shell::BashAnalysis;
use crate::shell::analyze_bash;
use crate::shell::argv_is_dangerous;
use crate::shell::argv_is_readonly;
use crate::shell::strip_wrappers;
use crate::tools::Builtin;

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
    Deny,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionNotice {
    pub message: String,
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
    pub preview: Option<String>,
}

/// The outcome of [`Permissions::escalate_sandbox`] — the code-level
/// escalation loop's consent step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EscalationOutcome {
    /// Approved (or bypass mode): re-run the command without the sandbox.
    Approved,
    /// The user was asked and declined: keep the sandboxed failure and do
    /// not invite a disable_sandbox retry.
    Declined,
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
    /// Ask before any unvouched-for call — the gate with the most oversight, and
    /// the mode in effect when no `--permission-mode` is given. cc's `default`
    /// mode, which it surfaces to the user as "Manual"; kloop uses `manual` as
    /// the one name, value and label alike.
    #[default]
    Manual,
    /// File writes inside the working directory are auto-approved.
    AcceptEdits,
    /// Everything is approved except deny rules and safety checks (cc
    /// `bypassPermissions` semantics — those two layers are immune).
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
            Mode::AcceptEdits => "accept-edits",
            Mode::Bypass => "bypass",
            Mode::Plan => "plan",
        }
    }

    /// The next mode in the shift+Tab cycle. Bypass is deliberately NOT reached
    /// by cycling — it is the dangerous one, opted into explicitly with
    /// `--permission-mode bypass`; stepping out of it lands on manual.
    pub fn cycled(self) -> Mode {
        match self {
            Mode::Manual => Mode::AcceptEdits,
            Mode::AcceptEdits => Mode::Plan,
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

#[derive(Clone, Debug)]
enum Rule {
    Tool(String),
    BashPrefix {
        tokens: Vec<String>,
        wildcard: bool,
    },
    PathGlob {
        tool: String,
        glob: GlobMatcher,
    },
    /// Consent to re-run one bash command *outside* the OS sandbox after it was
    /// denied inside it. Deliberately its own variant rather than a `bash(...)`
    /// rule: allowing a command to run is not the same permission as allowing it
    /// to run uncontained, so neither form may stand in for the other. It never
    /// matches the ordinary gate (`matches_argv` below) and `bash`/`Tool` rules
    /// never match an escalation.
    SandboxEscalatePrefix {
        tokens: Vec<String>,
        wildcard: bool,
    },
}

impl Rule {
    fn matches_tool(&self, name: &str) -> bool {
        matches!(self, Rule::Tool(t) if t == name)
    }

    /// Whether this rule vouches for one parsed bash argv.
    fn matches_argv(&self, argv: &[String]) -> bool {
        match self {
            Rule::Tool(t) => t == "bash",
            Rule::BashPrefix { tokens, wildcard } => argv_has_prefix(tokens, *wildcard, argv),
            Rule::PathGlob { .. } | Rule::SandboxEscalatePrefix { .. } => false,
        }
    }

    /// Whether this rule vouches for re-running one parsed bash argv without the
    /// sandbox. Only the dedicated variant can: a whole-tool `bash` rule or a
    /// `bash(<prefix>)` rule says the call may run, not that it may run
    /// uncontained.
    fn matches_escalation_argv(&self, argv: &[String]) -> bool {
        match self {
            Rule::SandboxEscalatePrefix { tokens, wildcard } => {
                argv_has_prefix(tokens, *wildcard, argv)
            }
            Rule::Tool(_) | Rule::BashPrefix { .. } | Rule::PathGlob { .. } => false,
        }
    }

    fn matches_path(&self, tool: &str, facts: &PathFacts) -> bool {
        match self {
            Rule::Tool(t) => t == tool,
            Rule::PathGlob {
                tool: rule_tool,
                glob,
            } => {
                rule_tool == tool
                    && (facts
                        .relative
                        .as_ref()
                        .is_some_and(|rel| glob.is_match(rel))
                        || facts
                            .original_relative
                            .as_ref()
                            .is_some_and(|rel| glob.is_match(rel))
                        || glob.is_match(&facts.normalized)
                        || glob.is_match(&facts.original))
            }
            Rule::BashPrefix { .. } | Rule::SandboxEscalatePrefix { .. } => false,
        }
    }
}

/// Whether `argv` matches an argv-prefix pattern: the whole command when the
/// rule was written without a trailing `*`, just its head when it had one.
/// `bash(...)` and `sandbox_escalate(...)` are the same shape over two
/// different consents, so the shape itself lives in one place.
fn argv_has_prefix(tokens: &[String], wildcard: bool, argv: &[String]) -> bool {
    let length_matches = if wildcard {
        argv.len() >= tokens.len()
    } else {
        argv.len() == tokens.len()
    };
    length_matches && argv.iter().zip(tokens).all(|(a, t)| a == t)
}

/// `<tokens…>` or `<tokens…> *` — the pattern both argv-prefix rules are
/// written in. `tool` only names the rule in the error.
fn parse_prefix_pattern(tool: &str, inner: &str) -> Result<(Vec<String>, bool)> {
    let mut tokens: Vec<String> = inner.split_whitespace().map(str::to_string).collect();
    let wildcard = tokens.last().is_some_and(|t| t == "*");
    if wildcard {
        tokens.pop();
    }
    if tokens.is_empty() {
        bail!("rule '{tool}({inner})': empty command pattern");
    }
    Ok((tokens, wildcard))
}

fn parse_rule(entry: &str) -> Result<Rule> {
    if let Some((tool, inner)) = entry
        .split_once('(')
        .and_then(|(t, rest)| rest.strip_suffix(')').map(|inner| (t, inner)))
    {
        match tool {
            "bash" => {
                let (tokens, wildcard) = parse_prefix_pattern(tool, inner)?;
                Ok(Rule::BashPrefix { tokens, wildcard })
            }
            "sandbox_escalate" => {
                let (tokens, wildcard) = parse_prefix_pattern(tool, inner)?;
                Ok(Rule::SandboxEscalatePrefix { tokens, wildcard })
            }
            "powershell" => {
                bail!(
                    "rule '{entry}': powershell(...) prefix rules are unsupported; PowerShell v1 only accepts the whole-tool rule 'powershell'"
                )
            }
            "write_file" | "edit_file" | "read_file" | "notebook_edit" => {
                // Match case-insensitively on case-folding filesystems so a
                // deny like `write_file(secrets/**)` is not slipped by `Secrets/`.
                let glob = globset::GlobBuilder::new(inner.trim())
                    .case_insensitive(FS_FOLDS_CASE)
                    .build()
                    .with_context(|| format!("rule '{entry}': invalid glob"))?
                    .compile_matcher();
                Ok(Rule::PathGlob {
                    tool: tool.to_string(),
                    glob,
                })
            }
            other => bail!("rule '{entry}': tool '{other}' does not take a pattern"),
        }
    } else if !entry.is_empty() && entry.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Ok(Rule::Tool(entry.to_string()))
    } else {
        bail!("rule '{entry}': expected a tool name, bash(<tokens>), or <file_tool>(<glob>)");
    }
}

fn parse_rules(entries: &[String]) -> Result<Vec<Rule>> {
    entries
        .iter()
        .map(String::as_str)
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(parse_rule)
        .collect()
}

#[derive(Clone, Copy, Debug)]
struct ModeState {
    current: Mode,
    pre_plan: Mode,
    capability_epoch: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PermissionCapabilityEpoch {
    project: u64,
    mode: u64,
    workspace_session: u64,
}

fn advance_epoch(epoch: &mut u64, owner: &str) {
    *epoch = epoch
        .checked_add(1)
        .unwrap_or_else(|| panic!("{owner} capability epoch exhausted"));
}

pub struct GlobalPermissionPolicy {
    deny: Vec<Rule>,
    ask: Vec<Rule>,
}

impl GlobalPermissionPolicy {
    pub fn new(deny: &[String], ask: &[String]) -> Result<Self> {
        Ok(Self {
            deny: parse_rules(deny)?,
            ask: parse_rules(ask)?,
        })
    }

    pub fn empty() -> Self {
        Self {
            deny: Vec::new(),
            ask: Vec::new(),
        }
    }
}

struct ProjectPolicyState {
    snapshot: ProjectPolicySnapshot,
    capability_epoch: u64,
}

pub struct ProjectPermissionPolicy {
    project_id: Option<crate::project::ProjectId>,
    state: RwLock<ProjectPolicyState>,
    writer: Option<Arc<dyn ProjectPermissionWriter>>,
}

impl ProjectPermissionPolicy {
    pub fn available(
        project_id: crate::project::ProjectId,
        snapshot: ProjectPolicySnapshot,
        writer: Option<Arc<dyn ProjectPermissionWriter>>,
    ) -> Self {
        Self {
            project_id: Some(project_id),
            state: RwLock::new(ProjectPolicyState {
                snapshot,
                capability_epoch: 0,
            }),
            writer,
        }
    }

    pub fn unavailable() -> Self {
        Self {
            project_id: None,
            state: RwLock::new(ProjectPolicyState {
                snapshot: ProjectPolicySnapshot::empty(),
                capability_epoch: 0,
            }),
            writer: None,
        }
    }

    pub fn snapshot(&self) -> ProjectPolicySnapshot {
        self.state.read().unwrap().snapshot.clone()
    }

    fn capability_epoch(&self) -> u64 {
        self.state.read().unwrap().capability_epoch
    }

    fn matches_allow(&self, name: &str, call: &CallFacts) -> bool {
        let state = self.state.read().unwrap();
        allow_rules_match(state.snapshot.allow.parsed(), name, call)
    }

    /// Whether durable rules consent to re-running every one of these argvs
    /// outside the sandbox. Strict like `matches_allow`: one uncovered segment
    /// means the whole command still has to be asked about.
    fn matches_escalation(&self, argvs: &[Vec<String>]) -> bool {
        if argvs.is_empty() {
            return false;
        }
        let state = self.state.read().unwrap();
        let rules = state.snapshot.allow.parsed();
        argvs
            .iter()
            .all(|argv| rules.iter().any(|rule| rule.matches_escalation_argv(argv)))
    }

    fn can_persist(&self) -> bool {
        self.project_id.is_some() && self.writer.is_some()
    }

    fn refresh_from_store(&self, snapshot: ProjectPolicySnapshot) {
        let mut state = self.state.write().unwrap();
        // Store loads can race durable writes. Only a snapshot at least as new
        // as the live policy may refresh it; explicit invalidation is separate.
        if snapshot.revision < state.snapshot.revision || snapshot == state.snapshot {
            return;
        }
        state.snapshot = snapshot;
        advance_epoch(&mut state.capability_epoch, "project policy");
    }

    fn invalidate_from_store(&self) {
        let mut state = self.state.write().unwrap();
        state.snapshot = ProjectPolicySnapshot::empty();
        // Invalidation is an event even when the durable revision falls back to
        // zero or the visible rules were already empty. The monotonic epoch keeps
        // an old deferred-tool receipt from surviving that ABA transition.
        advance_epoch(&mut state.capability_epoch, "project policy");
    }

    async fn persist(
        &self,
        additions: ProjectAllowRules,
    ) -> std::result::Result<ProjectPolicySnapshot, ProjectPolicyStoreError> {
        let project_id = self
            .project_id
            .clone()
            .ok_or(ProjectPolicyStoreError::Unavailable)?;
        let writer = self
            .writer
            .as_ref()
            .ok_or(ProjectPolicyStoreError::Unavailable)?;
        let published = writer.append_allow(project_id, additions).await?;
        let mut state = self.state.write().unwrap();
        if published.revision < state.snapshot.revision {
            return Err(ProjectPolicyStoreError::Invalid);
        }
        if published == state.snapshot {
            return Ok(published);
        }
        state.snapshot = published.clone();
        advance_epoch(&mut state.capability_epoch, "project policy");
        Ok(published)
    }
}

#[derive(Default)]
pub struct ProjectPolicyRegistry {
    policies: Mutex<HashMap<crate::project::ProjectId, Arc<ProjectPermissionPolicy>>>,
}

impl ProjectPolicyRegistry {
    pub fn get_or_insert(
        &self,
        project_id: crate::project::ProjectId,
        snapshot: ProjectPolicySnapshot,
        writer: Arc<dyn ProjectPermissionWriter>,
    ) -> Arc<ProjectPermissionPolicy> {
        let mut policies = self.policies.lock().unwrap();
        match policies.entry(project_id.clone()) {
            std::collections::hash_map::Entry::Occupied(entry) => {
                let policy = Arc::clone(entry.get());
                policy.refresh_from_store(snapshot);
                policy
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                let policy = Arc::new(ProjectPermissionPolicy::available(
                    project_id,
                    snapshot,
                    Some(writer),
                ));
                entry.insert(Arc::clone(&policy));
                policy
            }
        }
    }
    pub fn invalidate(&self, project_id: &crate::project::ProjectId) {
        let policy = self.policies.lock().unwrap().get(project_id).cloned();
        if let Some(policy) = policy {
            policy.invalidate_from_store();
        }
    }
}

#[derive(Default)]
struct WorkspacePermissionCache {
    signatures: HashSet<String>,
    capability_epoch: u64,
}

pub struct PermissionSession {
    mode: Mutex<ModeState>,
    cache: Mutex<HashMap<crate::project::WorkspaceId, WorkspacePermissionCache>>,
    approver: Option<Arc<dyn Approver>>,
}

impl PermissionSession {
    pub fn new(mode: Mode, approver: Option<Arc<dyn Approver>>) -> Self {
        Self {
            mode: Mutex::new(ModeState {
                current: mode,
                pre_plan: Mode::Manual,
                capability_epoch: 0,
            }),
            cache: Mutex::new(HashMap::new()),
            approver,
        }
    }
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
            preview: Some(plan.to_string()),
        };
        match approver.confirm(req).await {
            Decision::Allow(ApprovalScope::Once) => PlanExitOutcome::Approved(self.exit_plan()),
            Decision::Allow(ApprovalScope::WorkspaceSession | ApprovalScope::Project)
            | Decision::Deny => PlanExitOutcome::Declined,
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
    pub async fn check_call(
        &self,
        name: &str,
        input: &Value,
        depth: u8,
        sandbox_auto_allow: bool,
    ) -> Result<Option<PermissionNotice>, String> {
        self.check_call_with_resolved_path(name, input, None, None, depth, sandbox_auto_allow)
            .await
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
        sandbox_auto_allow: bool,
    ) -> Result<Option<PermissionNotice>, String> {
        if self.allow_everything {
            return Ok(None);
        }
        let call = CallFacts::gather(name, input, self.identity.cwd(), resolved_path);
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
            return Err(format!(
                "{name}: blocked by a deny permission rule. Do not retry this call or try to \
                 work around the rule; choose a different approach or ask the user."
            ));
        }

        // 2. Sensitive reads — credentials and agent state must never enter the
        // model context. This is a hard verdict before plan/sandbox/bypass and
        // cannot be remembered or approved away.
        if call.sensitive_read {
            return Err(format!(
                "{name}: reading this sensitive path is blocked. Do not retry or work around the protection."
            ));
        }

        // 3. Plan mode — read-only exploration only. A mutating call is refused
        // outright (deny above still wins; safety/ask below never see one), so
        // the model plans and acts only after the user approves exit_plan_mode.
        // Above safety on purpose: a destructive command here is a flat "no",
        // not a "[destructive] approve?" whose yes would break the promise.
        if self.mode() == Mode::Plan && !call.is_readonly(name) {
            return Err(format!(
                "{name}: this session is in plan mode, so only read-only exploration is allowed \
                 — file edits and commands with side effects are blocked. Keep investigating \
                 read-only, then call exit_plan_mode with your plan to get the user's approval \
                 before making any changes."
            ));
        }

        // 4. Safety checks — bypass-immune, straight to the user.
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

        // 5. Explicit ask rules — "always confirm this"; never remembered.
        if self.matches_ask(name, &call) {
            return self
                .ask_user(name, approval_input, preview_context, depth, None, None)
                .await;
        }

        // Safe session scheduler controls mutate only kloop's owner-scoped
        // registry/private store. Deny, plan mode, hazards and explicit ask
        // rules above retain priority; ordinary manual/accept/bypass modes do
        // not prompt for these controls.
        if matches!(name, "cron_create" | "cron_delete" | "schedule_wakeup") {
            return Ok(None);
        }

        // 6. Sandbox auto-allow — the OS sandbox will contain this call, so
        // nothing below (parse-level vetting, rules, the human) needs to be
        // consulted. Sits under deny/safety/ask: those keep their say.
        if sandbox_auto_allow && matches!(call.shell, Some(ShellFacts::Bash(_))) {
            return Ok(None);
        }

        // 7. Bypass mode — auto-run, with two exceptions.
        //
        // NOT an opaque bash script. Bypass waives the rule/ask layers, not the
        // safety promise: an unparseable command (subshell, redirect,
        // substitution…) could hide an `rm -rf` the destructive check never got
        // to see, so it falls through to the user like everywhere else opaque
        // scripts are refused an auto-verdict (deny/allow skip Opaque, and the
        // cache below can only hold one the user already read and approved
        // verbatim; the sandbox layer above may still auto-allow it because the
        // sandbox *contains* it — this can't).
        //
        // And NOT a call that gave up containment itself. A bash call reaches
        // this layer for one of three reasons, and only the first is the
        // model's own decision: it asked for `disable_sandbox`; the session has
        // no sandbox at all (disabled, or an unsupported platform); or the
        // policy set `auto_allow = false`. Bypass means "I trust what you are
        // doing" — not "I trust you to remove the containment first", which is
        // why `describe_parts` has always had a notice ready for it. The other
        // two reasons stay auto-run: the second is every bash call on a host
        // without a sandbox, so refusing it would retire the mode rather than
        // close a hole, and the third is the user's own two settings
        // disagreeing, which bypass is defined to win.
        //
        // A blocked call is not denied — it falls to the read-only self-verdict
        // (an escaped `ls` is still harmless), then allow rules, the session
        // cache, and finally the user.
        //
        // What this layer does NOT do is vet the command. `argv_is_dangerous`
        // (layer 4, above and bypass-immune) holds only what is irreversible
        // and outside git's reach, so on a host with no sandbox at all that
        // short list is the entire command-level net. That is the accepted
        // position, not an oversight — containment is the real net, and where
        // there is none the mode's name is the warning.
        if self.mode() == Mode::Bypass
            && !call.escapes_sandbox
            && !matches!(
                call.shell,
                Some(ShellFacts::Bash(BashAnalysis::Opaque) | ShellFacts::PowerShellOpaque)
            )
        {
            return Ok(None);
        }

        // 8. Read-only self-verdict.
        if call.is_readonly(name) {
            return Ok(None);
        }

        // 9. acceptEdits: file writes inside the working directory.
        if self.mode() == Mode::AcceptEdits
            && matches!(name, "write_file" | "edit_file" | "notebook_edit")
            && call.path.as_ref().is_some_and(|p| p.inside_cwd)
        {
            return Ok(None);
        }

        // 10. Allow rules.
        if self.matches_allow(name, &call) {
            return Ok(None);
        }

        // 11. Session cache.
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

        // 12. Ask.
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
    ) -> Result<Option<PermissionNotice>, String> {
        let Some(approver) = &self.session.approver else {
            return Err(format!(
                "{name}: approval required but no approver is available in this mode; denied."
            ));
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
                .await,
        };
        let decision = approver.confirm(req).await;
        let Decision::Allow(scope) = decision else {
            return Err(user_denial(name));
        };
        if !approval_scopes.contains(&scope) {
            return Err(user_denial(name));
        }
        match scope {
            ApprovalScope::Once => Ok(None),
            ApprovalScope::WorkspaceSession => {
                let Some(remember) = remember else {
                    return Err(user_denial(name));
                };
                self.remember_in_session(remember.signatures);
                Ok(None)
            }
            ApprovalScope::Project => {
                let Some(remember) = remember else {
                    return Err(user_denial(name));
                };
                let additions =
                    ProjectAllowRules::parse(&remember.rules).map_err(|_| user_denial(name))?;
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
    /// filters, mirroring the gate where deny and safety checks are
    /// bypass-immune.
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
        facts.sensitive
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
        // Bypass (`--permission-mode bypass`) means "don't ask" — escalate as the model-driven
        // disable_sandbox retry already would (it auto-passes the bypass
        // layer of the gate).
        if self.mode() == Mode::Bypass {
            return EscalationOutcome::Approved;
        }
        // A remembered escalation answers before the ask, exactly as a durable
        // allow rule does for the ordinary gate. Opaque scripts produce no
        // payload and so can never be remembered — the same rule the ordinary
        // gate follows, and stricter here matters more.
        let remember = escalation_remember_payload(command);
        if self.blanket_escalation_granted()
            || remember
                .as_ref()
                .is_some_and(|remember| self.escalation_remembered(remember))
        {
            return EscalationOutcome::Approved;
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
        let decision = approver.confirm(req).await;
        let Decision::Allow(scope) = decision else {
            return EscalationOutcome::Declined;
        };
        if !approval_scopes.contains(&scope) {
            return EscalationOutcome::Declined;
        }
        match scope {
            ApprovalScope::Once => EscalationOutcome::Approved,
            ApprovalScope::WorkspaceSession => {
                let signatures = remember.map_or_else(
                    || vec![BLANKET_ESCALATION_SIGNATURE.to_string()],
                    |remember| remember.signatures,
                );
                self.remember_in_session(signatures);
                EscalationOutcome::Approved
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
                EscalationOutcome::Approved
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
        escalation_remember_payload(command)
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
        if self.project.matches_escalation(&remember.argvs) {
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

/// What the sandbox actually refused, for the approval prompt. "The sandbox
/// blocked this" alone is the one thing the reader already knows; the class and
/// the line that proves it are what decide whether the answer is a writable
/// root, the network switch, or a different command.
fn describe_denial(denial: Option<&crate::sandbox::SandboxDenial>) -> String {
    let Some(denial) = denial else {
        return "the OS sandbox blocked this — run it without the sandbox?".to_string();
    };
    format!(
        "the OS sandbox blocked this ({}) — run it without the sandbox?\n{}",
        denial.kind.label(),
        denial.evidence
    )
}

/// The session-cache key standing for "every sandbox escalation in this
/// workspace session". Never written to the durable store: a script this
/// coarse cannot be spelled as a rule, and consent that broad should not
/// outlive the sitting it was given in.
const BLANKET_ESCALATION_SIGNATURE: &str = "sandbox_escalate:*";

/// What the ordinary gate's prompt shows under the remembering choices for a
/// script that can only be keyed on its own text. There is no rule to echo —
/// this memory is the session cache alone — and reprinting the command already
/// on screen two lines up would say nothing, so the echo states the granularity
/// instead. Unlike the escalation blanket below it covers exactly one command
/// text: the gate is the first door, so "every opaque script" would just be
/// bypass mode by another name.
const VERBATIM_REMEMBER_RULE: &str = "only this exact command text";

/// What the prompt shows under the remembering choices when the script is too
/// opaque to key on — the scope really is every escalation, and saying so is
/// the difference between an informed yes and a misread one.
const BLANKET_ESCALATION_RULE: &str = "every sandbox escalation this session";

/// Escalation consent is remembered at the same granularity the ordinary gate
/// uses for bash — a two-word command prefix per segment — but under its own
/// `sandbox_escalate(...)` rule, because "may run" and "may run uncontained"
/// are different permissions. `None` = opaque script or a token that would
/// corrupt a rule string, i.e. not remember-able.
fn escalation_remember_payload(command: &str) -> Option<Remember> {
    let BashAnalysis::Commands(cmds) = analyze_bash(command) else {
        return None;
    };
    bash_prefix_memory(&cmds, "sandbox_escalate", ReadOnlySegments::Remember)
}

fn user_denial(name: &str) -> String {
    format!(
        "The user declined this {name} call. Do not retry the same call; take a different approach, or ask the user how to proceed."
    )
}

fn allow_rules_match(allow: &[Rule], name: &str, call: &CallFacts) -> bool {
    match (&call.shell, &call.path) {
        (Some(ShellFacts::Bash(BashAnalysis::Commands(commands))), _) => commands
            .iter()
            .all(|argv| argv_is_readonly(argv) || allow.iter().any(|rule| rule.matches_argv(argv))),
        // Nothing vouches for a script the parser could not read, whole-tool
        // `bash` included: that rule was written without anyone having seen
        // this text, and no layer below can vet its content.
        (Some(ShellFacts::Bash(BashAnalysis::Opaque)), _) => false,
        (Some(ShellFacts::PowerShellOpaque), _) => allow.iter().any(|rule| rule.matches_tool(name)),
        (None, Some(path)) => allow.iter().any(|rule| rule.matches_path(name, path)),
        (None, None) => allow.iter().any(|rule| rule.matches_tool(name)),
    }
}

/// Deny/ask matching is the aggressive direction: bash argv are matched
/// after wrapper stripping so `sudo rm` / `env FOO=1 rm` cannot dodge a
/// `bash(rm *)` rule, and any single matching segment hits.
fn rules_hit(rules: &[Rule], name: &str, call: &CallFacts) -> bool {
    if rules.iter().any(|r| r.matches_tool(name)) {
        return true;
    }
    match (&call.shell, &call.path) {
        (Some(ShellFacts::Bash(BashAnalysis::Commands(cmds))), _) => cmds.iter().any(|argv| {
            let stripped = strip_wrappers(argv);
            rules
                .iter()
                .any(|r| r.matches_argv(&stripped) || r.matches_argv(argv))
        }),
        // Opaque scripts cannot be inspected, so content-level Bash/PowerShell
        // rules never decide here. Whole-tool rules were handled above.
        (Some(ShellFacts::Bash(BashAnalysis::Opaque)), _) => false,
        (Some(ShellFacts::PowerShellOpaque), _) => false,
        (None, Some(path)) => rules.iter().any(|r| r.matches_path(name, path)),
        (None, None) => false,
    }
}

struct Hazard {
    tag: &'static str,
}

enum ShellFacts {
    Bash(BashAnalysis),
    PowerShellOpaque,
}

/// The decomposition one tool call is judged on, gathered once per call. Two
/// built-ins answer [`Builtin::readonly`] from the call rather than the name,
/// so exactly those two facts are visible to the crate; the rest belong to this
/// module's gate.
pub(crate) struct CallFacts {
    shell: Option<ShellFacts>,
    path: Option<PathFacts>,
    path_key: Option<&'static str>,
    sensitive_read: bool,
    powershell_sensitive: bool,
    entering_existing_worktree: bool,
    pub(crate) removing_worktree: bool,
    /// This bash call asked to run outside the OS sandbox (`disable_sandbox`).
    /// Kept as a fact of the call because bypass mode reads it: giving up
    /// containment is the model's own decision, not a property of the command.
    escapes_sandbox: bool,
    /// Present only when the bash analysis gave up: with no argv to key on, the
    /// script's own text is what rules and session memories are written about.
    opaque_script: Option<OpaqueScript>,
}

/// An unparseable bash script kept whole. `no_sandbox` is the call's
/// `disable_sandbox` flag, carried alongside the text because "may run" and
/// "may run outside the sandbox" are different consents about the same words.
struct OpaqueScript {
    command: String,
    no_sandbox: bool,
}

struct PathFacts {
    /// Canonical existing ancestor plus any not-yet-created suffix.
    normalized: PathBuf,
    /// Resolved path relative to the canonical working directory.
    relative: Option<PathBuf>,
    /// Original lexical spelling, retained so explicit path rules keep matching
    /// aliases in addition to the resolved target.
    original: PathBuf,
    original_relative: Option<PathBuf>,
    inside_cwd: bool,
    sensitive: bool,
}

impl CallFacts {
    fn gather(name: &str, input: &Value, cwd: &Path, resolved_path: Option<&Path>) -> Self {
        let shell_command = matches!(name, "bash" | "powershell")
            .then(|| input["command"].as_str())
            .flatten();
        let shell = match (name, shell_command) {
            ("bash", Some(command)) => Some(ShellFacts::Bash(analyze_bash(command))),
            ("powershell", Some(_)) => Some(ShellFacts::PowerShellOpaque),
            _ => None,
        };
        let path_key = match name {
            "write_file" | "edit_file" | "read_file" => Some("path"),
            "notebook_edit" => Some("notebook_path"),
            _ => None,
        };
        let path = path_key
            .and_then(|key| input[key].as_str())
            .map(|raw| PathFacts::gather_with_resolved(Path::new(raw), cwd, resolved_path));
        let sensitive_read = (name == "read_file"
            && path.as_ref().is_some_and(|path| path.sensitive))
            || (name == "bash"
                && shell_command
                    .zip(shell.as_ref())
                    .is_some_and(|(command, facts)| {
                        let ShellFacts::Bash(analysis) = facts else {
                            return false;
                        };
                        bash_reads_sensitive_path(command, analysis, cwd)
                    }));
        let powershell_sensitive =
            name == "powershell" && shell_command.is_some_and(powershell_mentions_sensitive_path);
        let escapes_sandbox = name == "bash" && input["disable_sandbox"].as_bool().unwrap_or(false);
        let opaque_script = match (&shell, shell_command) {
            (Some(ShellFacts::Bash(BashAnalysis::Opaque)), Some(command)) => {
                let command = command.trim();
                (!command.is_empty()).then(|| OpaqueScript {
                    command: command.to_string(),
                    no_sandbox: escapes_sandbox,
                })
            }
            _ => None,
        };
        CallFacts {
            shell,
            path,
            path_key,
            sensitive_read,
            powershell_sensitive,
            entering_existing_worktree: name == "enter_worktree"
                && input.get("path").is_some_and(Value::is_string),
            removing_worktree: name == "exit_worktree"
                && input["action"].as_str() == Some("remove"),
            escapes_sandbox,
            opaque_script,
        }
    }

    fn hazard(&self, name: &str) -> Option<Hazard> {
        if self.entering_existing_worktree {
            return Some(Hazard {
                tag: "existing worktree",
            });
        }
        if self.removing_worktree {
            return Some(Hazard { tag: "destructive" });
        }
        if self.powershell_sensitive {
            return Some(Hazard {
                tag: "sensitive PowerShell path",
            });
        }
        if let Some(ShellFacts::Bash(BashAnalysis::Commands(cmds))) = &self.shell
            && cmds
                .iter()
                .any(|argv| argv_is_dangerous(argv) || argv_is_dangerous(&strip_wrappers(argv)))
        {
            return Some(Hazard { tag: "destructive" });
        }
        if matches!(name, "write_file" | "edit_file" | "notebook_edit")
            && self.path.as_ref().is_some_and(|p| p.sensitive)
        {
            return Some(Hazard {
                tag: "sensitive path",
            });
        }
        None
    }

    /// Whether this shell call decomposes into nothing but known read-only
    /// commands. Safe-to-run and safe-to-parallelize are two verdicts over this
    /// one decomposition, so `Builtin::concurrency_safe` asks the same question
    /// of its own copy of the analysis.
    pub(crate) fn bash_is_readonly(&self) -> bool {
        matches!(&self.shell, Some(ShellFacts::Bash(BashAnalysis::Commands(cmds)))
            if !cmds.is_empty() && cmds.iter().all(|c| argv_is_readonly(c)))
    }

    /// Read-only in the permission sense: nothing on the user's system to sign
    /// off on. Built-ins answer for themselves ([`Builtin::readonly`]) — the
    /// verdict is one of the facts a built-in owns, not a second table here.
    fn is_readonly(&self, name: &str) -> bool {
        if let Some(builtin) = Builtin::from_name(name) {
            return builtin.readonly(self);
        }
        // Listing only discloses the catalog a configured MCP server
        // advertises. Reading a model-selected URI still requires the normal
        // external-tool approval; read-only here would bypass it. Every other
        // source tool is judged not read-only: its source's own claim governs
        // batching, never whether the user is asked.
        name == "list_mcp_resources"
    }
}

fn bash_reads_sensitive_path(command: &str, analysis: &BashAnalysis, cwd: &Path) -> bool {
    if raw_mentions_sensitive_path(command) {
        return true;
    }
    let BashAnalysis::Commands(commands) = analysis else {
        return false;
    };
    commands.iter().any(|argv| {
        argv.iter().skip(1).any(|arg| {
            if arg.starts_with('-') && !arg.contains('/') {
                return false;
            }
            let path = expand_shell_path(arg, cwd);
            PathFacts::gather(&path, cwd).sensitive
        }) || recursive_search_covers_sensitive_path(argv, cwd)
    })
}

/// The one exemption to `.kloop` being sensitive: `.kloop/worktrees/<name>` holds
/// checkouts the agent is *supposed* to edit wholesale. Only this exact segment
/// pair is structural — a nested `.kloop` inside the worktree, and every other
/// child of `.kloop`, stays sensitive.
const WORKTREE_SEGMENT: &str = "worktrees";

/// Whether a raw command string may mention `.kloop/worktrees/`. The string
/// layer is the only protection left when a command is too opaque to parse into
/// argv, so it refuses to grant the exemption to anything carrying a `..`: a
/// spelling like `.kloop/worktrees/../sessions` must not walk out through the
/// hole. Normalized paths are exempted structurally instead, in
/// [`path_is_sensitive`].
fn worktree_mention_is_structural(command: &str) -> bool {
    !command.contains("..")
}

/// The second structural exemption to `.kloop` being sensitive: a spilled tool
/// result (`…/offload/off-0001.txt`) or a background shell's output
/// (`…/offload/bg-3.out`). These are the model's *own* output — it was handed a
/// head/tail preview and this exact path — so re-reading one discloses nothing a
/// tool would not have returned anyway, while refusing turns every oversized
/// result into a dead end. Nothing else in the store qualifies: the provider
/// credential is in `config.toml` and every project's transcript in `sessions/`,
/// and both stay sensitive.
fn is_spill_file_name(name: &str) -> bool {
    let numbered = |rest: &str, ext: &str| {
        rest.strip_suffix(ext)
            .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
    };
    name.strip_prefix("off-")
        .is_some_and(|rest| numbered(rest, ".txt"))
        || name
            .strip_prefix("bg-")
            .is_some_and(|rest| numbered(rest, ".out"))
}

/// Whether one shell token is exactly such a spill path. Token-wise rather than
/// a substring scan of the whole command, so masking one path never unmasks a
/// different `.kloop` mention sitting beside it. A `..` forfeits the exemption,
/// as it does for worktrees.
fn is_spill_path_token(token: &str) -> bool {
    if token.contains("..") {
        return false;
    }
    let Some((parent, name)) = token.rsplit_once('/') else {
        return false;
    };
    parent.ends_with("/offload") && is_spill_file_name(name)
}

/// Neutralize the `.kloop` inside spill tokens only, leaving every other token
/// for the needle scan. Separators are preserved so the rebuilt string keeps the
/// original shape.
fn mask_spill_tokens(command: &str) -> String {
    const SEPARATORS: &[char] = &[
        ' ', '\t', '\n', '\r', '\'', '"', '`', '(', ')', ';', '|', '<', '>', '=', ',', '&', '{',
        '}',
    ];
    let mut out = String::with_capacity(command.len());
    let mut token = String::new();
    let flush = |token: &mut String, out: &mut String| {
        if is_spill_path_token(token) {
            out.push_str(&token.replace("/.kloop/", "/kloop/"));
        } else {
            out.push_str(token);
        }
        token.clear();
    };
    for ch in command.chars() {
        if SEPARATORS.contains(&ch) {
            flush(&mut token, &mut out);
            out.push(ch);
        } else {
            token.push(ch);
        }
    }
    flush(&mut token, &mut out);
    out
}

fn raw_mentions_sensitive_path(command: &str) -> bool {
    let folded = fs_fold(command);
    let masked = mask_spill_tokens(folded.as_ref());
    let command = masked.as_str();
    if command.contains("/.kloop/worktrees/") && worktree_mention_is_structural(command) {
        // Re-test with the managed worktree prefix neutralized, so the rest of
        // the command is still screened for real state paths.
        let masked = command.replace("/.kloop/worktrees/", "/worktrees/");
        return mentions_sensitive_needle(&masked);
    }
    mentions_sensitive_needle(command)
}

fn mentions_sensitive_needle(command: &str) -> bool {
    [
        "~/.kloop", "/.kloop/", "~/.ssh", "/.ssh/", "~/.gnupg", "/.gnupg/", "~/.aws", "/.aws/",
    ]
    .iter()
    .any(|needle| command.contains(needle))
}

fn powershell_mentions_sensitive_path(command: &str) -> bool {
    let normalized = command.to_lowercase().replace('\\', "/");
    let normalized = if normalized.contains("/.kloop/worktrees/")
        && worktree_mention_is_structural(&normalized)
    {
        normalized.replace("/.kloop/worktrees/", "/worktrees/")
    } else {
        normalized
    };
    let normalized = normalized.as_str();
    [".kloop", ".ssh", ".gnupg", ".aws", ".env"]
        .into_iter()
        .any(|needle| {
            normalized.match_indices(needle).any(|(index, _)| {
                let before = normalized[..index].chars().next_back();
                let after = normalized[index + needle.len()..].chars().next();
                let boundary = |character: Option<char>| {
                    character.is_none_or(|character| {
                        character == '/'
                            || character.is_whitespace()
                            || matches!(
                                character,
                                '\'' | '"'
                                    | '`'
                                    | '('
                                    | ')'
                                    | '['
                                    | ']'
                                    | '{'
                                    | '}'
                                    | '='
                                    | ':'
                                    | ';'
                                    | ','
                            )
                    })
                };
                boundary(before) && (needle == ".env" || boundary(after))
            })
        })
}

fn expand_shell_path(raw: &str, cwd: &Path) -> PathBuf {
    if raw == "~" {
        return std::env::home_dir().unwrap_or_else(|| cwd.to_path_buf());
    }
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = std::env::home_dir()
    {
        return home.join(rest);
    }
    lexical_normalize(cwd, Path::new(raw))
}

fn recursive_search_covers_sensitive_path(argv: &[String], cwd: &Path) -> bool {
    let Some(command) = argv.first().and_then(|raw| raw.rsplit('/').next()) else {
        return false;
    };
    let recursive = match command {
        // ripgrep is recursive, but only exposes hidden credential directories
        // when --hidden / -u is requested.
        "rg" => argv[1..].iter().any(|arg| {
            arg == "--hidden"
                || (arg.starts_with('-')
                    && !arg.starts_with("--")
                    && arg.chars().skip(1).any(|ch| ch == 'u'))
        }),
        "grep" => argv[1..]
            .iter()
            .any(|arg| matches!(arg.as_str(), "-r" | "-R" | "--recursive")),
        _ => false,
    };
    if !recursive {
        return false;
    }
    let roots: Vec<PathBuf> = argv[1..]
        .iter()
        .filter(|arg| !arg.starts_with('-'))
        .map(|arg| expand_shell_path(arg, cwd))
        .filter(|path| path.is_dir())
        .collect();
    let roots = if roots.is_empty() {
        vec![cwd.to_path_buf()]
    } else {
        roots
    };
    roots.iter().any(|root| {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        let local_sensitive = [".kloop", ".ssh", ".gnupg", ".aws"]
            .iter()
            .any(|name| root.join(name).exists());
        let global_config_is_below = std::env::home_dir()
            .map(|home| home.join(".kloop/config.toml").starts_with(&root))
            .unwrap_or(false);
        local_sensitive || global_config_is_below
    })
}

impl PathFacts {
    fn gather(raw: &Path, cwd: &Path) -> Self {
        Self::gather_with_resolved(raw, cwd, None)
    }

    fn gather_with_resolved(raw: &Path, cwd: &Path, resolved_path: Option<&Path>) -> Self {
        let original = lexical_normalize(cwd, raw);
        let canonical_cwd =
            std::fs::canonicalize(cwd).unwrap_or_else(|_| lexical_normalize(Path::new("/"), cwd));
        let normalized = resolved_path.map(Path::to_path_buf).unwrap_or_else(|| {
            canonicalize_nearest_existing(&original).unwrap_or_else(|| original.clone())
        });
        let relative = normalized
            .strip_prefix(&canonical_cwd)
            .ok()
            .map(Path::to_path_buf);
        let original_relative = original.strip_prefix(cwd).ok().map(Path::to_path_buf);
        let inside_cwd = relative.is_some();
        let sensitive = path_is_sensitive(&original) || path_is_sensitive(&normalized);
        PathFacts {
            normalized,
            relative,
            original,
            original_relative,
            inside_cwd,
            sensitive,
        }
    }
}

/// Canonicalize the nearest existing ancestor and reattach a missing suffix.
/// This resolves parent symlinks even when the final write target does not yet
/// exist, so permissions judge the same path the filesystem executor will use.
fn canonicalize_nearest_existing(path: &Path) -> Option<PathBuf> {
    let mut ancestor = path;
    let mut missing = Vec::new();
    loop {
        match std::fs::canonicalize(ancestor) {
            Ok(mut resolved) => {
                for component in missing.iter().rev() {
                    resolved.push(component);
                }
                return Some(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(ancestor.file_name()?.to_os_string());
                ancestor = ancestor.parent()?;
            }
            Err(_) => return None,
        }
    }
}

/// Resolve `.`/`..` lexically (no filesystem access — the target of a write
/// may not exist yet), anchoring relative paths at `cwd`.
fn lexical_normalize(cwd: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let mut out = PathBuf::new();
    for comp in joined.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Case-folding filesystems (macOS/Windows default): `.GIT` and `.git` are
/// the same inode, so a cased alias would slip a write past a case-sensitive
/// sensitive-path or deny check and land on the real `.git/hooks`. Fold case
/// there; compare verbatim where distinct names are distinct files.
const FS_FOLDS_CASE: bool = cfg!(any(target_os = "macos", target_os = "windows"));

fn fs_fold(name: &str) -> std::borrow::Cow<'_, str> {
    if FS_FOLDS_CASE {
        std::borrow::Cow::Owned(name.to_ascii_lowercase())
    } else {
        std::borrow::Cow::Borrowed(name)
    }
}

/// Whether a normalized path ends in `offload/<spill file>` — the structural
/// form of [`is_spill_file_name`], for callers that already have components.
fn path_tail_is_spill(normalized: &Path) -> bool {
    let Some(name) = normalized.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if !is_spill_file_name(fs_fold(name).as_ref()) {
        return false;
    }
    normalized
        .parent()
        .and_then(Path::file_name)
        .and_then(|parent| parent.to_str())
        .is_some_and(|parent| fs_fold(parent) == "offload")
}

/// Paths where a write is privilege escalation, not editing: VCS internals
/// (hooks run code), kloop's own state, key material, shell/git rc files
/// (cc's `checkPathSafetyForAutoEdit` list, trimmed to this project's
/// blast radius).
fn path_is_sensitive(normalized: &Path) -> bool {
    const SENSITIVE_DIRS: &[&str] = &[".git", ".kloop", ".ssh", ".gnupg", ".aws"];
    const SENSITIVE_FILES: &[&str] = &[
        ".bashrc",
        ".zshrc",
        ".zshenv",
        ".zprofile",
        ".bash_profile",
        ".profile",
        ".gitconfig",
        ".envrc",
    ];
    // Callers normalize first, but the exemption below must not depend on that:
    // `.kloop/worktrees/../sessions` is only a worktree path if you stop reading
    // at the second segment. A `..` anywhere forfeits the exemption entirely.
    let traversal = normalized
        .components()
        .any(|comp| matches!(comp, Component::ParentDir));
    let mut components = normalized.components().peekable();
    while let Some(comp) = components.next() {
        let Component::Normal(name) = comp else {
            continue;
        };
        let Some(name) = name.to_str() else {
            return true; // non-UTF8 path: refuse to vouch
        };
        let folded = fs_fold(name);
        let name = folded.as_ref();
        let is_last = components.peek().is_none();
        // `.kloop/worktrees/...` is a checkout, not kloop's state. Skipping the
        // `.kloop` segment (rather than returning false) keeps the scan running
        // over the rest, so a nested `.kloop` inside the worktree still trips.
        if name == ".kloop"
            && !traversal
            && components.peek().is_some_and(|next| {
                matches!(next, Component::Normal(next)
                    if fs_fold(&next.to_string_lossy()) == WORKTREE_SEGMENT)
            })
        {
            continue;
        }
        // The spill exemption reads the *tail* rather than the next segment,
        // because the store partitions by project between the two
        // (`.kloop/projects/v1/<id>/offload/off-0001.txt`). Skipping `.kloop`
        // rather than returning keeps the scan running, so a `.ssh` or a nested
        // `.kloop` elsewhere in the path still trips.
        if name == ".kloop" && !traversal && path_tail_is_spill(normalized) {
            continue;
        }
        if SENSITIVE_DIRS.contains(&name) && !is_last {
            return true;
        }
        if is_last
            && (SENSITIVE_DIRS.contains(&name)
                || SENSITIVE_FILES.contains(&name)
                || name.starts_with(".env"))
        {
            return true;
        }
    }
    false
}

struct Remember {
    /// Suggested persistent rules (`bash(git commit *)`, `write_file(src/**)`).
    /// Empty when nothing durable can be written down.
    rules: Vec<String>,
    /// What the prompt echoes under the remembering choices. Usually the rules
    /// themselves; a verbatim script rule is the command text the panel already
    /// shows two lines up, so it states the granularity instead of repeating it.
    echo: Vec<String>,
    /// Session-cache keys, one per non-covered segment / path scope.
    signatures: Vec<String>,
    /// The parsed argv of each remembered segment. Only the sandbox-escalation
    /// path reads this — it has to test the command against durable rules
    /// itself, where the ordinary gate has already done that upstream. Empty
    /// for the ordinary gate's payloads.
    argvs: Vec<Vec<String>>,
}

impl Remember {
    /// Whether this memory can be written to the project store at all — the
    /// difference between offering the durable scope and offering only the
    /// session one.
    fn persistable(&self) -> bool {
        !self.rules.is_empty()
    }

    /// The ordinary shape: what the prompt echoes is the rule itself.
    fn echoing_rules(rules: Vec<String>, signatures: Vec<String>, argvs: Vec<Vec<String>>) -> Self {
        Self {
            echo: rules.clone(),
            rules,
            signatures,
            argvs,
        }
    }
}

/// Whether a read-only segment needs remembering. The ordinary gate skips them
/// — a read-only command was never going to be asked about, so there is nothing
/// to remember. The escalation does not skip: a read-only command still had to
/// be *denied inside the sandbox* to reach that prompt, so it is part of what
/// the yes was given for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReadOnlySegments {
    Skip,
    Remember,
}

/// One rule per distinct two-word command prefix, in `tool`'s own vocabulary:
/// `bash(git commit *)` for the ordinary gate, `sandbox_escalate(git commit *)`
/// for the escalation. The two consents are different permissions written at
/// the same granularity, so only the vocabulary and [`ReadOnlySegments`] differ
/// — everything else used to be a second copy of these twenty lines.
///
/// `None` = a token that would corrupt a rule string (a paren, a comma, or
/// embedded whitespace), so nothing can be written down at all. An empty argv
/// is rejected for the same reason: the parser never produces one, but a rule
/// reading `bash( *)` would be worse than no memory.
fn bash_prefix_memory(
    cmds: &[Vec<String>],
    tool: &str,
    segments: ReadOnlySegments,
) -> Option<Remember> {
    let mut rules = Vec::new();
    let mut signatures = Vec::new();
    let mut argvs = Vec::new();
    for argv in cmds {
        if segments == ReadOnlySegments::Skip && argv_is_readonly(argv) {
            continue;
        }
        let prefix: Vec<&str> = argv.iter().take(2).map(String::as_str).collect();
        if prefix.is_empty()
            || prefix
                .iter()
                .any(|t| t.contains(['(', ')', ',']) || t.chars().any(char::is_whitespace))
        {
            return None;
        }
        let head = prefix.join(" ");
        let rule = format!("{tool}({head} *)");
        if !rules.contains(&rule) {
            rules.push(rule);
            signatures.push(format!("{tool}:{head}"));
        }
        // Only a payload that remembered *every* segment may later stand in for
        // the whole command; the skipping form hands back none on purpose.
        if segments == ReadOnlySegments::Remember {
            argvs.push(argv.clone());
        }
    }
    if rules.is_empty() {
        return None;
    }
    Some(Remember::echoing_rules(rules, signatures, argvs))
}

/// cc's "always allow" granularity: bash remembers a two-word command
/// prefix per segment (`git push` never covers `git commit`); file writes
/// remember the parent directory; an unparseable script is remembered
/// verbatim, session-only. `None` = not remember-able (a resource URI, or
/// tokens that would corrupt a rule string).
fn remember_payload(name: &str, call: &CallFacts) -> Option<Remember> {
    // Resource URIs are model-selected dynamic locators. Remembering the generic
    // tool name would let approval for one server/URI authorize every future
    // resource read, so only an explicit configured allow rule may do that.
    if matches!(name, "read_mcp_resource" | "read_mcp_resource_dir") {
        return None;
    }
    match (&call.shell, &call.path) {
        (Some(ShellFacts::Bash(BashAnalysis::Commands(cmds))), _) => {
            bash_prefix_memory(cmds, "bash", ReadOnlySegments::Skip)
        }
        // PowerShell stays unremember-able: bash at least gets parsed and only
        // then gives up, while PowerShell has no analysis at all — so "every
        // call asks" is the only safety net it has left.
        (Some(ShellFacts::PowerShellOpaque), _) => None,
        // No prefix to key on, so the script is keyed by its own text: only
        // this exact command, re-offered in this same session, skips the
        // question. Session-only, and `rules` stays empty so the durable scope
        // is never offered (plan 145): the key is the whole command, and a real
        // command carries this run's own test filter, package list and `tail
        // -3`, so the next run of the same intent writes a different key — a
        // stored one is dead weight from the moment it lands (38 of them on the
        // user's disk, 0 hits between them).
        (Some(ShellFacts::Bash(BashAnalysis::Opaque)), _) => {
            let script = call.opaque_script.as_ref()?;
            // Escaping the sandbox is part of what the yes was given for, so it
            // is spelled into the session key: a contained run's approval must
            // never cover an uncontained one.
            let escape = if script.no_sandbox { "!no-sandbox" } else { "" };
            let command = &script.command;
            Some(Remember {
                rules: Vec::new(),
                echo: vec![VERBATIM_REMEMBER_RULE.to_string()],
                signatures: vec![format!("bash-script{escape}:{command}")],
                argvs: Vec::new(),
            })
        }
        (None, Some(path)) => {
            let scope = path
                .relative
                .as_deref()
                .unwrap_or(&path.normalized)
                .parent()
                .map(|d| d.to_string_lossy().into_owned())
                .unwrap_or_default();
            let pattern = if scope.is_empty() {
                "*".to_string()
            } else {
                format!("{scope}/**")
            };
            Some(Remember::echoing_rules(
                vec![format!("{name}({pattern})")],
                vec![format!("{name}:{scope}")],
                Vec::new(),
            ))
        }
        (None, None) => Some(Remember::echoing_rules(
            vec![name.to_string()],
            vec![name.to_string()],
            Vec::new(),
        )),
    }
}

/// The one thing a gated call acts on: the command, the path, or — for a tool
/// with neither — its whole input. Shared by the flat `description` and the
/// multi-line parts so the two can never name different targets for the same
/// approval.
fn call_detail(name: &str, input: &Value) -> String {
    match name {
        "bash" | "powershell" => input["command"].as_str().unwrap_or("?").to_string(),
        "write_file" | "edit_file" | "read_file" => {
            input["path"].as_str().unwrap_or("?").to_string()
        }
        "notebook_edit" => input["notebook_path"].as_str().unwrap_or("?").to_string(),
        _ => input.to_string(),
    }
}

fn describe(name: &str, input: &Value, depth: u8, hazard_tag: Option<&str>) -> String {
    let agent = if depth > 0 { "[sub-agent] " } else { "" };
    let hazard = hazard_tag.map_or(String::new(), |t| format!("[{t}] "));
    // The escape hatch from the OS sandbox is worth flagging to the human:
    // this run gets full filesystem and network access if approved.
    let no_sandbox = if name == "bash" && input["disable_sandbox"].as_bool().unwrap_or(false) {
        "[no sandbox] "
    } else {
        ""
    };
    let detail = clip(&call_detail(name, input));
    let shell_class = if name == "powershell" {
        "[unclassified PowerShell] "
    } else {
        ""
    };
    format!("{agent}{hazard}{no_sandbox}{shell_class}{name}: {detail}")
}

/// [`describe`] pulled apart for a multi-line frontend: the kind of action, the
/// one thing it acts on, and why it is being asked. Same inputs, same facts —
/// only the shape differs, so the two can never disagree about what is gated.
fn describe_parts(
    name: &str,
    input: &Value,
    depth: u8,
    hazard_tag: Option<&str>,
) -> (String, String, Option<String>) {
    let mut notices: Vec<String> = Vec::new();
    if depth > 0 {
        notices.push("requested by a sub-agent".to_string());
    }
    if let Some(tag) = hazard_tag {
        notices.push(tag.to_string());
    }
    if name == "bash" && input["disable_sandbox"].as_bool().unwrap_or(false) {
        notices.push("no OS sandbox — full filesystem and network access".to_string());
    }
    if name == "powershell" {
        notices.push("unclassified PowerShell".to_string());
    }
    let detail = call_detail(name, input);
    (
        tool_title(name).to_string(),
        clip(&detail),
        (!notices.is_empty()).then(|| notices.join(" · ")),
    )
}

/// A human name for the action a tool performs, for the panel's header row.
/// Built-ins all have one ([`Builtin::title`]); so do the two web tools, which
/// kloop names itself even though they arrive through the source seam. An MCP
/// tool keeps its own name — that is what the user configured and recognizes.
fn tool_title(name: &str) -> &str {
    if let Some(builtin) = Builtin::from_name(name) {
        return builtin.title();
    }
    match name {
        crate::tools::web::WEB_FETCH => "Fetch a URL",
        crate::tools::web::WEB_SEARCH => "Web search",
        other => other,
    }
}

fn sub_agent_notice(depth: u8) -> Option<String> {
    (depth > 0).then(|| "requested by a sub-agent".to_string())
}

fn join_notices(first: Option<String>, second: &str) -> String {
    match first {
        Some(first) => format!("{first} · {second}"),
        None => second.to_string(),
    }
}

/// Prompt text is display, not a payload: cap it so one enormous command or
/// path cannot dominate the panel.
fn clip(text: &str) -> String {
    text.chars().take(200).collect()
}

/// The escalation prompt: single-line (the flat `description` form) with
/// a tag that says why it is being asked.
fn describe_escalation(command: &str, depth: u8) -> String {
    let agent = if depth > 0 { "[sub-agent] " } else { "" };
    let cmd: String = command.chars().take(200).collect();
    format!("{agent}[sandbox denied — run without sandbox?] bash: {cmd}")
}

/// The exit-plan-mode prompt heading; the plan text itself rides in the
/// popup's scrollable `preview`.
fn describe_plan_exit(depth: u8) -> String {
    let agent = if depth > 0 { "[sub-agent] " } else { "" };
    format!("{agent}Exit plan mode and start on this plan?")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

    struct ScriptedWriter {
        snapshot: Mutex<ProjectPolicySnapshot>,
        fail: bool,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl ScriptedWriter {
        fn succeeding() -> Arc<Self> {
            Arc::new(Self {
                snapshot: Mutex::new(ProjectPolicySnapshot::empty()),
                fail: false,
                calls: Mutex::new(Vec::new()),
            })
        }

        fn failing() -> Arc<Self> {
            Arc::new(Self {
                snapshot: Mutex::new(ProjectPolicySnapshot::empty()),
                fail: true,
                calls: Mutex::new(Vec::new()),
            })
        }
    }

    impl ProjectPermissionWriter for ScriptedWriter {
        fn append_allow(
            &self,
            _project_id: crate::project::ProjectId,
            additions: ProjectAllowRules,
        ) -> Pin<
            Box<
                dyn Future<
                        Output = std::result::Result<
                            ProjectPolicySnapshot,
                            ProjectPolicyStoreError,
                        >,
                    > + Send
                    + '_,
            >,
        > {
            self.calls.lock().unwrap().push(additions.raw().to_vec());
            if self.fail {
                return Box::pin(async { Err(ProjectPolicyStoreError::PersistenceFailed) });
            }
            let mut snapshot = self.snapshot.lock().unwrap();
            let mut allow = snapshot.allow.raw().to_vec();
            for rule in additions.raw() {
                if !allow.contains(rule) {
                    allow.push(rule.clone());
                }
            }
            snapshot.revision += 1;
            snapshot.allow = ProjectAllowRules::parse(&allow).unwrap();
            let published = snapshot.clone();
            Box::pin(async move { Ok(published) })
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

    fn test_cwd() -> PathBuf {
        std::env::current_dir().expect("test process has a current directory")
    }

    /// The two bash memories are the same granularity in two vocabularies, and
    /// they disagree about exactly one thing: whether a read-only segment is
    /// part of what was approved. Writing them through one helper is only
    /// correct while that disagreement survives — so it is asserted from both
    /// ends of the same command.
    #[test]
    fn the_two_bash_memories_differ_only_in_vocabulary_and_read_only_segments() {
        let command = "ls -la && git commit -m x";
        let call = CallFacts::gather("bash", &json!({"command": command}), Path::new("/"), None);

        // The ordinary gate never had to ask about `ls`, so it remembers only
        // the segment that would have been asked about.
        let ordinary = remember_payload("bash", &call).expect("a parsed command is remember-able");
        assert_eq!(ordinary.rules, vec!["bash(git commit *)"]);
        assert_eq!(ordinary.signatures, vec!["bash:git commit"]);
        assert!(
            ordinary.argvs.is_empty(),
            "a payload that skipped a segment must not stand in for the whole command"
        );

        // The sandbox denied the whole line, `ls` included, so the escalation
        // remembers both — under its own rule name.
        let escalation = escalation_remember_payload(command).expect("same command, other door");
        assert_eq!(
            escalation.rules,
            vec![
                "sandbox_escalate(ls -la *)",
                "sandbox_escalate(git commit *)"
            ]
        );
        assert_eq!(
            escalation.signatures,
            vec!["sandbox_escalate:ls -la", "sandbox_escalate:git commit"]
        );
        assert_eq!(escalation.argvs.len(), 2);

        // Read-only throughout: nothing the ordinary gate could write down,
        // while the escalation still has two denied commands to consent to.
        let read_only = CallFacts::gather(
            "bash",
            &json!({"command": "ls && pwd"}),
            Path::new("/"),
            None,
        );
        assert!(remember_payload("bash", &read_only).is_none());
        assert_eq!(
            escalation_remember_payload("ls && pwd").unwrap().rules,
            vec!["sandbox_escalate(ls *)", "sandbox_escalate(pwd *)"]
        );

        // A prefix token that would corrupt a rule string stops both. The
        // command still parses: one that does not takes the verbatim path
        // (plan 135) instead, which is a different memory altogether.
        let corrupt = r#"git "commit x" -m y"#;
        let corrupt_call =
            CallFacts::gather("bash", &json!({"command": corrupt}), Path::new("/"), None);
        assert!(matches!(
            corrupt_call.shell,
            Some(ShellFacts::Bash(BashAnalysis::Commands(_)))
        ));
        assert!(remember_payload("bash", &corrupt_call).is_none());
        assert!(escalation_remember_payload(corrupt).is_none());
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

    // ── layer 5: read-only self-verdict ────────────────────────────────

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
        for task_tool in [
            "task_create",
            "task_get",
            "task_update",
            "task_list",
            "task_clear",
        ] {
            assert!(ok(&p, task_tool, json!({})).await);
        }
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
        let asked_in_bypass = gate(
            Mode::Bypass,
            rules(&[], &[], &["read_mcp_resource"]),
            approver.clone(),
        );
        assert!(!ok(&asked_in_bypass, "read_mcp_resource", read.clone()).await);
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

    // ── layer 2: safety checks are bypass-immune ───────────────────────

    #[tokio::test]
    async fn destructive_commands_ask_even_in_bypass_and_over_allowlist() {
        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
        let p = gate(
            Mode::Bypass,
            rules(&["bash(rm *)"], &[], &[]),
            approver.clone(),
        );
        assert!(ok(&p, "bash", bash("rm -rf build")).await);
        let asked = approver.asked();
        assert_eq!(
            asked.len(),
            1,
            "allow rule and bypass must not silence the check"
        );
        assert!(asked[0].description.contains("[destructive]"));
        assert_eq!(asked[0].approval_scopes, vec![ApprovalScope::Once]);
        assert_eq!(asked[0].remember_rules, None);

        // second time: deny (script exhausted) → the call is refused
        assert!(!ok(&p, "bash", bash("rm -rf build")).await);
    }

    /// The destructive list is short on purpose — "irreversible, and git is not
    /// the way back" — but everything on it has to reach this layer, which is
    /// the one bypass cannot waive. The negative half matters just as much:
    /// `git reset --hard` is how work gets recovered here, and turning it into
    /// a prompt would be the blocklist growing by vibe.
    #[tokio::test]
    async fn irreversible_commands_reach_the_bypass_immune_layer() {
        for command in [
            "dd if=/dev/zero of=/dev/sda",
            "mkfs.ext4 /dev/sdb1",
            "shred secret.txt",
            "sudo dd of=/dev/sda",
        ] {
            let approver = ScriptedApprover::new(vec![Decision::Deny]);
            let p = gate(Mode::Bypass, rules(&[], &[], &[]), approver.clone());
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

    /// An opaque bash script (here a redirect onto a file) can't be vetted by
    /// the deny or destructive-safety layers, so bypass mode must NOT auto-run
    /// it: it falls through to the user like every other opaque call. A
    /// parseable command in the same mode still auto-runs. Regression — a
    /// one-token redirect used to slip `rm -rf …` past the deny rule, the
    /// destructive check, AND the bypass short-circuit, running unprompted;
    /// since plan 144 the stream-only half of that shape hides nothing, so it
    /// is denied outright instead of merely reaching the user.
    #[tokio::test]
    async fn opaque_bash_is_not_auto_run_in_bypass() {
        let approver = ScriptedApprover::new(vec![Decision::Deny]);
        let p = gate(
            Mode::Bypass,
            rules(&[], &["bash(rm *)"], &[]),
            approver.clone(),
        );
        // A file target still means Opaque: it escapes deny + destructive, so
        // it must reach the user (here denied) rather than silently run.
        assert!(!ok(&p, "bash", bash("rm -rf build > cleanup.log")).await);
        assert_eq!(
            approver.ask_count(),
            1,
            "opaque bash must reach the user in bypass, not auto-run"
        );
        // Its stream-only twin is parsed, so the deny rule sees the `rm` itself
        // and nobody is asked at all — the stricter end of the same story.
        assert!(!ok(&p, "bash", bash("rm -rf build > /dev/null")).await);
        assert_eq!(
            approver.ask_count(),
            1,
            "a parsed `rm` is denied by the rule, without a question"
        );
        // A parseable, non-destructive command still auto-runs with no prompt.
        assert!(ok(&p, "bash", bash("ls -la")).await);
        assert_eq!(
            approver.ask_count(),
            1,
            "parseable bash still auto-runs in bypass"
        );
    }

    #[tokio::test]
    async fn sensitive_paths_ask_every_time_and_never_remember() {
        let approver = ScriptedApprover::new(vec![
            Decision::Allow(ApprovalScope::Once),
            Decision::Allow(ApprovalScope::Once),
        ]);
        let p = gate(
            Mode::AcceptEdits,
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
            format!("wc -c {store}/offload/off-0001.txt"),
            format!("python3 -c 'print(1)' < {store}/offload/off-0042.txt"),
            format!("cat {store}/offload/bg-3.out"),
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
                json!({"path": format!("{store}/offload/off-0001.txt")})
            )
            .await
        );

        for command in [
            // The credential and the transcripts are what the deny is for.
            "cat /home/u/.kloop/config.toml".to_string(),
            format!("cat {store}/sessions/20260101-000000.jsonl"),
            // Not a spill file, merely sitting next to one.
            format!("cat {store}/offload/notes.txt"),
            format!("cat {store}/offload/off-0001.txt.bak"),
            // Traversal forfeits the exemption, as it does for worktrees.
            format!("cat {store}/offload/../../../config.toml"),
            format!("cat {store}/offload/off-0001.txt/../../config.toml"),
            // Masking one token must not unmask another in the same command.
            format!("cat {store}/offload/off-0001.txt /home/u/.kloop/config.toml"),
            format!("cat {store}/offload/off-0001.txt; cat ~/.kloop/config.toml"),
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

    #[cfg(unix)]
    #[tokio::test]
    async fn sensitive_reads_follow_symlinks_to_the_canonical_target() {
        let root =
            std::env::temp_dir().join(format!("kloop-sensitive-read-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let secret_dir = root.join(".kloop");
        std::fs::create_dir_all(&secret_dir).unwrap();
        let secret = secret_dir.join("config.toml");
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
            Mode::AcceptEdits,
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
            Mode::AcceptEdits,
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

    // ── bypass: what it is actually trusting ───────────────────────────

    /// What lets bypass auto-run a bash call is that the call is still
    /// contained — not that the command happens to miss `argv_is_dangerous`,
    /// a blocklist that knows only `rm` and `sudo`. A call reaches this layer
    /// for three reasons and only one of them is the model's own decision.
    #[tokio::test]
    async fn bypass_stops_at_a_call_that_gave_up_containment() {
        let escaping = json!({"command": "make install", "disable_sandbox": true});

        // The model asked to leave the sandbox: bypass no longer answers for it.
        let approver = ScriptedApprover::new(vec![Decision::Deny]);
        let asked = gate(Mode::Bypass, rules(&[], &[], &[]), approver.clone());
        assert!(!ok(&asked, "bash", escaping.clone()).await);
        assert_eq!(approver.ask_count(), 1);
        let notice = approver.asked()[0]
            .notice
            .clone()
            .expect("an escaping call carries a notice");
        assert!(notice.contains("no OS sandbox"), "{notice}");

        // Same mode, same (sandbox-less) session: an ordinary command still
        // runs untouched. Refusing this one would retire the mode on every
        // host without a sandbox rather than close a hole.
        let approver = ScriptedApprover::new(vec![]);
        let running = gate(Mode::Bypass, rules(&[], &[], &[]), approver.clone());
        assert!(ok(&running, "bash", bash("make install")).await);

        // Escaping but read-only: the read-only self-verdict below still takes
        // it. An `ls` outside the sandbox is not worth interrupting anyone for.
        assert!(
            ok(
                &running,
                "bash",
                json!({"command": "ls -la", "disable_sandbox": true})
            )
            .await
        );

        // And a call the sandbox will contain never reaches this layer at all.
        assert!(
            running
                .check_call("bash", &bash("make install"), 0, true)
                .await
                .is_ok()
        );
        assert_eq!(approver.ask_count(), 0);

        // Outside bypass nothing changed: manual asks about this command
        // whether or not it escapes.
        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
        let manual = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        assert!(ok(&manual, "bash", escaping).await);
        assert_eq!(approver.ask_count(), 1);
    }

    // ── layer 3: ask rules ──────────────────────────────────────────────

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

    // ── layers 6/7: acceptEdits and allow rules ─────────────────────────

    #[tokio::test]
    async fn accept_edits_auto_allows_writes_inside_cwd_only() {
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(Mode::AcceptEdits, rules(&[], &[], &[]), approver.clone());
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
            "acceptEdits is files-only"
        );
        assert_eq!(approver.ask_count(), 3);
    }

    /// A `rebased` gate re-anchors acceptEdits: relative and in-tree writes for
    /// the NEW cwd (a worktree) auto-allow, while a path that was inside the
    /// OLD cwd now falls outside and asks — the sub-agent can't silently write
    /// the main tree just because its parent could.
    #[tokio::test]
    async fn rebased_reanchors_accept_edits_onto_the_new_cwd() {
        let approver = ScriptedApprover::new(vec![]);
        let parent = gate(Mode::AcceptEdits, rules(&[], &[], &[]), approver.clone());
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
                    "write_file(src/**)",
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
        assert!(ok(&p, "write_file", file("src/deep/mod.rs")).await);
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
            !ok(&p, "write_file", file("docs/x.md")).await,
            "glob scope holds"
        );
        assert!(
            !ok(&p, "bash", bash("cargo $(evil)")).await,
            "opaque never matches allow"
        );
        assert_eq!(approver.ask_count(), 4);
    }

    // ── layers 8/9: session cache and remember payloads ────────────────

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

    #[test]
    fn project_registry_refreshes_authoritative_store_snapshots() {
        let project_id = crate::project::WorkspaceIdentity::resolve(&test_cwd())
            .project_id()
            .unwrap()
            .clone();
        let writer: Arc<dyn ProjectPermissionWriter> = ScriptedWriter::succeeding();
        let registry = ProjectPolicyRegistry::default();
        let first = registry.get_or_insert(
            project_id.clone(),
            ProjectPolicySnapshot::empty(),
            Arc::clone(&writer),
        );
        let initial_epoch = first.capability_epoch();
        let revision_one = ProjectPolicySnapshot {
            revision: 1,
            allow: ProjectAllowRules::parse(&["bash(cargo *)".to_string()]).unwrap(),
        };
        let refreshed = registry.get_or_insert(
            project_id.clone(),
            revision_one.clone(),
            Arc::clone(&writer),
        );
        assert!(Arc::ptr_eq(&first, &refreshed));
        assert_eq!(first.snapshot(), revision_one);
        assert_eq!(first.capability_epoch(), initial_epoch + 1);

        let revision_two = ProjectPolicySnapshot {
            revision: 2,
            allow: ProjectAllowRules::parse(&["write_file(src/**)".to_string()]).unwrap(),
        };
        registry.get_or_insert(
            project_id.clone(),
            revision_two.clone(),
            Arc::clone(&writer),
        );
        let revision_two_epoch = first.capability_epoch();
        assert_eq!(revision_two_epoch, initial_epoch + 2);
        registry.get_or_insert(project_id.clone(), revision_one, Arc::clone(&writer));
        registry.get_or_insert(
            project_id.clone(),
            ProjectPolicySnapshot::empty(),
            Arc::clone(&writer),
        );
        assert_eq!(first.capability_epoch(), revision_two_epoch);
        assert_eq!(
            first.snapshot(),
            revision_two,
            "an older or missing-store load rolled policy back"
        );

        registry.invalidate(&project_id);
        assert_eq!(first.capability_epoch(), revision_two_epoch + 1);
        assert_eq!(
            first.snapshot(),
            ProjectPolicySnapshot::empty(),
            "a removed store did not revoke cached grants"
        );
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
        assert!(ok(&p, "write_file", file("src/a.rs")).await);
        assert!(
            ok(&p, "write_file", file("src/b.rs")).await,
            "same directory cached"
        );
        assert!(
            !ok(&p, "write_file", file("src/deep/c.rs")).await,
            "subdirectory asks"
        );
        assert_eq!(approver.ask_count(), 2);
        assert_eq!(
            approver.asked()[0].remember_rules,
            Some(vec!["write_file(src/**)".to_string()])
        );
    }

    // ── denial semantics & misc ─────────────────────────────────────────

    #[tokio::test]
    async fn user_denial_message_guides_the_model() {
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        let err = p.check("write_file", &file("x.txt"), 0).await.unwrap_err();
        assert!(err.contains("declined"), "{err}");
        assert!(err.contains("different approach"), "{err}");
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
        assert!(p.check("write_file", &file("x"), 0).await.is_err());
        assert!(
            p.check("bash", &bash("ls"), 0).await.is_ok(),
            "read-only still passes"
        );
    }

    #[tokio::test]
    async fn description_carries_depth_hazard_and_detail() {
        let approver = ScriptedApprover::new(vec![Decision::Deny, Decision::Deny]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        let _ = p.check("bash", &bash("rm -rf x"), 1).await;
        let _ = p.check("write_file", &file("a.txt"), 0).await;
        let asked = approver.asked();
        assert_eq!(
            asked[0].description,
            "[sub-agent] [destructive] bash: rm -rf x"
        );
        let path = std::fs::canonicalize(test_cwd()).unwrap().join("a.txt");
        assert_eq!(
            asked[1].description,
            format!("write_file: {}", path.display())
        );
    }

    /// The approval request carries a file-change diff for edit/write so the
    /// human sees the change; other tools carry none.
    #[tokio::test]
    async fn confirm_request_carries_a_change_preview() {
        let approver = ScriptedApprover::new(vec![Decision::Deny, Decision::Deny, Decision::Deny]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        let _ = p
            .check(
                "edit_file",
                &json!({"path": "f.rs", "old_string": "foo", "new_string": "bar"}),
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
        // f.rs cannot be read here, so the edit degrades to a two-string diff.
        assert_eq!(asked[0].preview.as_deref(), Some("-1  foo\n+1  bar"));
        assert_eq!(asked[1].preview.as_deref(), Some("(new file)\n+1  hi"));
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
        let approver =
            ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once), Decision::Deny]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        assert_eq!(
            p.escalate_sandbox("npm install", None, 0).await,
            EscalationOutcome::Approved
        );
        assert_eq!(
            p.escalate_sandbox("git push", None, 0).await,
            EscalationOutcome::Declined
        );
        assert_eq!(approver.ask_count(), 2);
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
            EscalationOutcome::Approved
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
            EscalationOutcome::Approved
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
            EscalationOutcome::Approved
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
            EscalationOutcome::Approved
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
            EscalationOutcome::Approved
        );
        assert_eq!(
            p.escalate_sandbox("go test ./pkg", None, 0).await,
            EscalationOutcome::Approved
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

    /// "May run" and "may run outside the sandbox" are different permissions,
    /// so neither rule form may stand in for the other.
    #[test]
    fn escalation_rules_and_bash_rules_do_not_substitute_for_each_other() {
        let argv = vec!["go".to_string(), "test".to_string(), "./pkg".to_string()];
        let escalate = parse_rule("sandbox_escalate(go test *)").unwrap();
        let bash = parse_rule("bash(go test *)").unwrap();
        let whole_tool = parse_rule("bash").unwrap();

        assert!(escalate.matches_escalation_argv(&argv));
        assert!(
            !bash.matches_escalation_argv(&argv),
            "bash(...) is not consent to uncontained"
        );
        assert!(
            !whole_tool.matches_escalation_argv(&argv),
            "the whole-tool bash rule is not consent to uncontained either"
        );

        assert!(bash.matches_argv(&argv));
        assert!(
            !escalate.matches_argv(&argv),
            "escalation consent does not let the command past the ordinary gate"
        );
        assert!(!escalate.matches_tool("bash"));
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
        for task_tool in [
            "task_create",
            "task_get",
            "task_update",
            "task_list",
            "task_clear",
        ] {
            assert!(ok(&p, task_tool, json!({})).await);
        }
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
        // Enter plan from accept-edits: that becomes the restore target.
        p.set_mode(Mode::AcceptEdits);
        p.set_mode(Mode::Plan);
        assert_eq!(p.mode(), Mode::Plan);

        assert_eq!(
            p.confirm_exit_plan("the plan", 0).await,
            PlanExitOutcome::Approved(Mode::AcceptEdits)
        );
        assert_eq!(p.mode(), Mode::AcceptEdits, "restored the pre-plan mode");
        // The approver saw the plan text as the popup preview.
        assert_eq!(approver.asked()[0].preview.as_deref(), Some("the plan"));

        // Back into plan, this time deny: stay put.
        p.set_mode(Mode::Plan);
        assert_eq!(
            p.confirm_exit_plan("v2", 0).await,
            PlanExitOutcome::Declined
        );
        assert_eq!(p.mode(), Mode::Plan, "denied exit keeps plan mode");
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

        for mode in [Mode::Manual, Mode::AcceptEdits, Mode::Bypass] {
            let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
            let permissions = gate(mode, rules(&[], &[], &[]), approver.clone());
            assert!(permissions.check("powershell", &input, 0).await.is_ok());
            assert_eq!(approver.ask_count(), 1, "{mode:?} must ask");
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
    async fn powershell_sensitive_windows_paths_force_bypass_immune_confirmation() {
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
                Mode::Bypass,
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

    #[test]
    fn powershell_prefix_rules_are_rejected_explicitly() {
        let error = parse_rule("powershell(Get-ChildItem *)")
            .unwrap_err()
            .to_string();
        assert!(error.contains("PowerShell v1"), "{error}");
        assert!(parse_rule("powershell").is_ok());
    }

    /// The string layer is the only screen left for a command too opaque to
    /// parse into argv, so its exemption must not become a traversal hole.
    #[test]
    fn raw_command_exemption_covers_worktrees_but_never_a_traversal() {
        assert!(!raw_mentions_sensitive_path(
            "cat /w/proj/.kloop/worktrees/wt/src/main.rs"
        ));
        assert!(raw_mentions_sensitive_path(
            "cat /w/proj/.kloop/sessions/x.jsonl"
        ));
        assert!(raw_mentions_sensitive_path(
            "cat /w/proj/.kloop/worktrees/../sessions/x.jsonl"
        ));
        // The rest of an exempted command is still screened.
        assert!(raw_mentions_sensitive_path(
            "cp /w/proj/.kloop/worktrees/wt/a ~/.ssh/authorized_keys"
        ));
        assert!(powershell_mentions_sensitive_path(
            "Get-Content /w/proj/.kloop/sessions/x.jsonl"
        ));
        assert!(!powershell_mentions_sensitive_path(
            "Get-Content /w/proj/.kloop/worktrees/wt/src/main.rs"
        ));
    }

    #[test]
    fn rule_parsing_accepts_valid_and_rejects_malformed() {
        assert!(parse_rule("write_file").is_ok());
        assert!(parse_rule("bash(cargo *)").is_ok());
        assert!(parse_rule("bash(git push origin)").is_ok());
        assert!(parse_rule("write_file(src/**)").is_ok());
        assert!(parse_rule("read_file(**/*.pem)").is_ok());

        assert!(parse_rule("bash()").is_err());
        assert!(parse_rule("bash(*)").is_err());
        assert!(parse_rule("task(x)").is_err());
        assert!(parse_rule("write file").is_err());
        // Plan 145 took the verbatim-script rule back out: an opaque script is
        // remembered for the session or not at all, so nothing durable may
        // spell one — including a store written by an older build.
        assert!(parse_rule("bash_script(cat > f <<EOF)").is_err());
        assert!(parse_rule("bash_script_no_sandbox(cat > f)").is_err());
    }

    #[test]
    fn sensitive_path_detection_is_component_based() {
        let s = |p: &str| path_is_sensitive(Path::new(p));
        assert!(s("/w/proj/.git/hooks/pre-commit"));
        assert!(s("/w/proj/.git"));
        assert!(s("/w/proj/.kloop/sessions/x.jsonl"));
        assert!(s("/home/u/.bashrc"));
        assert!(s("/w/proj/.env.production"));
        assert!(s("/home/u/.ssh/id_rsa"));

        assert!(!s("/w/proj/src/main.rs"));
        // A managed worktree is a checkout the agent edits wholesale: the one
        // exempt segment pair. Everything else under `.kloop` stays sensitive,
        // including a nested `.kloop` inside the worktree and a traversal that
        // re-enters kloop's own state through the exemption.
        let tree = format!("/w/proj/{}/wt", crate::worktree::WORKTREES_DIR);
        assert!(!s(&format!("{tree}/src/main.rs")));
        assert!(s(&format!("{tree}/.kloop/config.toml")), "nested state");
        assert!(s("/w/proj/.kloop/worktrees/../sessions/x.jsonl"));
        assert!(
            s("/w/proj/.kloop/worktreesx/src/main.rs"),
            "prefix ≠ segment"
        );
        assert!(!s("/w/proj/git/readme.md"), "git dir ≠ .git dir");
        assert!(!s("/w/proj/environment.rs"), ".env prefix is filename-only");
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

    #[test]
    fn sensitive_path_folds_case_on_case_insensitive_fs() {
        let s = |p: &str| path_is_sensitive(Path::new(p));
        // A cased alias resolves to the real `.git`/`.ssh`/`.env*` on a
        // case-folding FS, so it must be caught there; on a case-sensitive FS
        // those are genuinely distinct paths and stay non-sensitive.
        assert_eq!(s("/w/proj/.GIT/hooks/pre-commit"), FS_FOLDS_CASE);
        assert_eq!(s("/w/proj/.SSH/id_rsa"), FS_FOLDS_CASE);
        assert_eq!(s("/w/proj/.Env.production"), FS_FOLDS_CASE);
        assert_eq!(s("/home/u/.Bashrc"), FS_FOLDS_CASE);
    }
}
