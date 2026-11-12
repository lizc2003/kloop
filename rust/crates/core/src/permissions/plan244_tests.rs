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

/// 升级时选择 Project 写下的是升级规则，不是“可以运行”的普通规则。
#[tokio::test]
async fn plan244_escalation_project_memory_is_escalation_vocabulary() {
    let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Project)]);
    let writer = ScriptedWriter::succeeding();
    let p = gate_with_writer(Mode::Manual, approver, writer.clone());
    assert_eq!(
        p.escalate_sandbox("make check", None, 0).await.outcome,
        EscalationOutcome::Approved(EscalationApproval::User)
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

/// xargs 的替换串会写进后面每个参数（包括嵌套 shell 的脚本文本），实际目标
/// 不再由字面 argv 决定；写入事实必须保持未知，不能让 `bash_write` 规则旁路。
#[tokio::test]
async fn plan244_xargs_replacement_keeps_the_written_target_unknown() {
    let approver = ScriptedApprover::new(vec![]);
    let p = gate(
        Mode::Manual,
        rules(&[], &["bash_write(locked.txt)"], &[]),
        approver.clone(),
    );
    let command = "printf 'locked\\n' | xargs -I marker bash -c 'printf x > marker.txt'";
    let error = p.check("bash", &bash(command), 0).await.unwrap_err();
    assert!(error.contains("deny permission rule"), "{error}");
    assert_eq!(approver.ask_count(), 0);
}

/// `--process-slot-var` 会改写子进程环境；内层 `$HOME` 不能再按父 HOME 求值。
#[tokio::test]
async fn plan244_process_slot_var_makes_the_child_home_unknown() {
    let approver = ScriptedApprover::new(vec![]);
    let p = gate(
        Mode::Manual,
        rules(&[], &["bash_write(**)"], &[]),
        approver.clone(),
    );
    let command = "xargs --process-slot-var=HOME bash -c 'printf x > \"$HOME\"'";
    let error = p.check("bash", &bash(command), 0).await.unwrap_err();
    assert!(error.contains("deny permission rule"), "{error}");
    assert_eq!(approver.ask_count(), 0);
}

/// `time -o` 由包装器自己写报告文件；剥离后没有命令承载这份写入，事实层必须
/// 保留未知写效果。
#[tokio::test]
async fn plan244_time_output_report_stays_a_write() {
    let approver = ScriptedApprover::new(vec![]);
    let p = gate(
        Mode::Manual,
        rules(&[], &["bash_write(locked.log)"], &[]),
        approver.clone(),
    );
    let error = p.check("bash", &bash("time -o locked.log true"), 0).await.unwrap_err();
    assert!(error.contains("deny permission rule"), "{error}");
    assert_eq!(approver.ask_count(), 0);
}

/// 逐字记忆只限制“新规则”的粒度，已有精确升级规则仍须认领同一条命令，
/// 不能因为它先进沙箱而重新询问。
#[tokio::test]
async fn plan244_exact_escalation_rule_still_matches_verbatim_shapes() {
    let approver = ScriptedApprover::new(vec![]);
    let p = gate(
        Mode::Manual,
        rules(&["sandbox_escalate(timeout 600 cargo test)"], &[], &[]),
        approver.clone(),
    );
    assert!(p.sandbox_escalation_remembered("timeout 600 cargo test"));
    assert!(!p.sandbox_escalation_remembered("timeout 600 cargo build"));
    assert_eq!(approver.ask_count(), 0);
}

fn plan244_effectful_wrappers() -> [&'static str; 4] {
    [
        "printf x | xargs -I marker",
        "printf x | xargs --replace=marker",
        "/usr/bin/time -o timing.log",
        "/usr/bin/time --output=timing.log",
    ]
}

#[tokio::test]
async fn plan244_effectful_wrappers_keep_nested_deny_and_danger() {
    for wrapper in plan244_effectful_wrappers() {
        let command = format!("{wrapper} bash -c 'rm -rf build'");
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(Mode::Bypass, rules(&[], &["bash(rm *)"], &[]), approver.clone());
        let error = p.check_call("bash", &bash(&command), 0, true).await.expect_err(&command);
        assert!(error.contains("deny permission rule"), "{command}: {error}");
        assert_eq!(approver.ask_count(), 0, "{command}");

        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        p.check_call("bash", &bash(&command), 0, true).await.unwrap();
        let asked = approver.asked();
        assert_eq!(asked.len(), 1, "{command}");
        assert!(asked[0].description.contains("[destructive]"), "{command}");

        let command = format!("{wrapper} bash -c 'git push'");
        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
        let p = gate(Mode::Manual, rules(&[], &[], &["bash(git push *)"]), approver.clone());
        p.check_call("bash", &bash(&command), 0, true).await.unwrap();
        assert_eq!(approver.ask_count(), 1, "{command}");
    }
}

#[tokio::test]
async fn plan244_effectful_wrappers_keep_nested_sensitive_reads() {
    for wrapper in plan244_effectful_wrappers() {
        let command = format!("{wrapper} bash -c 'cat \"$HOME/.bashrc\"'");
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(Mode::Bypass, rules(&[], &[], &[]), approver.clone());
        let error = p.check_call("bash", &bash(&command), 0, true).await.expect_err(&command);
        assert!(error.contains("sensitive path"), "{command}: {error}");
        assert_eq!(approver.ask_count(), 0, "{command}");
    }
}

/// 带未知效果的包装器既不能落成前缀规则，也不再记原文（plan 283）：只能允许一次。
#[tokio::test]
async fn plan244_effectful_wrappers_are_only_allowed_once() {
    let commands = plan244_effectful_wrappers()
        .into_iter()
        .map(|wrapper| format!("{wrapper} cargo test"))
        .chain(["bash -c '/usr/bin/time -o timing.log cargo test'".into()]);
    for command in commands {
        let approver = ScriptedApprover::new(vec![
            Decision::Allow(ApprovalScope::WorkspaceSession),
            Decision::Allow(ApprovalScope::Once),
        ]);
        let writer = ScriptedWriter::succeeding();
        let p = gate_with_writer(Mode::Manual, approver.clone(), writer.clone());
        assert_eq!(p.escalate_sandbox(&command, None, 0).await.outcome,
            EscalationOutcome::Declined, "{command}: a scope that was not offered");
        assert_eq!(p.escalate_sandbox(&command, None, 0).await.outcome,
            EscalationOutcome::Approved(EscalationApproval::User), "{command}");
        for asked in approver.asked() {
            assert_eq!(asked.approval_scopes, vec![ApprovalScope::Once], "{command}");
            assert_eq!(asked.remember_rules, None, "{command}");
        }
        assert!(!p.sandbox_escalation_remembered(&command), "{command}");
        assert!(writer.calls.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn plan244_effectful_wrappers_keep_containment_consents_separate() {
    for wrapper in plan244_effectful_wrappers() {
        let command = format!("{wrapper} cargo test");
        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::WorkspaceSession)]);
        let p = gate(Mode::Manual, rules(&["bash"], &[], &[]), approver.clone());
        assert!(ok(&p, "bash", bash(&command)).await);
        assert_eq!(approver.ask_count(), 1, "{command}: whole-tool allow cannot cover unknown effects");
        assert!(!p.sandbox_escalation_remembered(&command), "{command}");
        assert_eq!(p.escalate_sandbox(&command, None, 0).await.outcome,
            EscalationOutcome::Declined);
        assert_eq!(approver.ask_count(), 2, "{command}");
    }
}

fn plan244_rewriting_xargs() -> [(&'static str, &'static str); 6] {
    [
        ("xargs -I marker", "marker"),
        ("xargs -Imarker", "marker"),
        ("xargs --replace=marker", "marker"),
        ("xargs -imarker", "marker"),
        ("xargs --replace", "{}"),
        ("xargs -i", "{}"),
    ]
}

#[tokio::test]
async fn plan244_rewritten_xargs_cannot_bypass_command_deny() {
    for (wrapper, marker) in plan244_rewriting_xargs() {
        for script in [marker.to_string(), format!("echo {marker}"), format!("r{marker}")] {
            let command = format!("printf 'rm -rf build\\n' | {wrapper} bash -c '{script}'");
            for mode in [Mode::Manual, Mode::Bypass] {
                let approver = ScriptedApprover::new(vec![]);
                let p = gate(mode, rules(&[], &["bash(rm *)"], &[]), approver.clone());
                let error = p.check_call("bash", &bash(&command), 0, true).await.expect_err(&command);
                assert!(error.contains("deny permission rule"), "{command}: {error}");
                assert_eq!(approver.ask_count(), 0, "{command}");
            }
        }
    }
    for command in [
        "xargs -I marker marker -rf build",
        "xargs -I marker env -u marker bash -c 'printf x'",
        "xargs -I marker xargs -I marker bash -c 'other'",
        "xargs -I marker bash marker 'rm -rf build'",
        "xargs -I marker -J other bash -c 'marker'",
        "xargs -I \"mar\\\\ker\" bash -c 'mar\\ker'",
        "xargs -I marker bash -c marker*",
        "bash -c 'xargs -I marker bash -c marker'",
    ] {
        let p = gate(Mode::Bypass, rules(&[], &["bash(rm *)"], &[]), ScriptedApprover::new(vec![]));
        let error = p.check_call("bash", &bash(command), 0, true).await.expect_err(command);
        assert!(error.contains("deny permission rule"), "{command}: {error}");
    }
    let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
    let p = gate(Mode::Manual, rules(&[], &[], &["bash(echo secret)"]), approver.clone());
    p.check_call("bash", &bash("xargs -I marker echo marker"), 0, true).await.unwrap();
    assert_eq!(approver.ask_count(), 1);
    for (command, deny) in [
        ("xargs -I marker rm marker build", "bash(rm -rf *)"),
        ("xargs -I marker echo marker", "bash(echo secret)"),
    ] {
        let p = gate(Mode::Bypass, rules(&[], &[deny], &[]), ScriptedApprover::new(vec![]));
        let error = p.check_call("bash", &bash(command), 0, true).await.expect_err(command);
        assert!(error.contains("deny permission rule"), "{command}: {error}");
    }
}

#[tokio::test]
async fn plan244_rewritten_xargs_read_targets_fail_closed() {
    for (wrapper, marker) in plan244_rewriting_xargs() {
        for tail in [
            format!("cat {marker}"),
            format!("bash -c 'cat {marker}'"),
            format!("bash -c 'cat < {marker}'"),
        ] {
            let command = format!("printf '.bashrc\\n' | {wrapper} {tail}");
            for mode in [Mode::Manual, Mode::Bypass] {
                let approver = ScriptedApprover::new(vec![]);
                let p = gate(mode, rules(&[], &[], &[]), approver.clone());
                let error = p.check_call("bash", &bash(&command), 0, true).await.expect_err(&command);
                assert!(error.contains("sensitive path"), "{command}: {error}");
                assert_eq!(approver.ask_count(), 0, "{command}");
            }
        }
        let command = format!("{wrapper} bash -c 'printf x > {marker}'");
        let p = gate(Mode::Bypass, rules(&[], &["bash_write(locked.txt)"], &[]), ScriptedApprover::new(vec![]));
        let error = p.check_call("bash", &bash(&command), 0, true).await.unwrap_err();
        assert!(error.contains("deny permission rule"), "{command}: {error}");

        // 直接交给 echo 的替换参数只是数据；没有进入 shell 正文或命令名。
        let command = format!("{wrapper} echo {marker}");
        let p = gate(Mode::Manual, rules(&[], &["bash(rm *)"], &[]), ScriptedApprover::new(vec![]));
        p.check_call("bash", &bash(&command), 0, true).await.expect(&command);
    }
    for command in [
        "xargs --replace -I marker echo {}",
        "xargs -I first -I marker echo first",
        "xargs -I marker e\\cho marker",
    ] {
        let p = gate(Mode::Manual, rules(&[], &["bash(rm *)"], &[]), ScriptedApprover::new(vec![]));
        p.check_call("bash", &bash(command), 0, true).await.expect(command);
    }
}

#[test]
fn plan244_rewritten_xargs_keeps_unknown_execution_dangerous() {
    for command in [
        "xargs -I marker bash -c 'marker'",
        "xargs -I marker bash -c 'printf x > marker'",
        "xargs -I marker marker -rf build",
        "xargs -I marker rm marker",
    ] {
        let call = CallFacts::gather("bash", &bash(command), &test_cwd(), None);
        assert_eq!(call.hazard("bash").map(|hazard| hazard.tag), Some("destructive"), "{command}");
    }
}

#[tokio::test]
async fn plan244_dynamic_wrapper_controls_keep_execution_unknown() {
    for tail in [
        "xargs $OPTS bash -c 'marker'",
        "xargs $OPTS -I marker bash -c 'marker'",
        "env $OPTS xargs -I marker bash -c 'marker'",
        "env -u $NAME xargs -I marker bash -c 'marker'",
        "command $OPTS xargs -I marker bash -c 'marker'",
        "xargs -I marker bash -c VALUE=\"$SCRIPT\"",
        "env \"$NAME=$VALUE\" xargs -I marker bash -c 'marker'",
    ] {
        let command = format!("printf 'rm -rf build\\n' | {tail}");
        for mode in [Mode::Manual, Mode::Bypass] {
            let approver = ScriptedApprover::new(vec![]);
            let p = gate(mode, rules(&[], &["bash(rm *)"], &[]), approver.clone());
            let error = p.check_call("bash", &bash(&command), 0, true).await.expect_err(&command);
            assert!(error.contains("deny permission rule"), "{command}: {error}");
            assert_eq!(approver.ask_count(), 0, "{command}");

            let p = gate(mode, rules(&[], &[], &[]), approver.clone());
            let error = p.check_call("bash", &bash(&command), 0, true).await.expect_err(&command);
            assert!(error.contains("sensitive path"), "{command}: {error}");
            assert_eq!(approver.ask_count(), 0, "{command}");
        }
    }
}

#[tokio::test]
async fn plan244_replacement_roles_preserve_nonreading_arguments() {
    for tail in [
        "mkdir -m marker notes",
        "git commit -m marker", "git commit --message=marker", "git commit -mmarker",
        "command echo marker", "command -p echo marker",
        "rg -e marker notes.txt", "rg --glob marker PATTERN notes.txt",
    ] {
        let command = format!("printf 'notes.txt\\n' | xargs -I marker {tail}");
        let call = CallFacts::gather("bash", &bash(&command), &test_cwd(), None);
        assert_eq!(call.hazard("bash").map(|hazard| hazard.tag), None, "{command}");
        for mode in [Mode::Manual, Mode::Bypass] {
            let approver = ScriptedApprover::new(vec![]);
            let p = gate(mode, rules(&[], &["bash(rm *)"], &[]), approver.clone());
            p.check_call("bash", &bash(&command), 0, true).await.expect(&command);
            assert_eq!(approver.ask_count(), 0, "{command}");
        }
        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        p.check_call("bash", &bash(&command), 0, false).await.expect(&command);
        let asked = approver.asked();
        assert_eq!(asked.len(), 1, "{command}");
        assert!(!asked[0].description.contains("[destructive]"), "{command}");
    }
    for command in [
        "xargs -I command echo literal", "xargs -I marker echo command",
        "env HOME=\"$DIR\" touch notes.txt",
    ] {
        let p = gate(Mode::Bypass, rules(&[], &["bash(command *)"], &[]), ScriptedApprover::new(vec![]));
        p.check_call("bash", &bash(command), 0, true).await.expect(command);
    }
}

#[tokio::test]
async fn plan244_replacement_roles_keep_reads_and_command_rules() {
    for tail in [
        "cat marker", "git commit -F marker", "git commit --file=marker",
        "rg -f marker notes.txt", "rg marker notes.txt", "command marker",
        "command cat marker",
    ] {
        let command = format!("xargs -I marker {tail}");
        let p = gate(Mode::Bypass, rules(&[], &[], &[]), ScriptedApprover::new(vec![]));
        let error = p.check_call("bash", &bash(&command), 0, true).await.expect_err(&command);
        assert!(error.contains("sensitive path"), "{command}: {error}");
    }
    for (command, deny) in [
        ("xargs -I marker command echo marker", "bash(echo secret)"),
        ("xargs -I marker command echo marker", "bash(command *)"),
        ("xargs -I marker env command echo marker", "bash(command *)"),
    ] {
        let p = gate(Mode::Bypass, rules(&[], &[deny], &[]), ScriptedApprover::new(vec![]));
        let error = p.check_call("bash", &bash(command), 0, true).await.unwrap_err();
        assert!(error.contains("deny permission rule"), "{command}: {error}");
    }
}

#[tokio::test]
async fn plan244_literal_and_replacement_roles_agree() {
    for tail in [
        "git -C repo commit -m marker",
        "git --no-pager commit -m marker",
        "git -c core.pager=cat commit -m marker",
        "ls --color=marker src",
        "stat --format=marker src",
    ] {
        for command in [tail.replace("marker", "notes.txt"), format!("xargs -I marker {tail}")] {
            let call = CallFacts::gather("bash", &bash(&command), &test_cwd(), None);
            assert!(!call.hazard("bash").is_some_and(|hazard| hazard.tag == "destructive"), "{command}");
            for mode in [Mode::Manual, Mode::Bypass] {
                let approver = ScriptedApprover::new(vec![]);
                let p = gate(mode, rules(&[], &["bash(rm *)"], &[]), approver.clone());
                p.check_call("bash", &bash(&command), 0, true).await.expect(&command);
                assert_eq!(approver.ask_count(), 0, "{command}");
            }
        }
    }
    for tail in [
        "git -C repo commit -m marker", "git --no-pager commit -m marker",
        "git -c core.pager=cat commit -m marker", "ls --color=marker src", "stat --format=marker src",
    ] {
        for value in [".env", "notes.txt"] {
            let command = tail.replace("marker", value);
            let p = gate(Mode::Bypass, rules(&[], &[], &[]), ScriptedApprover::new(vec![]));
            p.check_call("bash", &bash(&command), 0, true).await.expect(&command);
        }
    }
    for command in ["sudo find . -path ./.git -prune -o -print", "xargs find . -path ./.git -prune -o -print"] {
        let p = gate(Mode::Bypass, rules(&[], &[], &[]), ScriptedApprover::new(vec![]));
        p.check_call("bash", &bash(command), 0, true).await.expect(command);
    }
}

#[tokio::test]
async fn plan244_output_roles_keep_write_rules_and_reads() {
    for tail in ["tree -o marker src", "find . -fprint marker"] {
        for command in [tail.replace("marker", "out.txt"), format!("xargs -I marker {tail}")] {
            let p = gate(Mode::Bypass, rules(&[], &["bash_write(**)"], &[]), ScriptedApprover::new(vec![]));
            let error = p.check_call("bash", &bash(&command), 0, true).await.expect_err(&command);
            assert!(error.contains("deny permission rule"), "{command}: {error}");
        }
    }
    for command in [
        "tree -o .git/config .", "tree -so .git/config .",
        "find . -path ./.git -prune -o -fprint result.txt",
        "find . -path ./.git -prune -o -fprintf result.txt '%p'",
        "find . -path ./.git -prune -o -fls result.txt",
    ] {
        let p = gate(Mode::Bypass, rules(&[], &[], &[]), ScriptedApprover::new(vec![]));
        p.check_call("bash", &bash(command), 0, true).await.expect(command);
        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        p.check_call("bash", &bash(command), 0, false).await.expect(command);
        assert_eq!(approver.ask_count(), 1, "{command}");
    }
    for tail in [
        "git -C repo commit -F marker", "git --no-pager commit --file=marker",
        "git -c core.pager=cat commit -F marker", "tree --fromfile marker",
        "find . -files0-from marker",
    ] {
        for command in [tail.replace("marker", ".env"), format!("xargs -I marker {tail}")] {
            let p = gate(Mode::Bypass, rules(&[], &[], &[]), ScriptedApprover::new(vec![]));
            let error = p.check_call("bash", &bash(&command), 0, true).await.expect_err(&command);
            assert!(error.contains("sensitive path"), "{command}: {error}");
        }
    }
}

#[tokio::test]
async fn plan244_quoted_assignment_values_keep_execution_and_nested_risks() {
    for tail in ["env FOO=\"$(pwd)\" touch notes.txt", "env FOO=\"$*\" touch notes.txt"] {
        for command in [tail.to_string(), format!("xargs -I marker {tail}")] {
            let call = CallFacts::gather("bash", &bash(&command), &test_cwd(), None);
            assert_eq!(call.hazard("bash").map(|hazard| hazard.tag), None, "{command}");
            let p = gate(Mode::Bypass, rules(&[], &[], &[]), ScriptedApprover::new(vec![]));
            p.check_call("bash", &bash(&command), 0, true).await.expect(&command);
        }
    }
    for tail in ["env FOO=\"$(cat .env)\" touch notes.txt", "env FOO=\"$@\" touch notes.txt"] {
        let command = format!("xargs -I marker {tail}");
        let p = gate(Mode::Bypass, rules(&[], &[], &[]), ScriptedApprover::new(vec![]));
        let error = p.check_call("bash", &bash(&command), 0, true).await.expect_err(&command);
        assert!(error.contains("sensitive path"), "{command}: {error}");
    }
    for command in ["env FOO=\"$(pwd)\" touch notes.txt", "xargs -I marker env FOO=\"$(pwd)\" touch notes.txt"] {
        for deny in ["bash(pwd *)", "bash(touch *)"] {
            let p = gate(Mode::Bypass, rules(&[], &[deny], &[]), ScriptedApprover::new(vec![]));
            let error = p.check_call("bash", &bash(command), 0, true).await.expect_err(command);
            assert!(error.contains("deny permission rule"), "{command}: {error}");
        }
    }
}

#[tokio::test]
async fn plan244_sensitive_metadata_keeps_known_and_replaced_targets_blocked() {
    for tail in [
        "touch marker", "mkdir marker", "chmod 644 marker", "touch -r marker notes.txt",
        "chmod --reference=marker notes.txt", "ls -l marker", "stat marker",
        "du -- marker", "tree -- marker", "find -f marker -maxdepth 0",
    ] {
        let benign = tail.replace("marker", "notes.txt");
        for mode in [Mode::Manual, Mode::Bypass] {
            let approver = ScriptedApprover::new(vec![]);
            let p = gate(mode, rules(&[], &[], &[]), approver.clone());
            p.check_call("bash", &bash(&benign), 0, true).await.expect(&benign);
            for command in [
                tail.replace("marker", ".env"), tail.replace("marker", "~/.ssh/id_rsa"),
                tail.replace("marker", "~/.kloop/config.toml"), format!("xargs -I marker {tail}"),
            ] {
                let error = p.check_call("bash", &bash(&command), 0, true).await.expect_err(&command);
                assert!(error.contains("sensitive path"), "{command}: {error}");
            }
            assert_eq!(approver.ask_count(), 0, "{tail}");
        }
        let command = format!("xargs -I marker {tail}");
        let call = CallFacts::gather("bash", &bash(&command), &test_cwd(), None);
        assert!(!call.hazard("bash").is_some_and(|hazard| hazard.tag == "destructive"), "{command}");
    }
}

#[tokio::test]
async fn plan244_dynamic_reads_fail_closed_after_data_and_scalar_assignments() {
    for mode in [Mode::Manual, Mode::Bypass] {
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(mode, rules(&["bash"], &[], &[]), approver.clone());
        for command in [
            "echo .env > list", "find . -name .env", "rg -e .env notes.txt",
            "git commit -m .env", "env FOO=\"$(cat list)\" touch notes.txt",
            "env FOO=\"$(pwd)\" touch notes.txt", "env FOO=\"$*\" touch notes.txt",
            "echo \"$FOO\"", "git commit -m \"$FOO\"", "cat list",
        ] {
            p.check_call("bash", &bash(command), 0, true).await.expect(command);
        }
        for command in [
            "env FOO=\"$(cat list)\" sh -c 'cat \"$FOO\"'",
            "echo .env > list; env FOO=\"$(cat list)\" sh -c 'cat \"$FOO\"'",
            "FOO=$(cat list); cat \"$FOO\"", "cat \"$(cat list)\"",
            "cat \"$FOO\"", "cat \"$DIR/notes.txt\"", "sudo cat \"$FOO\"",
            "cat < \"$FOO\"", "xargs -I marker cat marker",
        ] {
            for sandbox_auto_allow in [false, true] {
                let error = p.check_call("bash", &bash(command), 0, sandbox_auto_allow)
                    .await.expect_err(command);
                assert!(error.contains("sensitive path"), "{command}: {error}");
            }
        }
        assert_eq!(approver.ask_count(), 0);
    }
}

#[tokio::test]
async fn plan244_secret_writes_are_hard_limits_across_output_forms() {
    for mode in [Mode::Manual, Mode::Bypass] {
        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once); 32]);
        let p = gate(mode, rules(&["bash", "write_file", "edit_file", "notebook_edit"], &[], &[]), approver.clone());
        for target in [".env", ".env.local", "\"$DIR/.env\"", "~/.bashrc"] {
            for command in [
                format!("tree -o {target} ."), format!("tree --outfile={target} ."),
                format!("find . -fprint {target}"), format!("find . -fprintf {target} '%p'"),
                format!("printf x > {target}"),
                format!("cd elsewhere; printf x > {target}"),
            ] {
                for sandbox_auto_allow in [false, true] {
                    let error = p.check_call("bash", &bash(&command), 0, sandbox_auto_allow)
                        .await.expect_err(&command);
                    assert!(error.contains("writing this sensitive path"), "{command}: {error}");
                }
            }
        }
        for name in ["write_file", "edit_file", "notebook_edit"] {
            for path in [".env", ".aws/note.ipynb"] {
                let input = if name == "notebook_edit" {
                    json!({"notebook_path": path})
                } else { file(path) };
                let error = p.check_call(name, &input, 0, true).await.expect_err(name);
                assert!(error.contains("writing this sensitive path"), "{name}: {error}");
            }
        }
        assert_eq!(approver.ask_count(), 0);
    }
    for target in [".git/config", "/dev/disk0"] {
        let command = format!("tree -o {target} .");
        for mode in [Mode::Manual, Mode::Bypass] {
            let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
            let p = gate(mode, rules(&[], &[], &[]), approver.clone());
            p.check_call("bash", &bash(&command), 0, true).await.expect(&command);
            assert_eq!(approver.ask_count(), usize::from(mode == Mode::Manual));
        }
    }
    #[cfg(unix)]
    {
        let root = plan244_temp("secret-write-alias");
        std::fs::write(root.join(".env"), "dummy fixture").unwrap();
        std::os::unix::fs::symlink(root.join(".env"), root.join("notes.txt")).unwrap();
        let target = root.join("notes.txt").display().to_string();
        for mode in [Mode::Manual, Mode::Bypass] {
            let p = gate(mode, rules(&[], &[], &[]), ScriptedApprover::new(vec![]));
            for (name, input) in [
                ("write_file", file(&target)),
                ("bash", bash(&format!("tree -o '{target}' ."))),
            ] {
                let error = p.check_call(name, &input, 0, true).await.expect_err(&target);
                assert!(error.contains("writing this sensitive path"), "{name}: {error}");
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn plan244_data_roles_preserve_sensitive_directory_evidence() {
    for command in [
        "echo ~/.ssh/key > list", "find . -name ~/.ssh/key",
        "rg -e ~/.ssh/key notes.txt", "git commit -m ~/.ssh/key",
        "tree -o ~/.ssh/key .", "find . -fprint ~/.ssh/key",
    ] {
        for mode in [Mode::Manual, Mode::Bypass] {
            let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
            let p = gate(mode, rules(&[], &[], &[]), approver.clone());
            let error = p.check_call("bash", &bash(command), 0, true).await.expect_err(command);
            assert!(error.contains("sensitive path"), "{command}: {error}");
            assert_eq!(approver.ask_count(), 0, "{command}");
        }
    }
}

#[tokio::test]
async fn plan244_unknown_output_targets_cannot_bypass_secret_write_limits() {
    for tail in [
        "tree -o marker src", "tree --outfile=marker src", "tree -so marker src",
        "find . -fprint marker", "find . -fprint0 marker", "find . -fprintf marker '%p'",
        "find . -fls marker",
    ] {
        for mode in [Mode::Manual, Mode::Bypass] {
            let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once); 16]);
            let p = gate(mode, rules(&["bash", "bash_write(**)"], &[], &[]), approver.clone());
            for command in [
                format!("xargs -I marker {tail}"),
                format!("printf '.env\\n' | xargs -I marker {tail}"),
                tail.replace("marker", "\"$TARGET\""),
                tail.replace("marker", ".env"),
            ] {
                for sandbox_auto_allow in [false, true] {
                    let error = p.check_call("bash", &bash(&command), 0, sandbox_auto_allow)
                        .await.expect_err(&command);
                    assert!(error.contains("writing this sensitive path"), "{command}: {error}");
                }
            }
            assert_eq!(approver.ask_count(), 0, "{tail}");
            let ordinary = tail.replace("marker", "out.txt");
            p.check_call("bash", &bash(&ordinary), 0, true).await.expect(&ordinary);
        }
    }
}

#[tokio::test]
async fn plan244_unknown_redirect_targets_keep_the_hard_write_boundary() {
    for command in [
        "printf x > \"$TARGET\"", "printf x > \"$DIR/notes.txt\"",
        "printf x > \"$(cat target.txt)\"", "printf x > *.txt",
    ] {
        for mode in [Mode::Manual, Mode::Bypass] {
            let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
            let p = gate(mode, rules(&["bash", "bash_write(**)"], &[], &[]), approver.clone());
            for sandbox_auto_allow in [false, true] {
                let error = p.check_call("bash", &bash(command), 0, sandbox_auto_allow)
                    .await.expect_err(command);
                assert!(error.contains("writing this sensitive path"), "{command}: {error}");
            }
            assert_eq!(approver.ask_count(), 0, "{command}");
        }
    }
}
