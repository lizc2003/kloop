//! 全局约束、项目策略与工作区会话状态的持有和发布。

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;

use anyhow::Result;

use super::Approver;
use super::Mode;
use super::ProjectAllowRules;
use super::ProjectPermissionWriter;
use super::ProjectPolicySnapshot;
use super::ProjectPolicyStoreError;
use super::facts::CallFacts;
use super::rule::Rule;
use super::rule::allow_rules_match;
use super::rule::parse_rules;

#[derive(Clone, Copy, Debug)]
pub(super) struct ModeState {
    pub(super) current: Mode,
    pub(super) pre_plan: Mode,
    pub(super) capability_epoch: u64,
}

pub(super) fn advance_epoch(epoch: &mut u64, owner: &str) {
    *epoch = epoch
        .checked_add(1)
        .unwrap_or_else(|| panic!("{owner} capability epoch exhausted"));
}

pub struct GlobalPermissionPolicy {
    pub(super) deny: Vec<Rule>,
    pub(super) ask: Vec<Rule>,
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

    pub(super) fn capability_epoch(&self) -> u64 {
        self.state.read().unwrap().capability_epoch
    }

    pub(super) fn matches_allow(&self, name: &str, call: &CallFacts) -> bool {
        let state = self.state.read().unwrap();
        allow_rules_match(state.snapshot.allow.parsed(), name, call)
    }

    /// Whether durable rules consent to re-running every one of these argvs
    /// outside the sandbox. Strict like `matches_allow`: one uncovered segment
    /// means the whole command still has to be asked about.
    pub(super) fn matches_escalation(&self, argvs: &[Vec<String>]) -> bool {
        if argvs.is_empty() {
            return false;
        }
        let state = self.state.read().unwrap();
        let rules = state.snapshot.allow.parsed();
        argvs
            .iter()
            .all(|argv| rules.iter().any(|rule| rule.matches_escalation_argv(argv)))
    }

    pub(super) fn can_persist(&self) -> bool {
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

    pub(super) async fn persist(
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
pub(super) struct WorkspacePermissionCache {
    pub(super) signatures: HashSet<String>,
    pub(super) capability_epoch: u64,
}

pub struct PermissionSession {
    pub(super) mode: Mutex<ModeState>,
    pub(super) cache: Mutex<HashMap<crate::project::WorkspaceId, WorkspacePermissionCache>>,
    pub(super) approver: Option<Arc<dyn Approver>>,
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

#[cfg(test)]
mod tests {
    use super::super::testutil::ScriptedWriter;
    use super::super::testutil::test_cwd;
    use super::*;

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
}
