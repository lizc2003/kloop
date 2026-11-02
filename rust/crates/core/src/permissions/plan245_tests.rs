// plan 245: 沙箱升级与实际执行对齐。

/// 通配升级是“沙箱已经拒绝过”之后的免问，不是执行前跳过沙箱的通行证。
#[tokio::test]
async fn plan245_blanket_escalation_never_skips_the_contained_attempt() {
    let approver =
        ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::WorkspaceSession)]);
    let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
    let opaque = "if true; then echo probe; fi";
    assert_eq!(
        p.escalate_sandbox(opaque, None, 0).await.outcome,
        EscalationOutcome::Approved(EscalationApproval::User)
    );
    assert_eq!(approver.ask_count(), 1);

    // 记录通配之后，任何命令仍先进沙箱；只有被拒后的升级判断才认这条通配。
    assert!(!p.sandbox_escalation_remembered(opaque));
    assert!(!p.sandbox_escalation_remembered("go test ./pkg"));
    assert_eq!(
        p.escalate_sandbox("go test ./pkg", None, 0).await.outcome,
        EscalationOutcome::Approved(EscalationApproval::Remembered)
    );
    assert_eq!(approver.ask_count(), 1, "通配仍免去第二次升级询问");

    // 模型主动 disable_sandbox 是另一个动作，通配不能预先批准它。
    assert!(
        !ok(
            &p,
            "bash",
            json!({"command": "go test ./pkg", "disable_sandbox": true})
        )
        .await
    );
    assert_eq!(approver.ask_count(), 2);
}

/// 可解析但没有任何安全复用范围的命令只提供 Once，不能升级成整个会话的通配。
#[tokio::test]
async fn plan245_unrepresentable_escalation_offers_only_once() {
    let approver =
        ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::WorkspaceSession)]);
    let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
    // 输出目标是根目录下的文件：可解析，但父目录 `/` 不能生成写入范围。
    let command = "make check > /kloop-plan245-root.log";
    assert!(escalation_remember_payload(command, &test_cwd()).is_none());

    assert_eq!(
        p.escalate_sandbox(command, None, 0).await.outcome,
        EscalationOutcome::Declined,
        "前端返回了未提供的会话范围，门必须拒绝它"
    );
    let asked = approver.asked();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].approval_scopes, vec![ApprovalScope::Once]);
    assert_eq!(asked[0].remember_rules, None);
    assert!(!p.sandbox_escalation_remembered(command));
}

/// 解析失败的脚本局部事实可能整体缺失；沙箱自动放行不能替它背书。
#[tokio::test]
async fn plan245_unreliable_shell_facts_are_not_auto_allowed() {
    let approver = ScriptedApprover::new(vec![Decision::Deny]);
    let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
    let command = "rm -rf data/\nif true; then";
    let error = p
        .check_call("bash", &bash(command), 0, /*sandbox_auto_allow*/ true)
        .await
        .expect_err("unreliable facts must reach the human");
    assert!(error.contains("declined"), "{error}");
    assert_eq!(approver.ask_count(), 1);

    // 完整树但严格白名单不认识的控制流仍可靠：沙箱自动放行不被一并关掉。
    let approver = ScriptedApprover::new(vec![]);
    let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
    p.check_call(
        "bash",
        &bash("if true; then echo ok; fi"),
        0,
        /*sandbox_auto_allow*/ true,
    )
    .await
    .unwrap();
    assert_eq!(approver.ask_count(), 0);
}
