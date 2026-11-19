//! 按能力升级（plan 283 阶段 2b）：沙箱拒绝了联网或某处写入时，问的是“把沙箱放宽这一点”，
//! 不是“把沙箱拿掉”。联网可以只给这一次、给本会话所有命令、或按程序记到项目；写只按具体
//! 目录，只给这一次或记到项目。重跑仍在沙箱里，其余限制（别处的写、凭据、受保护子路径）不变。

use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;

use crate::path_resolve::resolve_nearest_existing;

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

/// Whether bash may be offered writes to `dir`: a rule can name it (see
/// [`nameable_dir`]) and granting it is safe (see [`safe_to_grant`]). The
/// sandbox's own write denies still apply on top of any grant.
pub(crate) fn grantable_write_dir(dir: &Path, home: Option<&Path>) -> bool {
    nameable_dir(dir) && safe_to_grant(dir, home)
}

/// 能写成 `sandbox_write(<dir>)` 规则：UTF-8 的绝对路径，不含 glob 字符和换行。
fn nameable_dir(dir: &Path) -> bool {
    dir.is_absolute()
        && dir
            .to_str()
            .is_some_and(|text| !text.contains(['*', '?', '[', ']', '{', '}', '\n', '\r']))
}

/// 授权它不会把不该放的放出去：原拼写和解析后的落点都要过，因为授权时 `with_grants` 两种都会加进
/// 可写根（复审 P2：只解析家目录、不解析候选目录时，指向家目录或 `/` 的链接能混过去）。每种形式都
/// 不是 `/`，不是家目录或其祖先（家目录同样按原拼写和解析后两种比，`$HOME` 可能经过符号链接），也
/// 不碰权限层视为敏感的目录（`.git`、`.kloop`、凭据目录）。解析失败（断链、权限错误）不授权。
fn safe_to_grant(dir: &Path, home: Option<&Path>) -> bool {
    let Some(resolved) = resolve_nearest_existing(dir) else {
        return false;
    };
    let homes: Vec<PathBuf> = home
        .into_iter()
        .flat_map(|home| std::iter::once(home.to_path_buf()).chain(resolve_nearest_existing(home)))
        .collect();
    [dir, resolved.as_path()].into_iter().all(|form| {
        form.parent().is_some()
            && !homes.iter().any(|home| home.starts_with(form))
            && PathFacts::gather_with_resolved(
                &form.join("kloop-grant-probe"),
                Path::new("/"),
                None,
            )
            .sensitivity(Access::Write)
            .is_none()
    })
}

/// 被拒写入要提议授权的目录（plan 287）：落在已知的工具缓存里时提议整个缓存根，下一个依赖、
/// 下一份遥测不必再问；其它情况提议被拒路径的父目录。上提之前先看原始落点：被拒路径本身或它的
/// 父目录已被策略决定（禁写路径、受保护子路径、已在可写根里），或父目录授权出去不安全时，什么都
/// 不提议，授权哪一级都解除不了这次拒绝（两轮复审 P2）。父目录只是写不成规则（名字里有 glob 字符）
/// 时仍可由缓存根覆盖（复审 P3）。缓存根同样要能授权、策略没有决定它，否则退回父目录，不扩大。
/// `decided` 是策略的 `already_decides_writes`；`var` 读 kloop 自己的环境（bash 子进程继承它），
/// 测试里换成固定值。
pub(crate) fn proposed_write_dir(
    path: &Path,
    home: Option<&Path>,
    var: impl Fn(&str) -> Option<OsString>,
    decided: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let parent = path.parent()?;
    if decided(path) || decided(parent) || !safe_to_grant(parent, home) {
        return None;
    }
    let cache_root = home.and_then(|home| {
        tool_cache_roots(home, &var).into_iter().find(|root| {
            path.starts_with(root)
                && path != root
                && grantable_write_dir(root, Some(home))
                && !decided(root)
        })
    });
    cache_root.or_else(|| nameable_dir(parent).then(|| parent.to_path_buf()))
}

/// 包管理器按包分目录的缓存根。每个根给出原拼写和规范化后的两种形式：日志里的被拒路径是真实
/// 路径，家目录或缓存目录可能经过符号链接。
fn tool_cache_roots(home: &Path, var: &impl Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    let absolute = |name: &str| {
        var(name)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    let gopath = var("GOPATH")
        .and_then(|value| std::env::split_paths(&value).next())
        .filter(|path| path.is_absolute());
    let go_modules = absolute("GOMODCACHE").unwrap_or_else(|| {
        gopath
            .unwrap_or_else(|| home.join("go"))
            .join("pkg")
            .join("mod")
    });
    let user_config = if cfg!(target_os = "macos") {
        home.join("Library").join("Application Support")
    } else {
        absolute("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"))
    };
    let cargo = absolute("CARGO_HOME").unwrap_or_else(|| home.join(".cargo"));
    let gradle = absolute("GRADLE_USER_HOME").unwrap_or_else(|| home.join(".gradle"));
    [
        go_modules,
        user_config.join("go").join("telemetry"),
        absolute("npm_config_cache").unwrap_or_else(|| home.join(".npm")),
        cargo.join("registry"),
        cargo.join("git"),
        home.join(".m2").join("repository"),
        gradle.join("caches"),
    ]
    .into_iter()
    .flat_map(|root| {
        let resolved = resolve_nearest_existing(&root).filter(|resolved| *resolved != root);
        std::iter::once(root).chain(resolved)
    })
    .collect()
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
