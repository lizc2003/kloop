// plan 284：读取目标含变量的命令不再一律硬拒。明确拼出的敏感路径、同一条命令里
// 写出的凭据文件名照旧硬拒。

/// 被误拦的日常写法：变量只是普通目标或数据，沙箱里直接跑，不问也不拒。
#[tokio::test]
async fn plan284_dynamic_targets_run_like_literal_ones() {
    for command in [
        "DB=app_test_$(date +%s) && mysql --protocol=tcp -h 127.0.0.1 -u root \
         -e \"CREATE DATABASE $DB\" && APP_TEST_DB=$DB go test ./store/...",
        "for f in *.go; do gofmt -l \"$f\"; done",
        "curl -s \"$URL\"",
        "cat \"$FOO\"",
        "cat < \"$FOO\"",
        "cat \"$(cat list)\"",
        "FOO=$(cat list); cat \"$FOO\"",
        "ls -l \"$DIR\"",
    ] {
        for mode in [Mode::Manual, Mode::Bypass] {
            let approver = ScriptedApprover::new(vec![]);
            let p = gate(mode, rules(&[], &[], &[]), approver.clone());
            let verdict = p.check_call("bash", &bash(command), 0, true).await;
            assert_eq!(verdict, allowed_in(mode, true), "{mode:?}: {command}");
            assert_eq!(approver.ask_count(), 0, "{mode:?}: {command}");
        }
    }
}

/// 没有沙箱时按普通审批走：未知参数证明不了只读，问一次，不再硬拒。
#[tokio::test]
async fn plan284_dynamic_targets_without_a_sandbox_take_the_ordinary_path() {
    for command in [
        "cat \"$FOO\"",
        "mysql -h 127.0.0.1 -u root -e \"CREATE DATABASE $DB\"",
    ] {
        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        let verdict = p.check_call("bash", &bash(command), 0, false).await;
        assert_eq!(verdict, allowed_verdict(false), "{command}");
        assert_eq!(approver.ask_count(), 1, "{command}");
    }
}

/// 明确的敏感读取照旧硬拒，与未决目标同处一条命令也一样；凭据文件名出现在
/// 同一条命令里时，未决目标按它算。
#[tokio::test]
async fn plan284_known_secrets_stay_hard_limits_next_to_dynamic_targets() {
    for command in [
        // 未决目标在前，不能让扫描提前结束；`.e*` 不含完整文件名，只有路径判定看得见。
        "mysql -e \"$SQL\"; cat .e*",
        "cat \"$FOO\" < ~/.bashrc",
        "cat \"$HOME/.ssh/id_rsa\"",
        "cat \"$DIR/.env\"",
        "printf '.bashrc\\n' | xargs -I marker cat marker",
        "echo .env > list; FOO=$(cat list); cat \"$FOO\"",
        "F=.envrc; cat \"$F\"",
    ] {
        for mode in [Mode::Manual, Mode::Bypass] {
            for sandbox_auto_allow in [false, true] {
                let approver = ScriptedApprover::new(vec![]);
                let p = gate(mode, rules(&["bash"], &[], &[]), approver.clone());
                let error = p
                    .check_call("bash", &bash(command), 0, sandbox_auto_allow)
                    .await
                    .expect_err(command);
                assert!(error.contains("sensitive path"), "{command}: {error}");
                assert_eq!(approver.ask_count(), 0, "{command}");
            }
        }
    }
}

/// 正文里的凭据探针仍要求在沙箱里跑；旁边多一个变量参数不再把它变成硬拒。
#[cfg(target_os = "macos")]
#[tokio::test]
async fn plan284_dynamic_target_keeps_literal_body_protection() {
    let home = std::env::temp_dir().join(crate::resource_id::fresh("plan284-home-").unwrap());
    std::fs::create_dir_all(&home).unwrap();
    let shell_home = home.to_str().unwrap();
    let policy = SandboxPolicy::workspace(&home, &[], false).with_credential_roots(&home);
    let execution =
        || ExecutionContext::sandboxed(Some(&policy)).with_shell_home(Some(shell_home));
    let p = gate(Mode::Manual, rules(&[], &[], &[]), ScriptedApprover::new(vec![]));
    let command = "cat > out.txt <<'EOF'\n~/.ssh/id_rsa\nEOF\ncargo run -- \"$ARG\"";

    let verdict = p
        .check_call_with_execution("bash", &bash(command), 0, execution(), None)
        .await
        .unwrap();
    assert_eq!(verdict.protection, Protection::Required);
    assert_eq!(verdict.sandbox, SandboxPlan::Contained);

    let _ = std::fs::remove_dir_all(home);
}

/// 放宽的调用不能借未知词绕过 deny/ask：已知前缀与规则一致就按可能命中处理。
/// 原本就放行的调用（参数角色已知、不是未决读取）沿用原匹配，不因此新增拒绝。
#[tokio::test]
async fn plan284_unknown_words_cannot_slip_past_deny_but_add_no_new_denials() {
    let p = gate(
        Mode::Bypass,
        rules(&[], &["bash(git push *)", "bash(make clean)"], &[]),
        ScriptedApprover::new(vec![]),
    );
    for command in ["git $ACTION push origin", "git \"$ACTION\" push", "make \"$TARGET\""] {
        let error = p.check_call("bash", &bash(command), 0, true).await.expect_err(command);
        assert!(error.contains("deny permission rule"), "{command}: {error}");
    }
    for command in ["git status \"$PATHSPEC\"", "make test \"$EXTRA\"", "go test -run \"$T\" ./..."] {
        let verdict = p.check_call("bash", &bash(command), 0, true).await;
        assert_eq!(verdict, allowed_in(Mode::Bypass, true), "{command}");
    }

    // `rm` 的参数角色已知，不是未决读取：plan 284 前后都照常在沙箱里放行。
    let approver = ScriptedApprover::new(vec![]);
    let p = gate(Mode::Manual, rules(&[], &["bash(rm -rf *)"], &[]), approver.clone());
    let verdict = p.check_call("bash", &bash("rm \"$TMPFILE\""), 0, true).await;
    assert_eq!(verdict, allowed_verdict(true));
    assert_eq!(approver.ask_count(), 0);
}

/// 正文保护与未决读取可以同时成立：前者不能吞掉后者，否则 deny/ask 的保守匹配
/// 被跳过——单独的 `make "$TARGET"` 命中 `bash(make clean)`，加一段凭据探针正文就不命中。
#[cfg(target_os = "macos")]
#[tokio::test]
async fn plan284_literal_body_protection_keeps_the_unresolved_read_for_rules() {
    let home = std::env::temp_dir().join(crate::resource_id::fresh("plan284-rules-").unwrap());
    std::fs::create_dir_all(&home).unwrap();
    let shell_home = home.to_str().unwrap();
    let policy = SandboxPolicy::workspace(&home, &[], false).with_credential_roots(&home);
    let execution =
        || ExecutionContext::sandboxed(Some(&policy)).with_shell_home(Some(shell_home));
    let body = "cat > probe.txt <<'EOF'\n~/.ssh/id_rsa\nEOF\n";
    for command in [
        "TARGET=clean\nmake \"$TARGET\"".to_string(),
        format!("TARGET=clean\n{body}make \"$TARGET\""),
    ] {
        let p = gate(
            Mode::Bypass,
            rules(&[], &["bash(make clean)"], &[]),
            ScriptedApprover::new(vec![]),
        );
        let error = p
            .check_call_with_execution("bash", &bash(&command), 0, execution(), None)
            .await
            .expect_err(&command);
        assert!(error.contains("deny permission rule"), "{command}: {error}");

        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
        let p = gate(Mode::Manual, rules(&[], &[], &["bash(make clean)"]), approver.clone());
        let verdict = p
            .check_call_with_execution("bash", &bash(&command), 0, execution(), None)
            .await
            .unwrap_or_else(|error| panic!("{command}: {error}"));
        assert_eq!(approver.ask_count(), 1, "{command}");
        assert_eq!(verdict.sandbox, SandboxPlan::Contained, "{command}");
        let protection = if command.contains("<<") {
            Protection::Required
        } else {
            Protection::Clear
        };
        assert_eq!(verdict.protection, protection, "{command}");
    }

    let _ = std::fs::remove_dir_all(home);
}
