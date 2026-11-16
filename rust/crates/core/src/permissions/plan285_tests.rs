// plan 285：bypass 下 bash 不进 OS 沙箱；前置硬限制与正文凭据探针的沙箱保护照旧。

#[cfg(target_os = "macos")]
fn plan285_policy(label: &str) -> (PathBuf, SandboxPolicy) {
    let home = std::env::temp_dir().join(crate::resource_id::fresh(label).unwrap());
    std::fs::create_dir_all(&home).unwrap();
    let policy = SandboxPolicy::workspace(&home, &[], false).with_credential_roots(&home);
    (home, policy)
}

/// 有真实沙箱策略时，bypass 的普通命令（前台与后台）都不进沙箱，也不带授权；
/// manual 下同一条命令照旧在沙箱内。
#[cfg(target_os = "macos")]
#[tokio::test]
async fn plan285_bypass_runs_bash_uncontained() {
    let (home, policy) = plan285_policy("plan285-uncontained-");
    let shell_home = home.to_str().unwrap();
    let execution =
        || ExecutionContext::sandboxed(Some(&policy)).with_shell_home(Some(shell_home));
    for input in [
        bash("git ls-remote origin"),
        bash("security find-generic-password -s example -w"),
        bash("mysql -h db.internal -u app -e \"SELECT 1\""),
        json!({"command": "go test ./...", "background": true}),
    ] {
        for (mode, sandbox) in [
            (Mode::Bypass, SandboxPlan::None),
            (Mode::Manual, SandboxPlan::Contained),
        ] {
            let approver = ScriptedApprover::new(vec![]);
            let p = gate(mode, rules(&[], &[], &[]), approver.clone());
            let verdict = p
                .check_call_with_execution("bash", &input, 0, execution(), None)
                .await
                .unwrap_or_else(|error| panic!("{mode:?} {input}: {error}"));
            assert_eq!(
                verdict,
                Verdict {
                    sandbox,
                    protection: Protection::Clear,
                    notice: None,
                    grants: SandboxGrants::default(),
                },
                "{mode:?} {input}"
            );
            assert_eq!(approver.ask_count(), 0, "{mode:?} {input}");
        }
    }
    let _ = std::fs::remove_dir_all(home);
}

/// 不进沙箱不等于没有限制：deny 规则与明确拼出的凭据读取照旧硬拒；字面正文
/// 点名凭据路径的调用只有沙箱能保护，bypass 下也留在沙箱里。
#[cfg(target_os = "macos")]
#[tokio::test]
async fn plan285_bypass_keeps_hard_limits_and_literal_body_protection() {
    let (home, policy) = plan285_policy("plan285-limits-");
    let shell_home = home.to_str().unwrap();
    let execution =
        || ExecutionContext::sandboxed(Some(&policy)).with_shell_home(Some(shell_home));
    let p = gate(
        Mode::Bypass,
        rules(&[], &["bash(git push *)"], &[]),
        ScriptedApprover::new(vec![]),
    );
    for (command, error) in [
        ("git push origin main", "deny permission rule"),
        ("cat ~/.ssh/id_rsa", "sensitive path"),
        ("cat .env", "sensitive path"),
    ] {
        let refusal = p
            .check_call_with_execution("bash", &bash(command), 0, execution(), None)
            .await
            .expect_err(command);
        assert!(refusal.contains(error), "{command}: {refusal}");
    }

    let command = "cat > out.txt <<'EOF'\n~/.ssh/id_rsa\nEOF\ncargo run";
    let verdict = p
        .check_call_with_execution("bash", &bash(command), 0, execution(), None)
        .await
        .unwrap();
    assert_eq!(verdict.sandbox, SandboxPlan::Contained);
    assert_eq!(verdict.protection, Protection::Required);
    let _ = std::fs::remove_dir_all(home);
}
