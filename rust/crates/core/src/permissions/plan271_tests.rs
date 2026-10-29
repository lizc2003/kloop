// plan 271：`.kloop` 的读取只认私有状态根；项目里的 `.kloop/` 可读，写入仍确认。

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
async fn plan271_project_kloop_dirs_are_readable() {
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
            format!("rg style {shown}/.kloop"),
            format!("ls {shown}/.k*"),
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

/// 私有状态根仍是硬边界：任何拼写落到 `~/.kloop` 都拒，包括递归搜索的根把它
/// 包在里面。
#[tokio::test]
async fn plan271_private_state_root_stays_hard_blocked() {
    let home = std::env::home_dir().expect("home");
    let parent = home.parent().expect("home has a parent").display().to_string();
    let home = home.display();
    for mode in [Mode::Manual, Mode::Plan, Mode::Bypass] {
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(mode, rules(&[], &[], &[]), approver.clone());
        for command in [
            format!("cat {home}/.kloop/config.toml"),
            "cat ${HOME}/.kloop/sessions/x.jsonl".to_string(),
            format!("grep -rn api_key {parent}"),
            format!("ls {home}/.KLOOP/sessions"),
        ] {
            let blocked = p
                .check_call("bash", &bash(&command), 0, /*sandbox_auto_allow*/ true)
                .await
                .is_err();
            // 大小写别名只在折叠大小写的文件系统上指向同一个目录。
            let expected = sensitive::FS_FOLDS_CASE || !command.contains(".KLOOP");
            assert_eq!(blocked, expected, "{mode:?}: {command}");
        }
        assert!(p.read_path_blocked(Path::new(&format!("{home}/.kloop/config.toml"))));
        assert_eq!(approver.ask_count(), 0, "{mode:?}");
    }
}
