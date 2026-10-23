#[tokio::test]
async fn plan239_partial_risks_reach_the_real_gate() {
    for mode in [Mode::Manual, Mode::Plan, Mode::Bypass] {
        for auto in [false, true] {
            for command in [
                "echo $VALUE; cat .env",
                "if true; then cat .git/config; fi",
                "rg \"$PATTERN\" --file .env",
                "cat $FILES .env",
                "cat < .env; echo $VALUE",
                "cd elsewhere; cat .env",
                "sh -c 'echo $VALUE; cat .env'",
                "env A=1 sh -c 'echo $VALUE; cat .env'",
            ] {
                let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
                let p = gate(mode, rules(&[], &[], &[]), approver.clone());
                let error = p
                    .check_call("bash", &bash(command), 0, auto)
                    .await
                    .expect_err(command);
                assert!(error.contains("sensitive path"), "{command}: {error}");
                assert_eq!(approver.ask_count(), 0);
            }
            for command in [
                "echo $V; git push origin",
                "git push $REMOTE",
                "if true; then sudo git push origin; fi",
            ] {
                let approver = ScriptedApprover::new(vec![]);
                let p = gate(
                    mode,
                    rules(&[], &["bash(git push *)"], &[]),
                    approver.clone(),
                );
                let error = p
                    .check_call("bash", &bash(command), 0, auto)
                    .await
                    .expect_err(command);
                assert!(error.contains("deny permission rule"), "{command}: {error}");
                assert_eq!(approver.ask_count(), 0);
            }
        }
    }
    for command in [
        "echo $V; rm -rf build",
        "rm $TARGET -rf build",
        "echo $V > /dev/disk0",
        "cd elsewhere; echo $V > .env",
    ] {
        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        p.check_call("bash", &bash(command), 0, true).await.unwrap();
        assert_eq!(approver.ask_count(), 1, "{command}");
        assert!(approver.asked()[0].notice.is_some(), "{command}");
    }
}

#[tokio::test]
async fn plan239_unknown_arguments_cannot_be_deleted_to_match_rules() {
    let approver = ScriptedApprover::new(vec![]);
    let p = gate(
        Mode::Bypass,
        rules(&[], &["bash(git push *)"], &[]),
        approver,
    );
    assert!(
        p.check_call("bash", &bash("git $ACTION push origin"), 0, true)
            .await
            .is_ok()
    );
    let approver = ScriptedApprover::new(vec![Decision::Deny]);
    let p = gate(
        Mode::Manual,
        rules(&["bash", "bash(git push *)"], &[], &[]),
        approver.clone(),
    );
    assert!(
        p.check_call("bash", &bash("git push $REMOTE"), 0, false)
            .await
            .is_err()
    );
    assert_eq!(approver.ask_count(), 1);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn plan239_protected_literal_gate_keeps_opaque_consent() {
    use crate::sandbox::{ExecutionContext, SandboxPolicy};
    let home = std::env::temp_dir().join(crate::resource_id::fresh("plan239-home-").unwrap());
    std::fs::create_dir_all(&home).unwrap();
    let policy = SandboxPolicy::workspace(&home, &[], false).with_credential_roots(&home);
    let command = "cat > /tmp/probe.rs <<'EOF'\nlet path = \"~/.ssh/id_rsa\"; // 中文 $() `data`\nEOF\ncargo run";
    assert!(!crate::tools::Builtin::Bash.concurrency_safe(&bash(command)));
    for auto in [false, true] {
        for mode in [Mode::Manual, Mode::Plan, Mode::Bypass] {
            let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
            let p = gate(mode, rules(&[], &[], &[]), approver.clone());
            let mut policy = policy.clone();
            policy.auto_allow = auto;
            let result = p
                .check_call_with_execution(
                    "bash",
                    &bash(command),
                    0,
                    ExecutionContext::sandboxed(Some(&policy)),
                )
                .await;
            assert_eq!(
                result.is_ok(),
                mode != Mode::Plan,
                "{mode:?}, auto={auto}: {result:?}"
            );
            assert_eq!(
                approver.ask_count(),
                usize::from(mode == Mode::Manual && !auto)
            );
        }
    }
    let approver = ScriptedApprover::new(vec![
        Decision::Allow(ApprovalScope::WorkspaceSession),
        Decision::Deny,
    ]);
    let writer = ScriptedWriter::succeeding();
    let p = gate_with_writer(Mode::Manual, approver.clone(), writer.clone());
    let mut policy = policy.clone();
    policy.auto_allow = false;
    for _ in 0..2 {
        p.check_call_with_execution(
            "bash",
            &bash(command),
            0,
            ExecutionContext::sandboxed(Some(&policy)),
        )
        .await
        .unwrap();
    }
    assert_eq!(approver.ask_count(), 1);
    assert_eq!(
        approver.asked()[0].approval_scopes,
        vec![ApprovalScope::Once, ApprovalScope::WorkspaceSession]
    );
    assert!(writer.calls.lock().unwrap().is_empty());
    assert!(
        p.check_call_with_execution(
            "bash",
            &bash(&command.replace("id_rsa", "other")),
            0,
            ExecutionContext::sandboxed(Some(&policy))
        )
        .await
        .is_err()
    );
    assert_eq!(approver.ask_count(), 2);
    assert!(
        p.check_call("bash", &bash(command), 0, true)
            .await
            .unwrap_err()
            .contains("sensitive path")
    );
    assert!(
        p.check_call_with_execution("bash", &bash(command), 0, ExecutionContext::default())
            .await
            .is_err()
    );
    std::fs::remove_dir_all(home).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn plan239_literal_exemption_checks_every_needle_and_actual_policy() {
    use crate::sandbox::{ExecutionContext, SandboxPolicy};
    let home = std::env::temp_dir().join(crate::resource_id::fresh("plan239-policy-").unwrap());
    std::fs::create_dir_all(&home).unwrap();
    let policy = SandboxPolicy::workspace(&home, &[], false).with_credential_roots(&home);
    let approver = ScriptedApprover::new(vec![]);
    let p = gate(Mode::Bypass, rules(&[], &[], &[]), approver.clone());
    let check = |body: &str| bash(&format!("cat > /tmp/probe <<'EOF'\n{body}\nEOF"));
    for body in [
        "~/.ssh/id_rsa ~/.aws/credentials",
        &format!("{}/.gnupg/key", home.display()),
    ] {
        p.check_call_with_execution(
            "bash",
            &check(body),
            0,
            ExecutionContext::sandboxed(Some(&policy)),
        )
        .await
        .unwrap();
    }
    for body in [
        "~/.ssh/id ~/.kloop/config.toml",
        "~/.ssh/id /another/.ssh/id",
        "./.ssh/id",
        "~/.ssh/../outside/.ssh/id",
        "$HOME/.ssh/id",
    ] {
        assert!(
            p.check_call_with_execution(
                "bash",
                &check(body),
                0,
                ExecutionContext::sandboxed(Some(&policy))
            )
            .await
            .is_err(),
            "{body}"
        );
    }
    let input = check("~/.ssh/id");
    for command in [
        "cat > /tmp/probe <<EOF\n~/.ssh/id\nEOF",
        "python3 > /tmp/probe <<'EOF'\n~/.ssh/id\nEOF",
        "cat > /tmp/probe <<'EOF' | sh\n~/.ssh/id\nEOF",
        "cat > /tmp/probe 2>&1 <<'EOF'\n~/.ssh/id\nEOF",
        "cat <<<'~/.ssh/id' > /tmp/probe",
        "cat > /tmp/probe <<'EOF'\n~/.ssh/id",
    ] {
        assert!(
            p.check_call_with_execution(
                "bash",
                &bash(command),
                0,
                ExecutionContext::sandboxed(Some(&policy))
            )
            .await
            .is_err(),
            "{command}"
        );
    }
    let mut no_read = policy.clone();
    no_read.denied_read_paths.clear();
    let mut no_write = policy.clone();
    no_write.denied_write_paths.clear();
    let mut no_claim = policy.clone();
    no_claim.credential_home = None;
    for policy in [
        no_read,
        no_write,
        no_claim,
        policy.with_allowed_read_path(&home.join(".ssh")),
        policy.with_allowed_read_path(&home.join(".SSH")),
        policy.with_allowed_read_path(&home.join(".ssh/nested")),
        policy.with_allowed_read_path(&home),
    ] {
        assert!(
            p.check_call_with_execution(
                "bash",
                &input,
                0,
                ExecutionContext::sandboxed(Some(&policy))
            )
            .await
            .is_err()
        );
    }
    for suffix in [
        "cat .env",
        "cat ~/.aws/credentials",
        "cat /tmp/file > /dev/disk0",
        "rm -rf build",
    ] {
        let input = bash(&format!("{}\n{suffix}", input["command"].as_str().unwrap()));
        let approver = ScriptedApprover::new(vec![Decision::Deny]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        assert!(
            p.check_call_with_execution(
                "bash",
                &input,
                0,
                ExecutionContext::sandboxed(Some(&policy))
            )
            .await
            .is_err(),
            "{suffix}"
        );
        assert_eq!(
            approver.ask_count(),
            usize::from(suffix.contains("/dev/") || suffix.starts_with("rm")),
            "{suffix}"
        );
    }
    for ruleset in [
        rules(&[], &["bash(cat *)"], &[]),
        rules(&[], &[], &["bash(cat *)"]),
    ] {
        let approver = ScriptedApprover::new(vec![Decision::Deny]);
        let denied = !ruleset.deny.is_empty();
        let p = gate(Mode::Manual, ruleset, approver.clone());
        assert!(
            p.check_call_with_execution(
                "bash",
                &input,
                0,
                ExecutionContext::sandboxed(Some(&policy))
            )
            .await
            .is_err()
        );
        assert_eq!(approver.ask_count(), usize::from(!denied));
    }
    std::fs::remove_dir_all(home).unwrap();
}
