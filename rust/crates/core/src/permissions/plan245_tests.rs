// plan 245: 沙箱升级与实际执行对齐。

/// 可解析但没有任何安全复用范围的命令只提供 Once，不能升级成整个会话的通配。
#[tokio::test]
async fn plan245_unrepresentable_escalation_offers_only_once() {
    let approver =
        ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::WorkspaceSession)]);
    let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
    // 输出目标是根目录下的文件：可解析，但父目录 `/` 不能生成写入范围。
    let command = "make check > /kloop-plan245-root.log";
    assert!(escalation_remember_payload(command, &test_cwd(), None).is_none());

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

/// 断链叶子没有可信物理落点，但 deny/ask 仍须保留最近祖先的保守别名：
/// 绝对别名路径穿过指向 cwd 的符号链接时，相对 `bash_write(logs/**)` 不能漏。
#[cfg(unix)]
#[tokio::test]
async fn plan245_dangling_leaf_alias_keeps_deny_evidence_only() {
    let base = std::env::temp_dir()
        .join(crate::resource_id::fresh("kloop-plan245-alias-").expect("fresh id"));
    let repo = base.join("repo");
    let alias = base.join("alias");
    std::fs::create_dir_all(repo.join("logs")).unwrap();
    std::fs::create_dir_all(&alias).unwrap();
    std::fs::remove_dir(&alias).unwrap();
    std::os::unix::fs::symlink(&repo, &alias).unwrap();
    // `logs/out` 是指向尚不存在目标的断链，canonicalize 不能解析它。
    std::os::unix::fs::symlink(repo.join("new.txt"), repo.join("logs/out")).unwrap();
    let repo = std::fs::canonicalize(repo).unwrap();
    let command = format!("printf x > {}/logs/out", alias.display());

    let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
    let p = Permissions::new(
        Mode::Manual,
        &rules(&[], &["bash_write(logs/**)"], &[]),
        repo.clone(),
        Some(approver.clone()),
    )
    .unwrap();
    let error = p
        .check_call("bash", &bash(&command), 0, /*sandbox_auto_allow*/ true)
        .await
        .expect_err("the relative deny must see the alias parent");
    assert!(error.contains("deny permission rule"), "{error}");
    assert_eq!(approver.ask_count(), 0);

    // allow 方向不消费这条保守别名：没有 deny 时仍要询问。
    let approver = ScriptedApprover::new(vec![Decision::Deny]);
    let p = Permissions::new(
        Mode::Manual,
        &rules(&["bash_write(logs/**)"], &[], &[]),
        repo.clone(),
        Some(approver.clone()),
    )
    .unwrap();
    let error = p
        .check_call("bash", &bash(&command), 0, /*sandbox_auto_allow*/ false)
        .await
        .expect_err("an unresolved target must not match allow");
    assert!(error.contains("declined"), "{error}");
    assert_eq!(approver.ask_count(), 1);
    std::fs::remove_dir_all(&base).unwrap();
}
