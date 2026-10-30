// Plan 244：审批记忆的作用域收窄。

use std::path::Path;
use std::path::PathBuf;

fn plan244_temp(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(crate::resource_id::fresh(label).unwrap());
    std::fs::create_dir_all(&root).unwrap();
    std::fs::canonicalize(&root).unwrap()
}

fn plan244_write(path: impl AsRef<Path>) -> Value {
    json!({"path": path.as_ref(), "content": "x"})
}

/// 首词是包装器、第二词是选项或赋值时，两词前缀描述的是包装器或这一次的参数，
/// 不是命令本身：只记逐字会话记忆，不落持久规则。
#[tokio::test]
async fn plan244_wrapper_shapes_only_remember_the_exact_command() {
    for command in [
        "timeout 600 cargo test",
        "env RUST_LOG=1 cargo run",
        "time cargo build",
        "git -C ../lib status",
        "python3 -c 'print(1)'",
        "./scripts/run --fast",
    ] {
        let approver =
            ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::WorkspaceSession)]);
        let writer = ScriptedWriter::succeeding();
        let p = gate_with_writer(Mode::Manual, approver.clone(), writer.clone());
        assert!(ok(&p, "bash", bash(command)).await, "{command}");
        let asked = approver.asked();
        assert_eq!(asked.len(), 1, "{command}");
        assert_eq!(
            asked[0].approval_scopes,
            vec![ApprovalScope::Once, ApprovalScope::WorkspaceSession],
            "{command}: a verbatim key cannot be written down"
        );
        assert_eq!(
            asked[0].remember_rules,
            Some(vec!["only this exact command text".to_string()]),
            "{command}"
        );

        assert!(ok(&p, "bash", bash(command)).await, "{command}");
        assert_eq!(approver.ask_count(), 1, "{command}: same text is remembered");
        assert!(
            writer.calls.lock().unwrap().is_empty(),
            "{command}: nothing durable"
        );
    }

    let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::WorkspaceSession)]);
    let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
    assert!(ok(&p, "bash", bash("timeout 600 cargo test")).await);
    assert!(
        !ok(&p, "bash", bash("timeout 600 git push --force")).await,
        "the same wrapper prefix is not a grant for a different command"
    );
    assert_eq!(approver.ask_count(), 2);
}

/// 稳定的两词前缀仍照旧落成规则，记忆没有整体退化。
#[tokio::test]
async fn plan244_stable_two_word_prefixes_still_persist() {
    let cwd = test_cwd();
    for (command, rule) in [
        ("git commit -m x", "bash(git commit *)"),
        ("cargo test --workspace", "bash(cargo test *)"),
        ("make check", "bash(make check *)"),
    ] {
        let call = CallFacts::gather("bash", &bash(command), &cwd, None);
        let remember = remember_payload("bash", &call).expect(command);
        assert!(
            remember.rules.contains(&rule.to_string()),
            "{command}: {:?}",
            remember.rules
        );
    }
}

/// 脱沙箱是另一种权限：普通 `bash(...)`、whole-tool `bash` 和普通会话键都不能
/// 替代 `sandbox_escalate(...)`，而升级同意可以覆盖同一条命令。
#[tokio::test]
async fn plan244_disable_sandbox_needs_escalation_consent() {
    let escaping = json!({"command": "make check", "disable_sandbox": true});
    for allow in [vec!["bash(make check *)"], vec!["bash"]] {
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(Mode::Manual, rules(&allow, &[], &[]), approver.clone());
        assert!(!ok(&p, "bash", escaping.clone()).await, "{allow:?}");
        assert_eq!(approver.ask_count(), 1, "{allow:?}");
    }

    let approver = ScriptedApprover::new(vec![]);
    let p = gate(
        Mode::Manual,
        rules(&["sandbox_escalate(make check *)"], &[], &[]),
        approver.clone(),
    );
    assert!(ok(&p, "bash", escaping.clone()).await);
    assert_eq!(approver.ask_count(), 0, "escalation consent covers it");

    // A contained run's session grant is a different key.
    let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::WorkspaceSession)]);
    let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
    assert!(ok(&p, "bash", bash("make check")).await);
    assert!(!ok(&p, "bash", escaping.clone()).await);
    assert_eq!(approver.ask_count(), 2);

    // Remembering an escalation answers the model's own escape too.
    let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::WorkspaceSession)]);
    let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
    assert_eq!(
        p.escalate_sandbox("make check", None, 0).await.outcome,
        EscalationOutcome::Approved(EscalationApproval::User)
    );
    assert!(ok(&p, "bash", escaping.clone()).await);
    assert_eq!(approver.ask_count(), 1);
}

/// 脱沙箱调用选择 Project 时写下的是升级规则，不是“可以运行”的普通规则。
#[tokio::test]
async fn plan244_disable_sandbox_project_memory_is_escalation_vocabulary() {
    let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Project)]);
    let writer = ScriptedWriter::succeeding();
    let p = gate_with_writer(Mode::Manual, approver, writer.clone());
    assert!(
        ok(
            &p,
            "bash",
            json!({"command": "make check", "disable_sandbox": true})
        )
        .await
    );
    assert_eq!(
        writer.calls.lock().unwrap().as_slice(),
        &[vec!["sandbox_escalate(make check *)".to_string()]]
    );
}

/// allow 只看规范目标：原文拼写经过符号链接父目录落到别处时不得放行。
#[cfg(unix)]
#[tokio::test]
async fn plan244_file_allow_does_not_follow_the_alias_spelling() {
    let root = plan244_temp("plan244-alias-");
    let approved = root.join("approved");
    let outside = plan244_temp("plan244-outside-");
    std::fs::create_dir_all(&approved).unwrap();
    std::os::unix::fs::symlink(&outside, approved.join("la")).unwrap();

    let allow = format!("write_file({}/**)", approved.display());
    let approver = ScriptedApprover::new(vec![]);
    let p = Permissions::new(
        Mode::Manual,
        &rules(&[&allow], &[], &[]),
        root.clone(),
        Some(approver.clone()),
    )
    .unwrap();
    assert!(
        !ok(&p, "write_file", plan244_write(approved.join("la/x.txt"))).await,
        "the spelling is inside the grant, the target is not"
    );
    assert_eq!(approver.ask_count(), 1);
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&outside);
}

/// 相对 glob 只匹配 cwd 内的规范相对目标，不能顺手匹配绝对路径。
#[cfg(unix)]
#[tokio::test]
async fn plan244_relative_file_glob_stays_inside_the_cwd() {
    let outside = plan244_temp("plan244-relative-");
    let target = outside.join("x.md");
    let approver = ScriptedApprover::new(vec![]);
    let p = gate(
        Mode::Manual,
        rules(&["write_file(**/*.md)"], &[], &[]),
        approver.clone(),
    );
    assert!(!ok(&p, "write_file", plan244_write(&target)).await);
    assert_eq!(approver.ask_count(), 1);
    let _ = std::fs::remove_dir_all(&outside);
}

/// 目录名里的 glob 元字符不能被当成通配授权；解析不了就不提供任何记忆。
#[cfg(unix)]
#[tokio::test]
async fn plan244_file_memory_refuses_glob_metacharacters() {
    let outside = plan244_temp("plan244-metachar-");
    for relative in ["app/[slug]/x.md", "app/{x/x.md"] {
        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        let target = outside.join(relative);
        assert!(ok(&p, "write_file", plan244_write(&target)).await, "{relative}");
        let asked = approver.asked();
        assert_eq!(
            asked[0].approval_scopes,
            vec![ApprovalScope::Once],
            "{relative}: no session key either"
        );
        assert!(asked[0].remember_rules.is_none(), "{relative}");
        assert!(
            !ok(&p, "write_file", plan244_write(&target)).await,
            "{relative}: asked again"
        );
        assert_eq!(approver.ask_count(), 2, "{relative}");
    }
    let _ = std::fs::remove_dir_all(&outside);
}

/// 根部文件的父目录为空时锚到规范 cwd，不再生成裸 `*`；cwd 本身没有可授权范围。
#[tokio::test]
async fn plan244_file_memory_anchors_root_files_to_the_cwd() {
    let root = plan244_temp("plan244-rootfile-");
    let call = CallFacts::gather(
        "write_file",
        &plan244_write(root.join("check.log")),
        &root,
        None,
    );
    let remember = remember_payload("write_file", &call).expect("a cwd-root file is remember-able");
    assert_eq!(
        remember.rules,
        vec![format!("write_file({}/**)", root.display())]
    );
    assert!(!remember.rules[0].contains("(*)"));

    let directory = CallFacts::gather("write_file", &plan244_write(&root), &root, None);
    assert!(
        remember_payload("write_file", &directory).is_none(),
        "the cwd itself has no parent to grant"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// deny 与危险判定要穿过 wrapper 的选项操作数，不能停在 `{}`、`CI` 这样的操作数上。
#[tokio::test]
async fn plan244_deny_and_hazard_see_through_wrapper_operands() {
    let approver = ScriptedApprover::new(vec![]);
    let p = gate(
        Mode::Manual,
        rules(&[], &["bash(git push *)", "bash(rm *)"], &[]),
        approver.clone(),
    );
    for command in ["env -u CI git push", "xargs -I {} rm -rf {}", "timeout -s KILL 60 rm -rf build"] {
        let error = p.check("bash", &bash(command), 0).await.unwrap_err();
        assert!(error.contains("deny permission rule"), "{command}: {error}");
    }
    assert_eq!(approver.ask_count(), 0);

    let approver = ScriptedApprover::new(vec![
        Decision::Allow(ApprovalScope::Once),
        Decision::Allow(ApprovalScope::Once),
    ]);
    let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
    for command in ["xargs -I {} rm -rf {}", "xargs -z rm -rf build"] {
        assert!(ok(&p, "bash", bash(command)).await, "{command}");
        let asked = approver.asked();
        assert!(
            asked.last().unwrap().description.contains("[destructive]"),
            "{command}: {}",
            asked.last().unwrap().description
        );
    }
}

/// 升级的 Project 持久化失败照普通权限门发 notice，调用本身仍然放行。
#[tokio::test]
async fn plan244_escalation_persistence_failure_is_reported() {
    let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Project)]);
    let writer = ScriptedWriter::failing();
    let p = gate_with_writer(Mode::Manual, approver, writer);
    let escalation = p.escalate_sandbox("make check", None, 0).await;
    assert_eq!(
        escalation.outcome,
        EscalationOutcome::Approved(EscalationApproval::User)
    );
    assert!(
        escalation
            .notice
            .expect("persistence failure must be visible")
            .message
            .contains("not saved")
    );
}
