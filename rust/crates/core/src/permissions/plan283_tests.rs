// plan 283 阶段 0：每个问题、以及每个替人答掉问题的记忆，都留一条记录。

async fn plan283_gate_record(p: &Permissions, input: Value) -> Option<ApprovalRecord> {
    p.gate_call(
        "bash",
        &input,
        None,
        None,
        0,
        crate::sandbox::ExecutionContext::approval_only(false),
        None,
    )
    .await
    .approvals
    .pop()
}

/// 门问了人，记下问题、给了哪些作用域、答了什么；同一个问题第二次被会话记忆
/// 答掉，也记一条，说明是哪种记忆答的。不问的调用（只读）没有记录。
#[tokio::test]
async fn plan283_the_gate_records_questions_and_the_memories_that_answer_them() {
    let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::WorkspaceSession)]);
    let p = gate(Mode::Manual, rules(&["bash(cargo test *)"], &[], &[]), approver);

    let asked = ApprovalRecord {
        door: ApprovalDoor::Gate,
        tool: "bash".into(),
        reason: AskReason::Uncontained,
        detail: "git commit -m x".into(),
        keys: vec!["bash:git commit".into()],
        rules: vec!["bash(git commit *)".into()],
        offered: vec![ApprovalScope::Once, ApprovalScope::WorkspaceSession],
        answer: ApprovalAnswer::Allowed {
            scope: ApprovalScope::WorkspaceSession,
        },
    };
    assert_eq!(
        plan283_gate_record(&p, bash("git commit -m x")).await,
        Some(asked.clone())
    );
    assert_eq!(
        plan283_gate_record(&p, bash("git commit -m y")).await,
        Some(ApprovalRecord {
            detail: "git commit -m y".into(),
            offered: Vec::new(),
            answer: ApprovalAnswer::Remembered {
                by: MemorySource::Session,
            },
            ..asked.clone()
        })
    );
    assert_eq!(
        plan283_gate_record(&p, bash("cargo test -p core")).await,
        Some(ApprovalRecord {
            detail: "cargo test -p core".into(),
            keys: vec!["bash:cargo test".into()],
            rules: vec!["bash(cargo test *)".into()],
            offered: Vec::new(),
            answer: ApprovalAnswer::Remembered {
                by: MemorySource::Rule,
            },
            ..asked.clone()
        })
    );
    // 危险命令走安全层：原因是那个标签，不提供记忆；脚本耗尽后答 Deny。
    assert_eq!(
        plan283_gate_record(&p, bash("rm -rf build")).await,
        Some(ApprovalRecord {
            reason: AskReason::Hazard {
                tag: "destructive".into(),
            },
            detail: "rm -rf build".into(),
            keys: Vec::new(),
            rules: Vec::new(),
            offered: vec![ApprovalScope::Once],
            answer: ApprovalAnswer::Denied,
            ..asked
        })
    );
    assert_eq!(plan283_gate_record(&p, bash("ls -la")).await, None);
}

/// 升级这扇门同样记录：问了什么、拒绝的类别；记住之后再次升级记成会话记忆；
/// 没有人可问时记 NoApprover；bypass 不是问题，不记。
#[tokio::test]
async fn plan283_the_escalation_records_questions_and_the_memories_that_answer_them() {
    let denial = crate::sandbox::SandboxDenial {
        kind: crate::sandbox::DenialKind::Network,
        evidence: "curl: (6) Could not resolve host: example.com".into(),
    };
    let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::WorkspaceSession)]);
    let p = gate(Mode::Manual, rules(&[], &[], &[]), approver);
    let asked = ApprovalRecord {
        door: ApprovalDoor::Escalation,
        tool: "bash".into(),
        reason: AskReason::SandboxDenied {
            denial: "network".into(),
        },
        detail: "curl -sS https://example.com".into(),
        // 第二词是选项：plan 244 起只记原文，所以键就是整条命令。
        keys: vec!["sandbox_escalate-script:curl -sS https://example.com".into()],
        rules: vec![],
        offered: vec![ApprovalScope::Once, ApprovalScope::WorkspaceSession],
        answer: ApprovalAnswer::Allowed {
            scope: ApprovalScope::WorkspaceSession,
        },
    };
    let ask = || p.escalate_sandbox("curl -sS https://example.com", Some(&denial), 0);
    assert_eq!(ask().await.approval, Some(asked.clone()));
    assert_eq!(
        ask().await.approval,
        Some(ApprovalRecord {
            offered: Vec::new(),
            answer: ApprovalAnswer::Remembered {
                by: MemorySource::Session,
            },
            ..asked
        })
    );

    let headless = Permissions::new(Mode::Manual, &rules(&[], &[], &[]), test_cwd(), None).unwrap();
    let unanswered = headless.escalate_sandbox("go test ./pkg", None, 0).await;
    assert_eq!(unanswered.outcome, EscalationOutcome::NotAttempted);
    assert_eq!(
        unanswered.approval.map(|record| (record.reason, record.answer)),
        Some((
            AskReason::SandboxDenied {
                denial: "unknown".into(),
            },
            ApprovalAnswer::NoApprover
        ))
    );

    let bypass = gate(Mode::Bypass, rules(&[], &[], &[]), ScriptedApprover::new(vec![]));
    assert_eq!(
        bypass.escalate_sandbox("go test ./pkg", None, 0).await.approval,
        None
    );
}

/// 记录要能原样写进会话文件再读回来：字段名就是离线统计读的那份格式。
#[test]
fn plan283_a_record_round_trips_through_its_file_shape() {
    let record = ApprovalRecord {
        door: ApprovalDoor::Escalation,
        tool: "bash".into(),
        reason: AskReason::SandboxDenied {
            denial: "file write".into(),
        },
        detail: "make install".into(),
        keys: vec!["sandbox_escalate:make install".into()],
        rules: vec!["sandbox_escalate(make install *)".into()],
        offered: vec![
            ApprovalScope::Once,
            ApprovalScope::WorkspaceSession,
            ApprovalScope::Project,
        ],
        answer: ApprovalAnswer::Allowed {
            scope: ApprovalScope::Project,
        },
    };
    let value = serde_json::to_value(&record).unwrap();
    assert_eq!(
        value,
        json!({
            "door": "escalation",
            "tool": "bash",
            "reason": {"kind": "sandbox_denied", "denial": "file write"},
            "detail": "make install",
            "keys": ["sandbox_escalate:make install"],
            "rules": ["sandbox_escalate(make install *)"],
            "offered": ["once", "workspace_session", "project"],
            "answer": {"kind": "allowed", "scope": "project"},
        })
    );
    assert_eq!(
        serde_json::from_value::<ApprovalRecord>(value).unwrap(),
        record
    );
}

/// 阶段 1：只读段与 `cd` 既不进升级的键，也不参与匹配。已存的
/// `sandbox_escalate(make check *)` 覆盖带 `cd` 前缀、`| tail -N` 尾巴的同一个意图；
/// 新的询问提议的就是这条规则，而不是逐字记忆或 `sandbox_escalate(cd … *)`。
#[tokio::test]
async fn plan283_read_only_segments_and_cd_are_not_part_of_the_escalation_key() {
    let p = gate(
        Mode::Manual,
        rules(&["sandbox_escalate(make check *)"], &[], &[]),
        ScriptedApprover::new(vec![]),
    );
    for command in [
        "make check",
        "make check 2>&1 | tail -20",
        "cd /Users/someone/repo && make check 2>&1 | tail -80",
        "pwd; make check | grep -E 'test result|FAILED' | head -40",
    ] {
        assert!(p.sandbox_escalation_remembered(command), "{command}");
    }
    // 有效果的段仍要逐个被覆盖。
    assert!(!p.sandbox_escalation_remembered("make check && cargo install --path ."));
    assert!(!p.sandbox_escalation_remembered("cd /Users/someone/repo && make install"));

    let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
    let writer = ScriptedWriter::succeeding();
    let p = gate_with_writer(Mode::Manual, approver.clone(), writer);
    let escalation = p
        .escalate_sandbox(
            "cd /Users/someone/repo && cargo test -p kloop-core 2>&1 | tail -20",
            None,
            0,
        )
        .await;
    assert_eq!(
        escalation.outcome,
        EscalationOutcome::Approved(EscalationApproval::User)
    );
    let asked = approver.asked();
    assert_eq!(
        asked[0].approval_scopes,
        vec![
            ApprovalScope::Once,
            ApprovalScope::WorkspaceSession,
            ApprovalScope::Project
        ]
    );
    assert_eq!(
        asked[0].remember_rules,
        Some(vec!["sandbox_escalate(cargo test *)".to_string()])
    );
}

/// 只剩写入的命令（`echo`/`cat` 是只读段）由写入规则单独覆盖：键里没有命令段，
/// 匹配也不因此判成“什么都没同意”。
#[cfg(unix)]
#[tokio::test]
async fn plan283_a_write_only_escalation_is_covered_by_its_write_rule() {
    let outside = std::fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join("kloop-plan283-outside");
    let command = format!("echo done > {}/marker.txt", outside.display());
    let remember = escalation_remember_payload(&command, &test_cwd(), None).unwrap();
    assert_eq!(
        remember.rules,
        vec![format!("sandbox_escalate_write({}/**)", outside.display())]
    );
    assert!(remember.argvs.is_empty());

    let rule = format!("sandbox_escalate_write({}/**)", outside.display());
    let p = gate(Mode::Manual, rules(&[&rule], &[], &[]), ScriptedApprover::new(vec![]));
    assert!(p.sandbox_escalation_remembered(&command));
    assert!(!p.sandbox_escalation_remembered(&format!("{command} && make install")));
    // 没有命令段也没有写入：没有可同意的东西，不算记住。
    assert!(!p.sandbox_escalation_remembered("ls && pwd"));
}

/// 复审：命中升级记忆、运行前就跳过沙箱的调用也要留一条记录——门在沙箱层直接放行、
/// 执行层不再尝试沙箱，两边都不问，没有这条，记忆持续生效就看不见。被拒的调用和后台
/// 调用（不跳过沙箱）都不记。
#[cfg(target_os = "macos")]
#[tokio::test]
async fn plan283_a_remembered_escalation_that_skips_the_sandbox_is_recorded() {
    let policy = crate::sandbox::SandboxPolicy::workspace(&test_cwd(), &[], false);
    let execution = || crate::sandbox::ExecutionContext::sandboxed(Some(&policy));
    let command = "cd /x && cargo test -p core 2>&1 | tail -20";
    let p = gate(
        Mode::Manual,
        rules(&["sandbox_escalate(cargo test *)"], &[], &[]),
        ScriptedApprover::new(vec![]),
    );
    let outcome = p
        .gate_call("bash", &bash(command), None, None, 0, execution(), None)
        .await;
    assert_eq!(outcome.result.unwrap().sandbox, SandboxPlan::Escalated);
    assert_eq!(
        outcome.approvals,
        vec![ApprovalRecord {
            door: ApprovalDoor::Escalation,
            tool: "bash".into(),
            reason: AskReason::EscalatedBeforeRun,
            detail: command.into(),
            keys: vec!["sandbox_escalate:cargo test".into()],
            rules: vec!["sandbox_escalate(cargo test *)".into()],
            offered: Vec::new(),
            answer: ApprovalAnswer::Remembered {
                by: MemorySource::Rule,
            },
        }]
    );

    let background = json!({"command": command, "background": true});
    let outcome = p
        .gate_call("bash", &background, None, None, 0, execution(), None)
        .await;
    assert_eq!(outcome.result.unwrap().sandbox, SandboxPlan::Contained);
    assert_eq!(outcome.approvals, Vec::new());

    // 显式 ask 规则问了、被拒：只有门自己的那条记录。
    let p = gate(
        Mode::Manual,
        rules(&["sandbox_escalate(cargo test *)"], &[], &["bash(cargo test *)"]),
        ScriptedApprover::new(vec![]),
    );
    let outcome = p
        .gate_call("bash", &bash(command), None, None, 0, execution(), None)
        .await;
    assert!(outcome.result.is_err());
    assert_eq!(
        outcome
            .approvals
            .iter()
            .map(|record| (record.reason.clone(), record.answer))
            .collect::<Vec<_>>(),
        vec![(AskReason::AskRule, ApprovalAnswer::Denied)]
    );
}
