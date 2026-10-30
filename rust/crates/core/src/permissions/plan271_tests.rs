// plan 271：项目里的 `.kloop` 读取整个放行；私有状态根 `~/.kloop`、以及证明不了
// 不是它的 `.kloop` 照旧按名字硬拦；写入照旧确认。

fn plan271_repo(label: &str) -> PathBuf {
    let root = std::env::temp_dir()
        .join(crate::resource_id::fresh(&format!("kloop-plan271-{label}-")).expect("fresh id"));
    std::fs::create_dir_all(root.join(".kloop/worktrees")).unwrap();
    std::fs::create_dir_all(root.join(".kloop/rules")).unwrap();
    std::fs::write(root.join(".kloop/rules/style.md"), "style").unwrap();
    std::fs::write(root.join(".kloop/notes.md"), "notes").unwrap();
    std::fs::write(root.join("main.go"), "EndpointGeminiNative").unwrap();
    root
}

#[tokio::test]
async fn plan271_project_kloop_reads_pass() {
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
            "cat ./.kloop/notes.md".into(),
            format!("cat {shown}/.kloop/rules/style.md"),
            format!("cd {shown} && cat {shown}/.kloop/notes.md"),
            format!("ls -la {shown}/.kloop/"),
            "ls ./.kloop/".into(),
            format!("rg style {shown}/.kloop"),
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
            p.check("read_file", &json!({"path": ".kloop/notes.md"}), 0)
                .await,
            Ok(None),
            "{mode:?}"
        );
        assert!(!p.read_path_blocked(&root.join(".kloop/rules/style.md")));
        assert_eq!(approver.ask_count(), 0, "{mode:?}");
    }
    // 读重定向由 shell 自己打开；plan 模式不把重定向算作只读，只在 bypass 下看敏感读取门。
    let p = Permissions::new(Mode::Bypass, &rules(&[], &[], &[]), root.clone(), None).unwrap();
    let command = format!("cat < {shown}/.kloop/notes.md");
    assert_eq!(
        p.check_call("bash", &bash(&command), 0, /*sandbox_auto_allow*/ true)
            .await,
        Ok(None),
        "{command}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// 证明不了落点的拼写按名字拦：换落点的写法（glob、花括号、`..`、Git Bash 的
/// `/c/` 根、firmlink、`cd`、命令自带的 `-C`、经文本传给 `xargs`）、能当跳板的
/// 命令（`cd`/`ln` 进私有状态根），以及会写或未知的命令。
#[tokio::test]
async fn plan271_unplaced_kloop_reads_stay_blocked() {
    let root = plan271_repo("unplaced");
    let shown = root.display();
    let home = std::env::home_dir().expect("home");
    let above_home = home.parent().expect("home has a parent").display().to_string();
    let home = home.display();
    for mode in [Mode::Manual, Mode::Plan, Mode::Bypass] {
        let approver = ScriptedApprover::new(vec![]);
        let p = Permissions::new(mode, &rules(&[], &[], &[]), root.clone(), Some(approver.clone()))
            .unwrap();
        for command in [
            format!("cat {home}/.kloop/config.toml"),
            format!("cat {home}/.kloop/skills/x/SKILL.md"),
            "cat ${HOME}/.kloop/mcp-oauth.json".to_string(),
            "cat ~/{sub,}/.kloop/config.toml".into(),
            "cat /Users/*/.kloop/config.toml".into(),
            "cat /c/Users/u/.kloop/config.toml".into(),
            format!("cat /System/Volumes/Data{home}/.kloop/config.toml"),
            format!("cat {shown}/link/../.kloop/notes.md"),
            "cd ~ && cat .kloop/config.toml".into(),
            "cd ~/.kloop/skills && cat ../config.toml".into(),
            "ln -s ~/.kloop/skills /tmp/kx".into(),
            "tar -C ~ -cf - .kloop".into(),
            "echo /home/*/.kloop/config.toml | xargs cat".into(),
            // 会写或未知的命令不定位：可能自带换目录、换根的选项。
            "cp /tmp/payload.md .kloop/rules/style.md".into(),
            format!("cp /tmp/payload.md {shown}/.kloop/rules/style.md"),
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
            p.check(
                "read_file",
                &json!({"path": format!("{home}/.kloop/config.toml")}),
                0
            )
            .await
            .is_err(),
            "{mode:?}"
        );
        assert!(p.read_path_blocked(Path::new(&format!("{home}/.kloop/skills/x/SKILL.md"))));
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
