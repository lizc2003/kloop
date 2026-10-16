//! 策略与管线测试共用的项目存储夹具。

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;

use super::ProjectAllowRules;
use super::ProjectPermissionWriter;
use super::ProjectPolicySnapshot;
use super::ProjectPolicyStoreError;

pub(super) struct ScriptedWriter {
    snapshot: Mutex<ProjectPolicySnapshot>,
    fail: bool,
    pub(super) calls: Mutex<Vec<Vec<String>>>,
}

impl ScriptedWriter {
    pub(super) fn succeeding() -> Arc<Self> {
        Arc::new(Self {
            snapshot: Mutex::new(ProjectPolicySnapshot::empty()),
            fail: false,
            calls: Mutex::new(Vec::new()),
        })
    }

    pub(super) fn failing() -> Arc<Self> {
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
            dyn Future<Output = std::result::Result<ProjectPolicySnapshot, ProjectPolicyStoreError>>
                + Send
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

pub(super) fn test_cwd() -> PathBuf {
    std::env::current_dir().expect("test process has a current directory")
}
