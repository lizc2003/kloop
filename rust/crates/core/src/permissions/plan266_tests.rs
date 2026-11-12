// plan 266: 权限门一次判定执行面，执行层照单执行。

use crate::sandbox::ExecutionContext;
use crate::sandbox::SandboxPolicy;

/// 沙箱进不进由门决定：普通调用在内，主动脱沙箱与已记住的升级在外，后台
/// 永远在内（它没有拒绝后重跑的循环）。
#[cfg(target_os = "macos")]
#[tokio::test]
async fn plan266_containment_is_decided_once() {
    let policy = SandboxPolicy::workspace(&test_cwd(), &[], false);
    let execution = || ExecutionContext::sandboxed(Some(&policy));

    let p = gate(
        Mode::Manual,
        rules(&[], &[], &[]),
        ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]),
    );
    let verdict = p
        .check_call_with_execution("bash", &bash("go test ./pkg"), 0, execution(), None)
        .await
        .unwrap();
    assert_eq!(verdict.sandbox, SandboxPlan::Contained);

    // 沙箱盲的门没有可执行的沙箱上下文，也就不能假装包含它。
    let blind = p
        .check_call_with_execution(
            "bash",
            &bash("go test ./pkg"),
            0,
            ExecutionContext::approval_only(false),
            None,
        )
        .await
        .unwrap();
    assert_eq!(blind.sandbox, SandboxPlan::None);

    // 已记住的逐命令升级：这次调用直接跳过沙箱内的尝试。
    let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::WorkspaceSession)]);
    let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
    assert_eq!(
        p.escalate_sandbox("go test ./pkg", None, 0).await.outcome,
        EscalationOutcome::Approved(EscalationApproval::User)
    );
    assert!(
        p.sandbox_escalation_remembered("go test ./pkg"),
        "the escalation must be on record"
    );
    let verdict = p
        .check_call_with_execution("bash", &bash("go test ./pkg"), 0, execution(), None)
        .await
        .unwrap();
    assert_eq!(verdict.sandbox, SandboxPlan::Escalated);

    // 同一份记忆不会把后台运行带出沙箱。
    let verdict = p
        .check_call_with_execution(
            "bash",
            &json!({"command": "go test ./pkg", "background": true}),
            0,
            execution(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(verdict.sandbox, SandboxPlan::Contained);

    // 主动脱沙箱仍是显式选择，无论记住了什么。
    let verdict = p
        .check_call_with_execution(
            "bash",
            &json!({"command": "go test ./pkg", "disable_sandbox": true}),
            0,
            execution(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(verdict.sandbox, SandboxPlan::None);
}

/// 需要沙箱本身保护的字面正文不能被“已记住的升级”带出沙箱：门判出
/// `Required` 后直接拒绝，而不是留给执行层在启动前才发现。
#[cfg(target_os = "macos")]
#[tokio::test]
async fn plan266_required_protection_never_leaves_the_sandbox() {
    let home = std::env::temp_dir().join("kloop-plan266-protection-home");
    std::fs::create_dir_all(&home).unwrap();
    let shell_home = home.to_str().unwrap();
    let policy = SandboxPolicy::workspace(&home, &[], false).with_credential_roots(&home);
    let execution =
        || ExecutionContext::sandboxed(Some(&policy)).with_shell_home(Some(shell_home));
    let p = gate(Mode::Manual, rules(&[], &[], &[]), ScriptedApprover::new(vec![]));
    let command = "cat > out.txt <<'EOF'\n~/.ssh/id_rsa\nEOF";

    let verdict = p
        .check_call_with_execution("bash", &bash(command), 0, execution(), None)
        .await
        .unwrap();
    assert_eq!(verdict.protection, Protection::Required);
    assert_eq!(verdict.sandbox, SandboxPlan::Contained);

    // 模型主动脱沙箱也不能带走受保护的字面正文；这条在询问之前就拒绝。
    let error = p
        .check_call_with_execution(
            "bash",
            &json!({"command": command, "disable_sandbox": true}),
            0,
            execution(),
            None,
        )
        .await
        .expect_err("required protection cannot leave the sandbox");
    assert!(
        error.contains("requires credential read/write protection"),
        "{error}"
    );

    let _ = std::fs::remove_dir_all(home);
}
