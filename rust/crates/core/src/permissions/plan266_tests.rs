// plan 266: 权限门一次判定执行面，执行层照单执行。

#[cfg(target_os = "macos")]
use crate::sandbox::ExecutionContext;
#[cfg(target_os = "macos")]
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
    let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Project)]);
    let p = gate_with_writer(Mode::Manual, approver.clone(), ScriptedWriter::succeeding());
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
}

/// 需要沙箱本身保护的字面正文由门判出 `Required`，并在沙箱内运行。
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

    // 这类正文都是 heredoc，解析为 opaque：只有会话通配能记住它的升级，而通配
    // 从不在运行前跳过沙箱（plan 245），所以没有记忆能把它判成 Escalated。
    // 门里 Required 且不在沙箱内就拒绝的那一支仍保留，作为防御。
    assert!(!p.sandbox_escalation_remembered(command));

    let _ = std::fs::remove_dir_all(home);
}
