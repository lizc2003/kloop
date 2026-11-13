//! 按能力升级（plan 283 阶段 2b）：沙箱拒绝了联网或某处写入时，问的是“把沙箱放宽这一点”，
//! 不是“把沙箱拿掉”。联网可以只给这一次、给本会话所有命令、或按程序记到项目；写只按具体
//! 目录，只给这一次或记到项目。重跑仍在沙箱里，其余限制（别处的写、凭据、受保护子路径）不变。

use std::path::Path;
use std::path::PathBuf;

use super::ApprovalAnswer;
use super::ApprovalDoor;
use super::ApprovalRecord;
use super::ApprovalScope;
use super::AskReason;
use super::ConfirmRequest;
use super::Decision;
use super::Escalation;
use super::EscalationApproval;
use super::EscalationOutcome;
use super::MemorySource;
use super::Mode;
use super::PROJECT_RULE_NOT_SAVED;
use super::Permissions;
use super::ProjectAllowRules;
use super::SESSION_NETWORK_SIGNATURE;
use super::SandboxGrants;
use super::describe::clip;
use super::describe::join_notices;
use super::describe::sub_agent_notice;
use super::describe::tool_title;
use super::facts::Access;
use super::facts::PathFacts;

/// What a capability escalation offers to widen.
pub(crate) enum CapabilityRequest {
    /// The network. `programs` are what a project-scoped answer remembers
    /// (`sandbox_network(<program>)`); `None` when the command cannot be keyed
    /// on its programs, so the project scope is not offered.
    Network { programs: Option<Vec<String>> },
    /// These directories, writable for bash.
    Write { dirs: Vec<PathBuf> },
}

impl CapabilityRequest {
    fn grants(&self) -> SandboxGrants {
        match self {
            CapabilityRequest::Network { .. } => SandboxGrants {
                network: true,
                writable: Vec::new(),
            },
            CapabilityRequest::Write { dirs } => SandboxGrants {
                network: false,
                writable: dirs.clone(),
            },
        }
    }

    fn rules(&self) -> Vec<String> {
        match self {
            CapabilityRequest::Network { programs } => programs
                .iter()
                .flatten()
                .map(|program| format!("sandbox_network({program})"))
                .collect(),
            CapabilityRequest::Write { dirs } => dirs
                .iter()
                .map(|dir| format!("sandbox_write({})", dir.display()))
                .collect(),
        }
    }

    fn what(&self) -> String {
        match self {
            CapabilityRequest::Network { .. } => {
                "let it reach hosts outside this machine? File writes stay limited as before."
                    .to_string()
            }
            CapabilityRequest::Write { dirs } => format!(
                "let bash write {}? Everything else stays contained.",
                dirs.iter()
                    .map(|dir| dir.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

/// Whether bash may be offered writes to `dir`: an absolute directory below
/// the filesystem root, not the home directory itself, spelled without glob
/// characters (a rule could not name it), and naming nothing the permission
/// layer treats as sensitive — `.git`, `.kloop`, credential directories. The
/// sandbox's own write denies still apply on top of any grant.
pub(crate) fn grantable_write_dir(dir: &Path, home: Option<&Path>) -> bool {
    let Some(text) = dir.to_str() else {
        return false;
    };
    dir.is_absolute()
        && dir.parent().is_some()
        && home.is_none_or(|home| dir != home)
        && !text.contains(['*', '?', '[', ']', '{', '}', '\n', '\r'])
        && PathFacts::gather_with_resolved(&dir.join("kloop-grant-probe"), Path::new("/"), None)
            .sensitivity(Access::Write)
            .is_none()
}

impl Permissions {
    /// Whether a consent already on record covers this widening now: the
    /// session's network, the project's rules for these programs, or project
    /// directories containing every requested one.
    fn capability_remembered(&self, request: &CapabilityRequest) -> Option<MemorySource> {
        match request {
            CapabilityRequest::Network { programs } => {
                if self.session_network_granted() {
                    return Some(MemorySource::Session);
                }
                programs
                    .as_ref()
                    .filter(|programs| self.project.network_programs_covered(programs))
                    .map(|_| MemorySource::Rule)
            }
            CapabilityRequest::Write { dirs } => {
                let granted = self.project.sandbox_write_dirs();
                dirs.iter()
                    .all(|dir| granted.iter().any(|root| dir.starts_with(root)))
                    .then_some(MemorySource::Rule)
            }
        }
    }

    pub(crate) async fn escalate_capability(
        &self,
        command: &str,
        denial: &crate::sandbox::SandboxDenial,
        request: &CapabilityRequest,
        depth: u8,
    ) -> Escalation {
        if self.allow_everything {
            return Escalation::new(EscalationOutcome::NotAttempted);
        }
        let grants = request.grants();
        let network = matches!(request, CapabilityRequest::Network { .. });
        let rules = request.rules();
        let mut record = ApprovalRecord {
            door: ApprovalDoor::Escalation,
            tool: "bash".to_string(),
            reason: AskReason::SandboxDenied {
                denial: denial.kind.label().to_string(),
            },
            detail: clip(command),
            keys: if network {
                vec![SESSION_NETWORK_SIGNATURE.to_string()]
            } else {
                Vec::new()
            },
            rules: rules.clone(),
            offered: Vec::new(),
            answer: ApprovalAnswer::NoApprover,
        };
        // The verdict was made before the run; a concurrent call (another
        // sub-agent) may have been granted the same capability since.
        if let Some(by) = self.capability_remembered(request) {
            record.answer = ApprovalAnswer::Remembered { by };
            return Escalation::new(EscalationOutcome::Approved(EscalationApproval::Remembered))
                .granting(grants)
                .recorded(record);
        }
        if self.mode() == Mode::Bypass {
            return Escalation::new(EscalationOutcome::Approved(EscalationApproval::Bypass))
                .granting(grants);
        }
        let Some(approver) = &self.session.approver else {
            return Escalation::new(EscalationOutcome::NotAttempted).recorded(record);
        };
        let mut scopes = vec![ApprovalScope::Once];
        let mut echo = Vec::new();
        if network {
            scopes.push(ApprovalScope::WorkspaceSession);
            echo.push("this session: every bash command may reach the network".to_string());
        }
        let persistable = !rules.is_empty()
            && self.project.can_persist()
            && ProjectAllowRules::parse(&rules).is_ok();
        if persistable {
            scopes.push(ApprovalScope::Project);
            echo.push(format!("this project: {}", rules.join(", ")));
        }
        let ask = if network {
            "sandbox blocked the network — allow it?"
        } else {
            "sandbox blocked a write — allow that directory?"
        };
        let agent = if depth > 0 { "[sub-agent] " } else { "" };
        let req = ConfirmRequest {
            description: format!("{agent}[{ask}] bash: {}", clip(command)),
            title: Some(tool_title("bash").to_string()),
            detail: Some(clip(command)),
            notice: Some(join_notices(
                sub_agent_notice(depth),
                &format!(
                    "the OS sandbox blocked this ({}): {} — {}",
                    denial.kind.label(),
                    denial.evidence,
                    request.what()
                ),
            )),
            approval_scopes: scopes.clone(),
            remember_rules: (!echo.is_empty()).then_some(echo),
            preview: None,
        };
        record.offered = scopes.clone();
        record.answer = ApprovalAnswer::Denied;
        let scope = match approver.confirm(req).await {
            Decision::Allow(scope) if scopes.contains(&scope) => scope,
            Decision::Allow(_) | Decision::Deny => {
                return Escalation::new(EscalationOutcome::Declined).recorded(record);
            }
            Decision::Stop => {
                record.answer = ApprovalAnswer::Stopped;
                return Escalation::new(EscalationOutcome::Stopped).recorded(record);
            }
        };
        record.answer = ApprovalAnswer::Allowed { scope };
        let approved =
            Escalation::new(EscalationOutcome::Approved(EscalationApproval::User)).granting(grants);
        match scope {
            ApprovalScope::Once => approved.recorded(record),
            ApprovalScope::WorkspaceSession => {
                self.remember_in_session([SESSION_NETWORK_SIGNATURE.to_string()]);
                approved.recorded(record)
            }
            ApprovalScope::Project => {
                let saved = match ProjectAllowRules::parse(&rules) {
                    Ok(additions) => self.project.persist(additions).await.is_ok(),
                    Err(_) => false,
                };
                let approved = if saved {
                    approved
                } else {
                    Escalation {
                        notice: Some(super::PermissionNotice {
                            message: PROJECT_RULE_NOT_SAVED.into(),
                        }),
                        ..approved
                    }
                };
                approved.recorded(record)
            }
        }
    }
}
