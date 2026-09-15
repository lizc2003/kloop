use std::fmt;
use std::sync::Arc;

use anyhow::Result;
use anyhow::anyhow;
use kloop_protocol::LocalAgentId;
use kloop_protocol::LocalContextId;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use sha2::Digest as _;
use sha2::Sha256;

use crate::config::EffectiveWorkspace;

const RECEIPT_VERSION: u8 = 1;
pub(crate) const MAX_RECEIPT_BYTES: usize = 2 * 1024;
const SESSION_DOMAIN: &[u8] = b"kloop-execution-session/v1\0";
const THREAD_DOMAIN: &[u8] = b"kloop-execution-thread/v1\0";
const ROLLOUT_DOMAIN: &[u8] = b"kloop-execution-rollout/v1\0";

macro_rules! transient_id {
    ($name:ident, $prefix:literal, $noun:literal) => {
        #[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
        #[serde(transparent)]
        pub(crate) struct $name(String);

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let raw = String::deserialize(deserializer)?;
                Self::parse(&raw).map_err(serde::de::Error::custom)
            }
        }

        impl $name {
            pub(crate) fn parse(raw: &str) -> Result<Self> {
                let Some(sequence) = raw.strip_prefix($prefix) else {
                    return Err(anyhow!(concat!($noun, " id must use `", $prefix, "N`")));
                };
                if sequence.is_empty()
                    || sequence.starts_with('0')
                    || !sequence.bytes().all(|byte| byte.is_ascii_digit())
                    || sequence.parse::<u64>().is_err()
                {
                    return Err(anyhow!(concat!(
                        $noun,
                        " id must use canonical `",
                        $prefix,
                        "N` with N >= 1"
                    )));
                }
                Ok(Self(raw.to_string()))
            }

            pub(crate) fn as_str(&self) -> &str {
                &self.0
            }

            fn validate(&self) -> Result<()> {
                Self::parse(&self.0).map(|_| ())
            }

            fn sequence(&self) -> Result<u64> {
                self.validate()?;
                Ok(self.0[$prefix.len()..]
                    .parse()
                    .expect("validated transient execution sequence"))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

transient_id!(AgentExecutionId, "agent-", "Agent execution");
transient_id!(ProgramExecutionId, "program-", "Program execution");
transient_id!(WorkflowExecutionId, "workflow-", "Workflow execution");
transient_id!(BackgroundShellId, "bg-", "background shell");

macro_rules! durable_id {
    ($name:ident, $prefix:literal, $separator:literal, $noun:literal) => {
        #[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
        #[serde(transparent)]
        pub(crate) struct $name(String);

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let raw = String::deserialize(deserializer)?;
                Self::parse(&raw).map_err(serde::de::Error::custom)
            }
        }

        impl $name {
            pub(crate) fn parse(raw: &str) -> Result<Self> {
                let Some(rest) = raw.strip_prefix($prefix) else {
                    return Err(anyhow!(concat!($noun, " id has the wrong prefix")));
                };
                let Some((clock, sequence)) = rest.split_once($separator) else {
                    return Err(anyhow!(concat!($noun, " id has the wrong shape")));
                };
                if clock.is_empty()
                    || !clock.bytes().all(|byte| byte.is_ascii_digit())
                    || sequence.is_empty()
                    || sequence.starts_with('0')
                    || !sequence.bytes().all(|byte| byte.is_ascii_digit())
                    || sequence.parse::<u64>().is_err()
                {
                    return Err(anyhow!(concat!($noun, " id has the wrong shape")));
                }
                Ok(Self(raw.to_string()))
            }

            fn validate(&self) -> Result<()> {
                Self::parse(&self.0).map(|_| ())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

durable_id!(ProgramRunId, "run-", "-", "Program run");
durable_id!(WorkflowRunId, "wf_", "-", "Workflow run");

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub(crate) enum TransientExecutionId {
    Agent(AgentExecutionId),
    Program(ProgramExecutionId),
    Workflow(WorkflowExecutionId),
    Shell(BackgroundShellId),
}

impl TransientExecutionId {
    pub(crate) fn as_str(&self) -> &str {
        match self {
            Self::Agent(id) => id.as_str(),
            Self::Program(id) => id.as_str(),
            Self::Workflow(id) => id.as_str(),
            Self::Shell(id) => id.as_str(),
        }
    }

    pub(crate) fn kind(&self) -> ExecutionKind {
        match self {
            Self::Agent(_) => ExecutionKind::Agent,
            Self::Program(_) => ExecutionKind::Program,
            Self::Workflow(_) => ExecutionKind::Workflow,
            Self::Shell(_) => ExecutionKind::Shell,
        }
    }

    fn validate(&self) -> Result<()> {
        match self {
            Self::Agent(id) => id.validate(),
            Self::Program(id) => id.validate(),
            Self::Workflow(id) => id.validate(),
            Self::Shell(id) => id.validate(),
        }
    }

    fn sequence(&self) -> Result<u64> {
        match self {
            Self::Agent(id) => id.sequence(),
            Self::Program(id) => id.sequence(),
            Self::Workflow(id) => id.sequence(),
            Self::Shell(id) => id.sequence(),
        }
    }
}

impl fmt::Display for TransientExecutionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub(crate) enum DurableExecutionId {
    Program(ProgramRunId),
    Workflow(WorkflowRunId),
}

impl DurableExecutionId {
    fn as_str(&self) -> &str {
        match self {
            Self::Program(id) => &id.0,
            Self::Workflow(id) => &id.0,
        }
    }

    fn kind(&self) -> ExecutionKind {
        match self {
            Self::Program(_) => ExecutionKind::Program,
            Self::Workflow(_) => ExecutionKind::Workflow,
        }
    }

    fn validate(&self) -> Result<()> {
        match self {
            Self::Program(id) => id.validate(),
            Self::Workflow(id) => id.validate(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExecutionKind {
    Agent,
    Program,
    Workflow,
    Shell,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecutionRef {
    execution: TransientExecutionId,
    durable: Option<DurableExecutionId>,
}

impl ExecutionRef {
    fn validate(&self) -> Result<()> {
        validate_durable_pair(&self.execution, self.durable.as_ref())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopeRef {
    session: Option<OpaqueRef>,
    thread: Option<OpaqueRef>,
}

impl ScopeRef {
    fn from_session(raw: &str) -> Self {
        if raw.is_empty() {
            return Self {
                session: None,
                thread: None,
            };
        }
        Self {
            session: Some(OpaqueRef::digest(SESSION_DOMAIN, raw)),
            thread: Some(OpaqueRef::digest(THREAD_DOMAIN, raw)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
struct OpaqueRef(String);

impl OpaqueRef {
    fn digest(domain: &[u8], raw: &str) -> Self {
        let mut digest = Sha256::new();
        digest.update(domain);
        digest.update((raw.len() as u64).to_be_bytes());
        digest.update(raw.as_bytes());
        Self(format!("{:x}", digest.finalize()))
    }

    fn validate(&self) -> Result<()> {
        if self.0.len() != 64
            || !self
                .0
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(anyhow!("opaque provenance identity is malformed"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum MailboxRoute {
    Agent {
        context_id: LocalContextId,
        parent: LocalAgentId,
        child: LocalAgentId,
    },
    NotMailboxPeer,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AdmissionAuthority {
    context_id: LocalContextId,
    caller: LocalAgentId,
    depth: u8,
}

impl AdmissionAuthority {
    pub(crate) fn new(context_id: LocalContextId, caller: LocalAgentId, depth: u8) -> Self {
        Self {
            context_id,
            caller,
            depth,
        }
    }

    #[cfg(test)]
    pub(crate) fn caller(&self) -> &LocalAgentId {
        &self.caller
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "value", rename_all = "snake_case")]
enum Evidence<T> {
    Available(T),
    Unavailable,
    NotApplicable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RolloutLineRef {
    session: OpaqueRef,
    sequence: u64,
}

impl RolloutLineRef {
    fn parse(raw: &str) -> Option<Self> {
        let (session, sequence) = raw.rsplit_once('#')?;
        let sequence = sequence.parse::<u64>().ok().filter(|value| *value > 0)?;
        (!session.is_empty()).then(|| Self {
            session: OpaqueRef::digest(ROLLOUT_DOMAIN, session),
            sequence,
        })
    }

    fn validate(&self) -> Result<()> {
        if self.sequence == 0 {
            return Err(anyhow!("rollout sequence must be positive"));
        }
        self.session.validate()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RolloutProvenance {
    parent_line: Evidence<RolloutLineRef>,
    subagent_of: Evidence<RolloutLineRef>,
}

impl RolloutProvenance {
    fn new(parent: Option<&str>, child_lineage: bool) -> Self {
        let parsed = parent.and_then(RolloutLineRef::parse);
        Self {
            parent_line: parsed
                .clone()
                .map_or(Evidence::Unavailable, Evidence::Available),
            subagent_of: if child_lineage {
                parsed.map_or(Evidence::Unavailable, Evidence::Available)
            } else {
                Evidence::NotApplicable
            },
        }
    }

    fn validate(&self) -> Result<()> {
        for evidence in [&self.parent_line, &self.subagent_of] {
            if let Evidence::Available(line) = evidence {
                line.validate()?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
struct ProjectRef(String);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
struct WorkspaceRef(String);

fn validate_digest_id(raw: &str, prefix: &str, noun: &str) -> Result<()> {
    let Some(digest) = raw.strip_prefix(prefix) else {
        return Err(anyhow!("{noun} has the wrong prefix"));
    };
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(anyhow!("{noun} is malformed"));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkspaceDisposition {
    Base,
    SessionWorktree,
    IsolatedChild,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkspaceProvenance {
    project: Option<ProjectRef>,
    workspace: WorkspaceRef,
    workspace_epoch: u64,
    disposition: WorkspaceDisposition,
}

impl WorkspaceProvenance {
    pub(crate) fn capture_current(workspace: &EffectiveWorkspace) -> Self {
        Self::capture(
            workspace,
            if workspace.branch.is_some() {
                WorkspaceDisposition::SessionWorktree
            } else {
                WorkspaceDisposition::Base
            },
        )
    }

    pub(crate) fn capture(
        workspace: &EffectiveWorkspace,
        disposition: WorkspaceDisposition,
    ) -> Self {
        Self {
            project: workspace
                .identity
                .project_id()
                .map(|id| ProjectRef(id.as_str().to_string())),
            workspace: WorkspaceRef(workspace.identity.workspace_id().as_str().to_string()),
            workspace_epoch: workspace.workspace_epoch,
            disposition,
        }
    }

    fn validate(&self) -> Result<()> {
        if let Some(project) = &self.project {
            validate_digest_id(&project.0, "p1_", "project identity")?;
        }
        validate_digest_id(&self.workspace.0, "w1_", "workspace identity")
    }

    #[cfg(test)]
    pub(crate) fn disposition(&self) -> WorkspaceDisposition {
        self.disposition
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AdmissionOrigin {
    RunAgent,
    StructuredAgent,
    SkillFork,
    Program,
    Workflow,
    BackgroundShell,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TerminalOwner {
    ForegroundCaller,
    BackgroundExecutions,
    BackgroundShells,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DeliveryRoute {
    DirectToolResult,
    ParentInboxBody,
    ShellOutputPointer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TerminalRoute {
    owner: TerminalOwner,
    delivery: DeliveryRoute,
}

impl TerminalRoute {
    pub(crate) fn new(owner: TerminalOwner, delivery: DeliveryRoute) -> Self {
        Self { owner, delivery }
    }

    pub(crate) fn owner(&self) -> TerminalOwner {
        self.owner
    }

    pub(crate) fn delivery(&self) -> DeliveryRoute {
        self.delivery
    }
}

pub(crate) struct ResolvedExecutionAdmission<'a> {
    pub(crate) session_id: &'a str,
    pub(crate) parent: Option<ExecutionRef>,
    pub(crate) execution: TransientExecutionId,
    pub(crate) durable: Option<DurableExecutionId>,
    pub(crate) mailbox: MailboxRoute,
    pub(crate) authority: AdmissionAuthority,
    pub(crate) parent_rollout_id: Option<&'a str>,
    pub(crate) workspace: WorkspaceProvenance,
    pub(crate) origin: AdmissionOrigin,
    pub(crate) terminal: TerminalRoute,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecutionProvenanceReceipt {
    version: u8,
    scope: ScopeRef,
    parent: Option<ExecutionRef>,
    execution: TransientExecutionId,
    durable: Option<DurableExecutionId>,
    mailbox: MailboxRoute,
    authority: AdmissionAuthority,
    rollout: RolloutProvenance,
    workspace: WorkspaceProvenance,
    origin: AdmissionOrigin,
    terminal: TerminalRoute,
}

#[cfg(test)]
fn test_effective_workspace() -> EffectiveWorkspace {
    EffectiveWorkspace {
        identity: crate::project::WorkspaceIdentity::ephemeral(std::env::temp_dir()),
        workspace_epoch: 0,
        cwd: std::env::temp_dir(),
        permissions: Arc::new(crate::permissions::Permissions::allow_all()),
        file_state: Arc::default(),
        sandbox: None,
        system: String::new(),
        branch: None,
    }
}

#[derive(Clone)]
pub(crate) struct ExecutionRegistration {
    receipt: Arc<ExecutionProvenanceReceipt>,
}

impl ExecutionRegistration {
    pub(crate) fn new(receipt: Arc<ExecutionProvenanceReceipt>) -> Self {
        Self { receipt }
    }

    pub(crate) fn id(&self) -> &str {
        self.receipt.execution().as_str()
    }

    pub(crate) fn shares_receipt(&self, receipt: &Arc<ExecutionProvenanceReceipt>) -> bool {
        Arc::ptr_eq(&self.receipt, receipt)
    }
}

impl ExecutionProvenanceReceipt {
    pub(crate) fn mint(admission: ResolvedExecutionAdmission<'_>) -> Result<Arc<Self>> {
        let child_lineage = admission.execution.kind() == ExecutionKind::Agent;
        let receipt = Self {
            version: RECEIPT_VERSION,
            scope: ScopeRef::from_session(admission.session_id),
            parent: admission.parent,
            execution: admission.execution,
            durable: admission.durable,
            mailbox: admission.mailbox,
            authority: admission.authority,
            rollout: RolloutProvenance::new(admission.parent_rollout_id, child_lineage),
            workspace: admission.workspace,
            origin: admission.origin,
            terminal: admission.terminal,
        };
        receipt.validate()?;
        receipt.ensure_bounded()?;
        Ok(Arc::new(receipt))
    }

    pub(crate) fn from_persisted_value(value: &Value) -> Result<Arc<Self>> {
        let bytes = serde_json::to_vec(value)?;
        if bytes.len() > MAX_RECEIPT_BYTES {
            return Err(anyhow!(
                "execution provenance receipt exceeds {MAX_RECEIPT_BYTES} bytes"
            ));
        }
        let receipt: Self = serde_json::from_value(value.clone())?;
        receipt.validate()?;
        receipt.ensure_bounded()?;
        Ok(Arc::new(receipt))
    }

    pub(crate) fn to_persisted_value(&self) -> Value {
        serde_json::to_value(self).expect("validated provenance receipt serializes")
    }

    pub(crate) fn execution(&self) -> &TransientExecutionId {
        &self.execution
    }

    pub(crate) fn durable(&self) -> Option<&DurableExecutionId> {
        self.durable.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn parent(&self) -> Option<&ExecutionRef> {
        self.parent.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn mailbox(&self) -> &MailboxRoute {
        &self.mailbox
    }

    #[cfg(test)]
    pub(crate) fn authority(&self) -> &AdmissionAuthority {
        &self.authority
    }

    #[cfg(test)]
    pub(crate) fn workspace(&self) -> &WorkspaceProvenance {
        &self.workspace
    }

    pub(crate) fn terminal(&self) -> TerminalRoute {
        self.terminal
    }

    pub(crate) fn is_agent_journal_evidence(&self, expected_parent_durable: Option<&str>) -> bool {
        let Some(parent) = &self.parent else {
            return false;
        };
        self.execution.kind() == ExecutionKind::Agent
            && matches!(
                parent.execution.kind(),
                ExecutionKind::Program | ExecutionKind::Workflow
            )
            && expected_parent_durable.is_none_or(|expected| {
                parent
                    .durable
                    .as_ref()
                    .is_some_and(|durable| durable.as_str() == expected)
            })
            && matches!(
                (self.terminal.owner, self.terminal.delivery),
                (
                    TerminalOwner::ForegroundCaller,
                    DeliveryRoute::DirectToolResult
                )
            )
            && matches!(
                self.origin,
                AdmissionOrigin::RunAgent | AdmissionOrigin::StructuredAgent
            )
    }

    pub(crate) fn as_execution_ref(&self) -> ExecutionRef {
        ExecutionRef {
            execution: self.execution.clone(),
            durable: self.durable.clone(),
        }
    }

    #[cfg(test)]
    pub(crate) fn test_journal_agent(id: &str, parent_run_id: &str) -> Arc<Self> {
        let context_id = LocalContextId::new("local-context-1").unwrap();
        let effective = test_effective_workspace();
        Self::mint(ResolvedExecutionAdmission {
            session_id: "",
            parent: Some(ExecutionRef {
                execution: TransientExecutionId::Program(
                    ProgramExecutionId::parse("program-1").unwrap(),
                ),
                durable: Some(DurableExecutionId::Program(
                    ProgramRunId::parse(parent_run_id).unwrap(),
                )),
            }),
            execution: TransientExecutionId::Agent(AgentExecutionId::parse(id).unwrap()),
            durable: None,
            mailbox: MailboxRoute::Agent {
                context_id: context_id.clone(),
                parent: LocalAgentId::Main,
                child: id.parse().unwrap(),
            },
            authority: AdmissionAuthority::new(context_id, LocalAgentId::Main, 0),
            parent_rollout_id: None,
            workspace: WorkspaceProvenance::capture(&effective, WorkspaceDisposition::Base),
            origin: AdmissionOrigin::RunAgent,
            terminal: TerminalRoute::new(
                TerminalOwner::ForegroundCaller,
                DeliveryRoute::DirectToolResult,
            ),
        })
        .unwrap()
    }

    #[cfg(test)]
    pub(crate) fn test_program(id: &str, run_id: &str) -> Arc<Self> {
        let context_id = LocalContextId::new("local-context-1").unwrap();
        let effective = test_effective_workspace();
        Self::mint(ResolvedExecutionAdmission {
            session_id: "",
            parent: None,
            execution: TransientExecutionId::Program(ProgramExecutionId::parse(id).unwrap()),
            durable: Some(DurableExecutionId::Program(
                ProgramRunId::parse(run_id).unwrap(),
            )),
            mailbox: MailboxRoute::NotMailboxPeer,
            authority: AdmissionAuthority::new(context_id, LocalAgentId::Main, 0),
            parent_rollout_id: None,
            workspace: WorkspaceProvenance::capture(&effective, WorkspaceDisposition::Base),
            origin: AdmissionOrigin::Program,
            terminal: TerminalRoute::new(
                TerminalOwner::ForegroundCaller,
                DeliveryRoute::DirectToolResult,
            ),
        })
        .unwrap()
    }

    fn ensure_bounded(&self) -> Result<()> {
        let size = serde_json::to_vec(self)?.len();
        if size > MAX_RECEIPT_BYTES {
            return Err(anyhow!(
                "execution provenance receipt exceeds {MAX_RECEIPT_BYTES} bytes"
            ));
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if self.version != RECEIPT_VERSION {
            return Err(anyhow!(
                "unsupported execution provenance version {}",
                self.version
            ));
        }
        if let Some(session) = &self.scope.session {
            session.validate()?;
        }
        if let Some(thread) = &self.scope.thread {
            thread.validate()?;
        }
        if self.scope.session.is_some() != self.scope.thread.is_some() {
            return Err(anyhow!(
                "session and thread provenance must have equal availability"
            ));
        }
        if let Some(parent) = &self.parent {
            parent.validate()?;
        }
        validate_durable_pair(&self.execution, self.durable.as_ref())?;
        validate_mailbox(&self.execution, &self.mailbox)?;
        validate_origin(&self.execution, self.origin)?;
        validate_terminal(&self.execution, self.terminal)?;
        self.rollout.validate()?;
        self.workspace.validate()?;
        Ok(())
    }
}

fn validate_durable_pair(
    execution: &TransientExecutionId,
    durable: Option<&DurableExecutionId>,
) -> Result<()> {
    execution.validate()?;
    if let Some(durable) = durable {
        durable.validate()?;
    }
    let valid = matches!(
        (execution, durable),
        (TransientExecutionId::Agent(_), None)
            | (
                TransientExecutionId::Program(_),
                Some(DurableExecutionId::Program(_))
            )
            | (
                TransientExecutionId::Workflow(_),
                Some(DurableExecutionId::Workflow(_))
            )
            | (TransientExecutionId::Shell(_), None)
    );
    valid
        .then_some(())
        .ok_or_else(|| anyhow!("execution and durable provenance kinds do not match"))
}

fn validate_mailbox(execution: &TransientExecutionId, mailbox: &MailboxRoute) -> Result<()> {
    match (execution, mailbox) {
        (
            TransientExecutionId::Agent(execution),
            MailboxRoute::Agent {
                context_id: _,
                parent: _,
                child: LocalAgentId::Agent(child),
            },
        ) if child == execution.as_str() => Ok(()),
        (TransientExecutionId::Agent(_), _) => Err(anyhow!(
            "Agent execution must carry its exact mailbox route"
        )),
        (_, MailboxRoute::NotMailboxPeer) => Ok(()),
        _ => Err(anyhow!(
            "Program, Workflow, and shell executions are not mailbox peers"
        )),
    }
}

fn validate_origin(execution: &TransientExecutionId, origin: AdmissionOrigin) -> Result<()> {
    let valid = match execution {
        TransientExecutionId::Agent(_) => matches!(
            origin,
            AdmissionOrigin::RunAgent
                | AdmissionOrigin::StructuredAgent
                | AdmissionOrigin::SkillFork
        ),
        TransientExecutionId::Program(_) => origin == AdmissionOrigin::Program,
        TransientExecutionId::Workflow(_) => origin == AdmissionOrigin::Workflow,
        TransientExecutionId::Shell(_) => origin == AdmissionOrigin::BackgroundShell,
    };
    valid
        .then_some(())
        .ok_or_else(|| anyhow!("execution and admission origin do not match"))
}

fn validate_terminal(execution: &TransientExecutionId, terminal: TerminalRoute) -> Result<()> {
    let valid = match execution {
        TransientExecutionId::Agent(_) | TransientExecutionId::Program(_) => matches!(
            (terminal.owner, terminal.delivery),
            (
                TerminalOwner::ForegroundCaller,
                DeliveryRoute::DirectToolResult
            ) | (
                TerminalOwner::BackgroundExecutions,
                DeliveryRoute::ParentInboxBody
            )
        ),
        TransientExecutionId::Workflow(_) => matches!(
            (terminal.owner, terminal.delivery),
            (
                TerminalOwner::BackgroundExecutions,
                DeliveryRoute::ParentInboxBody
            )
        ),
        TransientExecutionId::Shell(_) => matches!(
            (terminal.owner, terminal.delivery),
            (
                TerminalOwner::BackgroundShells,
                DeliveryRoute::ShellOutputPointer
            )
        ),
    };
    valid
        .then_some(())
        .ok_or_else(|| anyhow!("execution and terminal route do not match"))
}

pub(crate) const MAX_PROVENANCE_HISTORY_BYTES: usize = 128 * 1024;
const MAX_PROVENANCE_ATTEMPTS: usize = 32;
const PROVENANCE_HISTORY_VERSION: u8 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProvenanceHistory {
    version: u8,
    durable: DurableExecutionId,
    attempts: Vec<Value>,
}

impl ProvenanceHistory {
    fn validated_attempts(&self) -> Result<Vec<Arc<ExecutionProvenanceReceipt>>> {
        if self.version != PROVENANCE_HISTORY_VERSION {
            return Err(anyhow!("unsupported provenance history version"));
        }
        if self.attempts.len() > MAX_PROVENANCE_ATTEMPTS {
            return Err(anyhow!("provenance history exceeds the attempt cap"));
        }
        self.durable.validate()?;
        let attempts = self
            .attempts
            .iter()
            .map(ExecutionProvenanceReceipt::from_persisted_value)
            .collect::<Result<Vec<_>>>()?;
        if attempts
            .iter()
            .any(|receipt| receipt.durable() != Some(&self.durable))
        {
            return Err(anyhow!("provenance history durable identity mismatch"));
        }
        Ok(attempts)
    }
}

/// Return the canonical transient sequences already recorded for one durable
/// execution kind. `None` means the history is unavailable and must not be
/// inferred; a missing sidecar is a valid empty history.
pub(crate) fn persisted_attempt_sequences(
    existing: Option<&[u8]>,
    kind: ExecutionKind,
) -> Option<Vec<u64>> {
    let Some(bytes) = existing else {
        return Some(Vec::new());
    };
    if bytes.len() > MAX_PROVENANCE_HISTORY_BYTES {
        return None;
    }
    let history: ProvenanceHistory = serde_json::from_slice(bytes).ok()?;
    if history.durable.kind() != kind {
        return None;
    }
    let attempts = history.validated_attempts().ok()?;
    attempts
        .into_iter()
        .map(|receipt| {
            (receipt.execution().kind() == kind)
                .then(|| receipt.execution().sequence().ok())
                .flatten()
        })
        .collect()
}

pub(crate) enum ProvenanceHistoryUpdate {
    Write(Vec<u8>),
    Preserve,
}

pub(crate) fn update_provenance_history(
    existing: Option<&[u8]>,
    receipt: &ExecutionProvenanceReceipt,
) -> ProvenanceHistoryUpdate {
    let Some(durable) = receipt.durable().cloned() else {
        return ProvenanceHistoryUpdate::Preserve;
    };
    let (mut history, attempts) = match existing {
        Some(bytes) => {
            if bytes.len() > MAX_PROVENANCE_HISTORY_BYTES {
                return ProvenanceHistoryUpdate::Preserve;
            }
            let Ok(history) = serde_json::from_slice::<ProvenanceHistory>(bytes) else {
                return ProvenanceHistoryUpdate::Preserve;
            };
            let Ok(attempts) = history.validated_attempts() else {
                return ProvenanceHistoryUpdate::Preserve;
            };
            if history.durable != durable {
                return ProvenanceHistoryUpdate::Preserve;
            }
            (history, attempts)
        }
        None => (
            ProvenanceHistory {
                version: PROVENANCE_HISTORY_VERSION,
                durable,
                attempts: Vec::new(),
            },
            Vec::new(),
        ),
    };
    if history.attempts.len() >= MAX_PROVENANCE_ATTEMPTS
        || attempts
            .iter()
            .any(|stored| stored.execution() == receipt.execution())
    {
        return ProvenanceHistoryUpdate::Preserve;
    }
    history.attempts.push(receipt.to_persisted_value());
    match serde_json::to_vec_pretty(&history) {
        Ok(bytes) if bytes.len() <= MAX_PROVENANCE_HISTORY_BYTES => {
            ProvenanceHistoryUpdate::Write(bytes)
        }
        _ => ProvenanceHistoryUpdate::Preserve,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::WorkspaceIdentity;
    use serde_json::json;
    use std::path::PathBuf;

    fn workspace(disposition: WorkspaceDisposition) -> WorkspaceProvenance {
        WorkspaceProvenance::capture(
            &EffectiveWorkspace {
                identity: WorkspaceIdentity::ephemeral(PathBuf::from("/private/secret-worktree")),
                workspace_epoch: 7,
                cwd: PathBuf::from("/private/secret-worktree"),
                permissions: Arc::new(crate::permissions::Permissions::allow_all()),
                file_state: Arc::default(),
                sandbox: None,
                system: "secret prompt".into(),
                branch: Some("secret-branch".into()),
            },
            disposition,
        )
    }

    fn agent_receipt() -> Arc<ExecutionProvenanceReceipt> {
        let execution = AgentExecutionId::parse("agent-7").unwrap();
        ExecutionProvenanceReceipt::mint(ResolvedExecutionAdmission {
            session_id: "session-private-name",
            parent: None,
            execution: TransientExecutionId::Agent(execution),
            durable: None,
            mailbox: MailboxRoute::Agent {
                context_id: LocalContextId::new("local-context-3").unwrap(),
                parent: LocalAgentId::Main,
                child: "agent-7".parse().unwrap(),
            },
            authority: AdmissionAuthority::new(
                LocalContextId::new("local-context-3").unwrap(),
                LocalAgentId::Main,
                0,
            ),
            parent_rollout_id: Some("session-private-name#8"),
            workspace: workspace(WorkspaceDisposition::IsolatedChild),
            origin: AdmissionOrigin::RunAgent,
            terminal: TerminalRoute::new(
                TerminalOwner::ForegroundCaller,
                DeliveryRoute::DirectToolResult,
            ),
        })
        .unwrap()
    }

    fn program_receipt(label: &str, run_id: &str) -> Arc<ExecutionProvenanceReceipt> {
        ExecutionProvenanceReceipt::mint(ResolvedExecutionAdmission {
            session_id: "session-private-name",
            parent: None,
            execution: TransientExecutionId::Program(ProgramExecutionId::parse(label).unwrap()),
            durable: Some(DurableExecutionId::Program(
                ProgramRunId::parse(run_id).unwrap(),
            )),
            mailbox: MailboxRoute::NotMailboxPeer,
            authority: AdmissionAuthority::new(
                LocalContextId::new("local-context-3").unwrap(),
                LocalAgentId::Main,
                0,
            ),
            parent_rollout_id: Some("session-private-name#8"),
            workspace: workspace(WorkspaceDisposition::Base),
            origin: AdmissionOrigin::Program,
            terminal: TerminalRoute::new(
                TerminalOwner::ForegroundCaller,
                DeliveryRoute::DirectToolResult,
            ),
        })
        .unwrap()
    }

    fn workflow_receipt(label: &str, run_id: &str) -> Arc<ExecutionProvenanceReceipt> {
        ExecutionProvenanceReceipt::mint(ResolvedExecutionAdmission {
            session_id: "session-private-name",
            parent: None,
            execution: TransientExecutionId::Workflow(WorkflowExecutionId::parse(label).unwrap()),
            durable: Some(DurableExecutionId::Workflow(
                WorkflowRunId::parse(run_id).unwrap(),
            )),
            mailbox: MailboxRoute::NotMailboxPeer,
            authority: AdmissionAuthority::new(
                LocalContextId::new("local-context-3").unwrap(),
                LocalAgentId::Main,
                0,
            ),
            parent_rollout_id: Some("session-private-name#8"),
            workspace: workspace(WorkspaceDisposition::Base),
            origin: AdmissionOrigin::Workflow,
            terminal: TerminalRoute::new(
                TerminalOwner::BackgroundExecutions,
                DeliveryRoute::ParentInboxBody,
            ),
        })
        .unwrap()
    }

    #[test]
    fn canonical_execution_ids_are_typed_and_strict() {
        assert_eq!(
            AgentExecutionId::parse("agent-1").unwrap().as_str(),
            "agent-1"
        );
        assert!(AgentExecutionId::parse("program-1").is_err());
        assert!(AgentExecutionId::parse("agent-0").is_err());
        assert!(ProgramExecutionId::parse("program-01").is_err());
        assert!(WorkflowExecutionId::parse("workflow-x").is_err());
        assert!(BackgroundShellId::parse("bg-2").is_ok());
        assert!(ProgramRunId::parse("run-0-1").is_ok());
        assert!(ProgramRunId::parse("run-1-x").is_err());
        assert!(WorkflowRunId::parse("wf_1-1").is_ok());
        assert!(WorkflowRunId::parse("run-1-1").is_err());
        assert!(serde_json::from_value::<ProgramExecutionId>(json!("program-0")).is_err());
        assert!(serde_json::from_value::<ProgramExecutionId>(json!("program-x")).is_err());
        assert!(serde_json::from_value::<WorkflowExecutionId>(json!("workflow-0")).is_err());
        assert!(serde_json::from_value::<ProgramRunId>(json!("run-bad-1")).is_err());
        assert!(serde_json::from_value::<WorkflowRunId>(json!("wf_bad-1")).is_err());
    }

    #[test]
    fn receipt_is_bounded_and_omits_free_form_secrets() {
        let receipt = agent_receipt();
        let json = serde_json::to_string(&*receipt).unwrap();
        assert!(json.len() <= MAX_RECEIPT_BYTES);
        for forbidden in [
            "session-private-name",
            "/private/secret-worktree",
            "secret prompt",
            "secret-branch",
            "endpoint",
            "provider",
            "model",
            "billing",
            "task_id",
        ] {
            assert!(!json.contains(forbidden), "leaked {forbidden}: {json}");
        }
    }

    #[test]
    fn session_and_thread_refs_are_domain_separated() {
        let receipt = agent_receipt();
        assert_ne!(receipt.scope.session, receipt.scope.thread);
    }

    #[test]
    fn non_agents_cannot_claim_mailbox_identity() {
        let result = ExecutionProvenanceReceipt::mint(ResolvedExecutionAdmission {
            session_id: "s",
            parent: None,
            execution: TransientExecutionId::Program(
                ProgramExecutionId::parse("program-1").unwrap(),
            ),
            durable: Some(DurableExecutionId::Program(
                ProgramRunId::parse("run-1-1").unwrap(),
            )),
            mailbox: MailboxRoute::Agent {
                context_id: LocalContextId::new("local-context-1").unwrap(),
                parent: LocalAgentId::Main,
                child: "agent-1".parse().unwrap(),
            },
            authority: AdmissionAuthority::new(
                LocalContextId::new("local-context-1").unwrap(),
                LocalAgentId::Main,
                0,
            ),
            parent_rollout_id: None,
            workspace: workspace(WorkspaceDisposition::Base),
            origin: AdmissionOrigin::Program,
            terminal: TerminalRoute::new(
                TerminalOwner::ForegroundCaller,
                DeliveryRoute::DirectToolResult,
            ),
        });
        assert!(result.is_err());
    }

    #[test]
    fn persisted_receipt_revalidates_kind_combinations() {
        let receipt = agent_receipt();
        let mut value = receipt.to_persisted_value();
        value["execution"] = serde_json::json!({"kind":"program","id":"program-1"});
        assert!(ExecutionProvenanceReceipt::from_persisted_value(&value).is_err());
    }

    #[test]
    fn parent_reference_is_flat() {
        let receipt = agent_receipt();
        let parent = receipt.as_execution_ref();
        let child = ExecutionProvenanceReceipt::mint(ResolvedExecutionAdmission {
            session_id: "s",
            parent: Some(parent),
            execution: TransientExecutionId::Shell(BackgroundShellId::parse("bg-1").unwrap()),
            durable: None,
            mailbox: MailboxRoute::NotMailboxPeer,
            authority: AdmissionAuthority::new(
                LocalContextId::new("local-context-3").unwrap(),
                "agent-7".parse().unwrap(),
                1,
            ),
            parent_rollout_id: Some("s#9"),
            workspace: workspace(WorkspaceDisposition::IsolatedChild),
            origin: AdmissionOrigin::BackgroundShell,
            terminal: TerminalRoute::new(
                TerminalOwner::BackgroundShells,
                DeliveryRoute::ShellOutputPointer,
            ),
        })
        .unwrap();
        let json = serde_json::to_string(&*child).unwrap();
        assert_eq!(json.matches("execution").count(), 2);
        assert!(!json.contains("parent\":{\"parent"));
    }

    #[test]
    fn provenance_history_appends_valid_attempts_and_preserves_bad_input() {
        let first = program_receipt("program-1", "run-7-1");
        let ProvenanceHistoryUpdate::Write(first_bytes) = update_provenance_history(None, &first)
        else {
            panic!("fresh provenance history was not written");
        };
        let first_history: ProvenanceHistory = serde_json::from_slice(&first_bytes).unwrap();
        assert_eq!(first_history.durable, first.durable().unwrap().clone());
        assert_eq!(first_history.attempts, vec![first.to_persisted_value()]);

        let second = program_receipt("program-2", "run-7-1");
        let ProvenanceHistoryUpdate::Write(second_bytes) =
            update_provenance_history(Some(&first_bytes), &second)
        else {
            panic!("valid provenance history was not appended");
        };
        let second_history: ProvenanceHistory = serde_json::from_slice(&second_bytes).unwrap();
        assert_eq!(second_history.attempts.len(), 2);
        assert_eq!(second_history.attempts[0], first.to_persisted_value());
        assert_eq!(second_history.attempts[1], second.to_persisted_value());

        assert!(matches!(
            update_provenance_history(Some(b"not json"), &second),
            ProvenanceHistoryUpdate::Preserve
        ));
        let next = program_receipt("program-3", "run-7-1");
        let wrong_run = program_receipt("program-3", "run-8-1");
        assert!(matches!(
            update_provenance_history(Some(&second_bytes), &wrong_run),
            ProvenanceHistoryUpdate::Preserve
        ));
        assert_eq!(
            persisted_attempt_sequences(Some(&second_bytes), ExecutionKind::Program),
            Some(vec![1, 2])
        );
        assert_eq!(
            persisted_attempt_sequences(None, ExecutionKind::Program),
            Some(Vec::new())
        );
        assert_eq!(
            persisted_attempt_sequences(Some(&second_bytes), ExecutionKind::Workflow),
            None
        );

        let mut malformed_transient: Value = serde_json::from_slice(&second_bytes).unwrap();
        malformed_transient["attempts"][0]["execution"]["id"] = json!("program-0");
        let malformed_transient = serde_json::to_vec(&malformed_transient).unwrap();
        assert!(matches!(
            update_provenance_history(Some(&malformed_transient), &next),
            ProvenanceHistoryUpdate::Preserve
        ));
        assert_eq!(
            persisted_attempt_sequences(Some(&malformed_transient), ExecutionKind::Program),
            None
        );

        let mut malformed_durable: Value = serde_json::from_slice(&second_bytes).unwrap();
        malformed_durable["attempts"][0]["durable"]["id"] = json!("run-bad-1");
        let malformed_durable = serde_json::to_vec(&malformed_durable).unwrap();
        assert!(matches!(
            update_provenance_history(Some(&malformed_durable), &next),
            ProvenanceHistoryUpdate::Preserve
        ));

        let workflow = workflow_receipt("workflow-9", "wf_7-1");
        let ProvenanceHistoryUpdate::Write(workflow_bytes) =
            update_provenance_history(None, &workflow)
        else {
            panic!("fresh Workflow provenance history was not written");
        };
        assert_eq!(
            persisted_attempt_sequences(Some(&workflow_bytes), ExecutionKind::Workflow),
            Some(vec![9])
        );
    }

    #[test]
    fn provenance_history_stops_at_the_attempt_cap_without_rewriting() {
        let mut bytes = None;
        for sequence in 1..=MAX_PROVENANCE_ATTEMPTS {
            let receipt = program_receipt(&format!("program-{sequence}"), "run-9-1");
            let ProvenanceHistoryUpdate::Write(updated) =
                update_provenance_history(bytes.as_deref(), &receipt)
            else {
                panic!("attempt {sequence} was not persisted");
            };
            assert!(updated.len() <= MAX_PROVENANCE_HISTORY_BYTES);
            bytes = Some(updated);
        }
        let overflow = program_receipt("program-33", "run-9-1");
        assert!(matches!(
            update_provenance_history(bytes.as_deref(), &overflow),
            ProvenanceHistoryUpdate::Preserve
        ));
    }
}
