// plan 271：`.kloop` 按它下面第一层的条目判。worktrees、rules、skills、commands
// 可读；其余是凭据或 agent state；写入照旧确认。

fn plan271_repo(label: &str) -> PathBuf {
    let root = std::env::temp_dir()
        .join(crate::resource_id::fresh(&format!("kloop-plan271-{label}-")).expect("fresh id"));
    std::fs::create_dir_all(root.join(".kloop/worktrees")).unwrap();
    std::fs::create_dir_all(root.join(".kloop/rules")).unwrap();
    std::fs::write(root.join(".kloop/rules/style.md"), "style").unwrap();
    std::fs::write(root.join("main.go"), "EndpointGeminiNative").unwrap();
    root
}

#[tokio::test]
async fn plan271_readable_kloop_entries_pass_in_any_spelling() {
    let root = plan271_repo("read");
    let shown = root.display();
    for mode in [Mode::Manual, Mode::Plan, Mode::Bypass] {
        let approver = ScriptedApprover::new(vec![]);
        let p = Permissions::new(mode, &rules(&[], &[], &[]), root.clone(), Some(approver.clone()))
            .unwrap();
        for command in [
            // 会话里被误拦的原命令：仓库根下有 kloop 自己建的 `.kloop/worktrees/`。
            format!(
                "cd {shown} && grep -rn \"EndpointGeminiNative\" --include='*.go' . | grep -v _test.go"
            ),
            "grep -rn EndpointGeminiNative .".to_string(),
            "cat .kloop/rules/style.md".into(),
            format!("cat {shown}/.kloop/rules/style.md"),
            format!("cd {shown} && cat ./.kloop/rules/style.md"),
            format!("rg style {shown}/.kloop/rules"),
            format!("ls {shown}/.kloop"),
            format!("cat {shown}/.k*/rules/style.md"),
        ] {
            assert_eq!(
                p.check_call("bash", &bash(&command), 0, /*sandbox_auto_allow*/ true)
                    .await,
                Ok(None),
                "{mode:?}: {command}"
            );
        }
        assert_eq!(
            p.check("read_file", &json!({"path": ".kloop/rules/style.md"}), 0)
                .await,
            Ok(None),
            "{mode:?}"
        );
        assert!(!p.read_path_blocked(&root.join(".kloop/rules/style.md")));
        assert_eq!(approver.ask_count(), 0, "{mode:?}");
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// 判定只看名字，换落点的拼写（glob、花括号、`..`、Git Bash 的 `/c/` 根、firmlink、
/// `cd`、命令自带的 `-C`、经文本传给 `xargs`）都改不了结论。
#[tokio::test]
async fn plan271_kloop_state_stays_blocked_in_any_spelling() {
    let root = plan271_repo("state");
    std::fs::write(root.join(".kloop/config.toml"), "api_key = 'x'").unwrap();
    let shown = root.display();
    let home = std::env::home_dir().expect("home");
    let above_home = home.parent().expect("home has a parent").display().to_string();
    for mode in [Mode::Manual, Mode::Plan, Mode::Bypass] {
        let approver = ScriptedApprover::new(vec![]);
        let p = Permissions::new(mode, &rules(&[], &[], &[]), root.clone(), Some(approver.clone()))
            .unwrap();
        for command in [
            "cat ~/{sub,}/.kloop/config.toml".to_string(),
            "cat /Users/*/.kloop/config.toml".into(),
            "cat /c/Users/u/.kloop/config.toml".into(),
            "cat /System/Volumes/Data/Users/u/.kloop/config.toml".into(),
            format!("cat {shown}/link/../.kloop/config.toml"),
            "cat ${HOME}/.kloop/mcp-oauth.json".into(),
            "cd ~ && cat .kloop/config.toml".into(),
            "tar -C ~ -cf - .kloop".into(),
            "echo /home/*/.kloop/config.toml | xargs cat".into(),
            format!("cat {shown}/.kloop/projects/v1/p/sessions/x.jsonl"),
            format!("rg KEY {shown}/.kloop"),
            "grep -rn api_key .".into(),
            format!("grep -rn api_key {above_home}"),
        ] {
            let error = p
                .check_call("bash", &bash(&command), 0, /*sandbox_auto_allow*/ true)
                .await
                .expect_err(&command);
            assert!(
                error.contains("sensitive path"),
                "{mode:?}: {command}: {error}"
            );
        }
        assert!(
            p.check("read_file", &json!({"path": ".kloop/config.toml"}), 0)
                .await
                .is_err(),
            "{mode:?}"
        );
        assert!(p.read_path_blocked(&root.join(".kloop/config.toml")));
        assert_eq!(approver.ask_count(), 0, "{mode:?}");
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// 项目 `.kloop/rules`、`skills`、`commands` 会引导以后的会话：写入照旧逐次确认。
#[tokio::test]
async fn plan271_project_kloop_writes_still_ask() {
    let root = plan271_repo("write");
    let approver = ScriptedApprover::new(vec![]);
    let p = Permissions::new(
        Mode::Manual,
        &rules(&["write_file", "bash"], &[], &[]),
        root.clone(),
        Some(approver.clone()),
    )
    .unwrap();
    assert!(!ok(&p, "write_file", file(".kloop/rules/style.md")).await);
    assert!(!ok(&p, "bash", bash("echo x > .kloop/rules/style.md")).await);
    let tags: Vec<_> = approver
        .asked()
        .iter()
        .map(|request| {
            ["[sensitive path]", "[sensitive redirect path]"]
                .into_iter()
                .find(|tag| request.description.contains(tag))
        })
        .collect();
    assert_eq!(
        tags,
        vec![Some("[sensitive path]"), Some("[sensitive redirect path]")]
    );
    let _ = std::fs::remove_dir_all(&root);
}
