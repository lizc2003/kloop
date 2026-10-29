// plan 243：敏感路径判定的补漏回归（递归搜索拼写、glob 拼写、`~`/`$HOME` 写目标）。

fn plan243_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir()
        .join(crate::resource_id::fresh(&format!("kloop-plan243-{label}-")).expect("fresh id"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

#[tokio::test]
async fn plan243_recursive_searches_are_hard_blocked_over_credentials() {
    let env_root = plan243_root("recursive-env");
    std::fs::write(env_root.join(".env"), "API_KEY=x").unwrap();
    let credential_root = plan243_root("recursive-credential");
    std::fs::create_dir_all(credential_root.join(".ssh")).unwrap();

    for mode in [Mode::Manual, Mode::Plan, Mode::Bypass] {
        for command in [
            format!("grep -rn KEY {}", env_root.display()),
            format!("grep -Rl KEY {}", env_root.display()),
            format!("grep -d recurse KEY {}", env_root.display()),
            format!("grep --directories=recurse KEY {}", env_root.display()),
            format!("rg -. KEY {}", env_root.display()),
            format!("rg --unrestricted KEY {}", env_root.display()),
            format!("rg -uu KEY {}", env_root.display()),
            format!("rg --hidden KEY {}", env_root.display()),
            format!("grep -rn KEY {}", credential_root.display()),
        ] {
            let approver = ScriptedApprover::new(vec![]);
            let p = gate(mode, rules(&[], &[], &[]), approver.clone());
            let error = p
                .check_call("bash", &bash(&command), 0, true)
                .await
                .expect_err(&command);
            assert!(error.contains("sensitive path"), "{command}: {error}");
            assert_eq!(approver.ask_count(), 0, "{command}");
        }
    }

    // 非递归调用照旧放行：搜索根里有没有 `.env` 与单文件读取无关。
    let approver = ScriptedApprover::new(vec![]);
    let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
    p.check_call(
        "bash",
        &bash(&format!("grep -n KEY {}/notes.txt", env_root.display())),
        0,
        true,
    )
    .await
    .unwrap();
    assert_eq!(approver.ask_count(), 0);

    let _ = std::fs::remove_dir_all(&env_root);
    let _ = std::fs::remove_dir_all(&credential_root);
}

#[tokio::test]
async fn plan243_glob_spellings_cannot_reach_sensitive_names() {
    let root = plan243_root("glob");
    let root = root.display().to_string();

    for mode in [Mode::Manual, Mode::Plan, Mode::Bypass] {
        for command in [
            format!("cat {root}/.en?"),
            format!("cat {root}/.gi?/config"),
            format!("cat {root}/.s*/id_rsa"),
        ] {
            let approver = ScriptedApprover::new(vec![]);
            let p = gate(mode, rules(&[], &[], &[]), approver.clone());
            let error = p
                .check_call("bash", &bash(&command), 0, true)
                .await
                .expect_err(&command);
            assert!(error.contains("sensitive path"), "{command}: {error}");
            assert_eq!(approver.ask_count(), 0, "{command}");
        }
    }

    // 拼不出敏感名字的 glob 不打扰：`.c*` 命不中名单里的任何一项。
    let approver = ScriptedApprover::new(vec![]);
    let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
    for command in [format!("cat {root}/.c*"), format!("ls {root}/*.txt")] {
        p.check_call("bash", &bash(&command), 0, true)
            .await
            .unwrap_or_else(|error| panic!("{command}: {error}"));
    }
    assert_eq!(approver.ask_count(), 0);

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn plan243_home_spelled_write_targets_follow_the_redirect_checks() {
    for command in [
        "echo x >> ~/.bashrc",
        "printf x > $HOME/.gitconfig",
        "echo x >> ${HOME}/.zshrc",
    ] {
        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        p.check_call("bash", &bash(command), 0, false)
            .await
            .unwrap_or_else(|error| panic!("{command}: {error}"));
        assert_eq!(approver.ask_count(), 1, "{command}");
        assert_eq!(
            approver.asked()[0].notice.as_deref(),
            Some("sensitive redirect path"),
            "{command}"
        );
    }

    // 展开后的绝对目标参与 deny 匹配，bypass 也挡得住。
    for command in ["echo x >> ~/.bashrc", "printf x > $HOME/.gitconfig"] {
        let p = gate(
            Mode::Bypass,
            rules(
                &[],
                &["bash_write(**/.bashrc)", "bash_write(**/.gitconfig)"],
                &[],
            ),
            ScriptedApprover::new(vec![]),
        );
        let error = p
            .check_call("bash", &bash(command), 0, true)
            .await
            .expect_err(command);
        assert!(error.contains("deny permission rule"), "{command}: {error}");
    }
}

#[tokio::test]
async fn plan243_stream_devices_are_not_sensitive_targets() {
    let approver = ScriptedApprover::new(vec![]);
    let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
    for command in [
        "echo x > /dev/null",
        "echo x > /dev/stdout",
        "echo x > /dev/stderr",
        "echo x > /dev/tty",
    ] {
        p.check_call("bash", &bash(command), 0, true)
            .await
            .unwrap_or_else(|error| panic!("{command}: {error}"));
    }
    assert_eq!(
        approver.ask_count(),
        0,
        "stream devices have no persistent effect"
    );

    // 真正的设备写入仍然逐次确认。
    let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
    let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
    p.check_call("bash", &bash("echo x > /dev/disk0"), 0, true)
        .await
        .unwrap();
    assert_eq!(approver.ask_count(), 1);
    assert_eq!(
        approver.asked()[0].notice.as_deref(),
        Some("sensitive redirect path")
    );
}

#[tokio::test]
async fn plan243_unparsed_search_options_keep_recursion_evidence() {
    let root = plan243_root("unparsed");
    std::fs::write(root.join(".env"), "API_KEY=x").unwrap();
    for command in [
        format!("rg --no-config -. KEY {}", root.display()),
        format!("rg --no-config -uu KEY {}", root.display()),
        format!("grep --no-config -rn KEY {}", root.display()),
        format!("grep --no-config -d recurse KEY {}", root.display()),
    ] {
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(Mode::Bypass, rules(&[], &[], &[]), approver.clone());
        let error = p
            .check_call("bash", &bash(&command), 0, true)
            .await
            .expect_err(&command);
        assert!(error.contains("sensitive path"), "{command}: {error}");
        assert_eq!(approver.ask_count(), 0, "{command}");
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn plan243_glob_read_redirects_are_screened() {
    let root = plan243_root("read-redirect");
    let root = root.display().to_string();
    for command in [
        format!("cat < {root}/.en?"),
        format!("cat 0< {root}/.gi?/config"),
    ] {
        let p = gate(
            Mode::Bypass,
            rules(&[], &[], &[]),
            ScriptedApprover::new(vec![]),
        );
        let error = p
            .check_call("bash", &bash(&command), 0, true)
            .await
            .expect_err(&command);
        assert!(error.contains("sensitive path"), "{command}: {error}");
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn plan243_quoted_home_suffix_is_decoded() {
    for command in ["echo x > ~/\".bashrc\"", "echo x >> ~/.bas\"hrc\""] {
        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        p.check_call("bash", &bash(command), 0, false)
            .await
            .unwrap_or_else(|error| panic!("{command}: {error}"));
        assert_eq!(approver.ask_count(), 1, "{command}");
        assert_eq!(
            approver.asked()[0].notice.as_deref(),
            Some("sensitive redirect path"),
            "{command}"
        );

        let p = gate(
            Mode::Bypass,
            rules(&[], &["bash_write(**/.bashrc)"], &[]),
            ScriptedApprover::new(vec![]),
        );
        let error = p
            .check_call("bash", &bash(command), 0, true)
            .await
            .expect_err(command);
        assert!(error.contains("deny permission rule"), "{command}: {error}");
    }
}

#[tokio::test]
async fn plan243_text_arguments_are_not_sensitive_reads() {
    let p = gate(
        Mode::Bypass,
        rules(&[], &[], &[]),
        ScriptedApprover::new(vec![]),
    );
    p.check_call("bash", &bash("echo \"$PWD/.env\""), 0, true)
        .await
        .unwrap();
}

#[tokio::test]
async fn plan243_negated_class_is_not_cut_short() {
    let root = plan243_root("negated");
    let root = root.display().to_string();
    // `[!]]` 是“排除 ]”：第一个 ] 是成员，`.e[!]]v` 能拼出 `.env`。
    let command = format!("cat {root}/.e[!]]v");
    let p = gate(
        Mode::Bypass,
        rules(&[], &[], &[]),
        ScriptedApprover::new(vec![]),
    );
    let error = p
        .check_call("bash", &bash(&command), 0, true)
        .await
        .expect_err(&command);
    assert!(error.contains("sensitive path"), "{command}: {error}");
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn plan243_unmodeled_braces_keep_the_prefix_exclusion() {
    let root = plan243_root("braces");
    let root = root.display().to_string();
    // 花括号只能展开成 .cache/.config，拼不出任何敏感名。
    let command = format!("ls -d {root}/.c{{ache,onfig}}");
    let p = gate(
        Mode::Manual,
        rules(&[], &[], &[]),
        ScriptedApprover::new(vec![]),
    );
    p.check_call("bash", &bash(&command), 0, true)
        .await
        .unwrap_or_else(|error| panic!("{command}: {error}"));
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn plan243_apostrophe_inside_double_quotes_stays_in_the_name() {
    // 双引号里的单引号是文件名的一部分：deny 必须按真实名字匹配。
    let p = gate(
        Mode::Bypass,
        rules(&[], &["bash_write(**/O'Brien.txt)"], &[]),
        ScriptedApprover::new(vec![]),
    );
    let command = "echo x > \"$HOME/O'Brien.txt\"";
    let error = p
        .check_call("bash", &bash(command), 0, true)
        .await
        .expect_err(command);
    assert!(error.contains("deny permission rule"), "{command}: {error}");
}

#[tokio::test]
async fn plan243_home_prefix_quoting_is_independent_of_the_suffix() {
    // `$HOME` 没有引用，后缀的单引号只对后缀生效：落点仍是 `~/.bashrc`。
    for command in ["echo x >> $HOME/'.bashrc'", "echo x >> $HOME/\".zshrc\""] {
        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        p.check_call("bash", &bash(command), 0, false)
            .await
            .unwrap_or_else(|error| panic!("{command}: {error}"));
        assert_eq!(approver.ask_count(), 1, "{command}");
        assert_eq!(
            approver.asked()[0].notice.as_deref(),
            Some("sensitive redirect path"),
            "{command}"
        );
    }

    let p = gate(
        Mode::Bypass,
        rules(&[], &["bash_write(**/.bashrc)"], &[]),
        ScriptedApprover::new(vec![]),
    );
    let error = p
        .check_call("bash", &bash("echo x >> $HOME/'.bashrc'"), 0, true)
        .await
        .expect_err("quoted suffix");
    assert!(error.contains("deny permission rule"), "{error}");
}

#[tokio::test]
async fn plan243_equivalent_words_have_the_same_write_policy() {
    for target in [
        "~/.bas\\\nhrc",
        "~/.bashrc",
        "~/'.bashrc'",
        "$HOME/.bashrc",
        "${HOME}/.bashrc",
        "\"$HOME/.bashrc\"",
        "\"\"$HOME/.bashrc",
        "''${HOME}/.bashrc",
        "$HOME/'.bashrc'",
        "\"$HOME\"/'.bashrc'",
        "\"\"\"$HOME\"/.bas\"hrc\"",
    ] {
        let command = format!("echo x >> {target}");
        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        p.check_call("bash", &bash(&command), 0, true)
            .await
            .unwrap();
        assert_eq!(approver.ask_count(), 1, "{target}");
        assert_eq!(
            approver.asked()[0].notice.as_deref(),
            Some("sensitive redirect path"),
            "{target}"
        );

        let p = gate(
            Mode::Bypass,
            rules(&[], &["bash_write(**/.bashrc)"], &[]),
            ScriptedApprover::new(vec![]),
        );
        let error = p
            .check_call("bash", &bash(&command), 0, true)
            .await
            .expect_err(target);
        assert!(error.contains("deny permission rule"), "{target}: {error}");
    }
}

#[tokio::test]
async fn plan243_home_concatenation_keeps_the_real_path() {
    let home = std::env::home_dir().unwrap();
    for target in ["\"$HOME.bashrc\"", "${HOME}.bashrc", "\"\"${HOME}.bashrc"] {
        let command = format!("echo x > {target}");
        // 这是 home 的兄弟文件，不能误认成 home 内的 rc 文件。
        let p = gate(
            Mode::Bypass,
            rules(&[], &["bash_write(**/.bashrc)"], &[]),
            ScriptedApprover::new(vec![]),
        );
        p.check_call("bash", &bash(&command), 0, true)
            .await
            .unwrap();

        let deny = format!("bash_write({}.bashrc)", home.display());
        let p = gate(
            Mode::Bypass,
            rules(&[], &[&deny], &[]),
            ScriptedApprover::new(vec![]),
        );
        let error = p
            .check_call("bash", &bash(&command), 0, true)
            .await
            .expect_err(target);
        assert!(error.contains("deny permission rule"), "{target}: {error}");
    }
}

#[tokio::test]
async fn plan243_unknown_writes_keep_explicit_rules_effective() {
    for command in [
        "printf x > \"$TARGET\"",
        "printf x > \"$DIR/notes.txt\"",
        "printf x > *.txt",
        "cd /tmp; printf x > notes.txt",
        "HOME=/tmp; printf x > \"$HOME/notes.txt\"",
        "HOME=/tmp; bash -c 'printf x > \"$HOME/notes.txt\"'",
        "printf -v HOME /tmp; echo x > \"$HOME/notes.txt\"",
        "bash -lc 'printf x > \"$HOME/notes.txt\"'",
        "$CMD 'HOME=/tmp'; echo x > \"$HOME/notes.txt\"",
        "for HOME in /tmp; do echo x > \"$HOME/notes.txt\"; done",
    ] {
        let p = gate(
            Mode::Bypass,
            rules(&[], &["bash_write(**/locked.txt)"], &[]),
            ScriptedApprover::new(vec![]),
        );
        let error = p
            .check_call("bash", &bash(command), 0, true)
            .await
            .expect_err(command);
        assert!(error.contains("deny permission rule"), "{command}: {error}");

        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
        let p = gate(
            Mode::Manual,
            rules(&[], &[], &["bash_write(**/locked.txt)"]),
            approver.clone(),
        );
        p.check_call("bash", &bash(command), 0, true).await.unwrap();
        assert_eq!(approver.ask_count(), 1, "{command}");

        // 没有显式规则、也没有敏感证据时，保留现有沙箱免审批。
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
        p.check_call("bash", &bash(command), 0, true).await.unwrap();
        assert_eq!(approver.ask_count(), 0, "{command}");
    }

    // 求值结果只能收紧风险判断，不能变成文件范围 allow 的证据。
    let approver = ScriptedApprover::new(vec![Decision::Deny]);
    let p = gate(
        Mode::Manual,
        rules(&["bash_write(**)"], &[], &[]),
        approver.clone(),
    );
    assert!(
        p.check_call("bash", &bash("printf x > \"$TARGET\""), 0, false)
            .await
            .is_err()
    );
    assert_eq!(approver.ask_count(), 1);
}

#[tokio::test]
async fn plan243_word_evaluation_is_shared_by_read_arguments_and_redirects() {
    for target in [
        "~/.bas\\\nhrc",
        "\"\"$HOME/.bashrc",
        "${HOME}/'.bashrc'",
        "\"$DIR/.bashrc\"",
        "\".en?\"",
    ] {
        for command in [format!("cat {target}"), format!("cat < {target}")] {
            let p = gate(
                Mode::Bypass,
                rules(&[], &[], &[]),
                ScriptedApprover::new(vec![]),
            );
            let error = p
                .check_call("bash", &bash(&command), 0, true)
                .await
                .expect_err(&command);
            assert!(error.contains("sensitive path"), "{command}: {error}");
        }
    }
}

#[tokio::test]
async fn plan243_parse_limits_do_not_erase_unknown_write_effects() {
    let command = format!("#{}\nprintf x > /tmp/locked.txt", " ".repeat(1_048_576));
    let p = gate(
        Mode::Bypass,
        rules(&[], &["bash_write(**/locked.txt)"], &[]),
        ScriptedApprover::new(vec![]),
    );
    let error = p
        .check_call("bash", &bash(&command), 0, true)
        .await
        .expect_err("unknown effects");
    assert!(error.contains("deny permission rule"), "{error}");
}

#[tokio::test]
async fn plan243_wrapper_environment_changes_keep_narrow_write_rules_effective() {
    use crate::sandbox::ExecutionContext;

    let inner = "bash -c 'printf x > \"$HOME/locked.txt\"'";
    for command in [
        "env HOME=/w/proj",
        "env -- HOME=/w/proj",
        "env -i HOME=/w/proj",
        "env --ignore-environment HOME=/w/proj",
        "env -u HOME HOME=/w/proj",
        "env -uHOME HOME=/w/proj",
        "env --unset=HOME HOME=/w/proj",
        "sudo env HOME=/w/proj",
        "sudo -E env HOME=/w/proj",
        "sudo -u root env HOME=/w/proj",
        "timeout 5 env HOME=/w/proj",
        "timeout -s TERM 5 env HOME=/w/proj",
        "nice -n -10 env HOME=/w/proj",
        "env HOME=\"$DIR\"",
        "env --unsupported value",
        "timeout --unsupported value 5 env HOME=/w/proj",
    ]
    .into_iter()
    .map(|wrapper| format!("{wrapper} {inner}"))
    .chain(["sudo -R /w/proj bash -c 'printf x > /locked.txt'".into()])
    {
        let p = gate(
            Mode::Bypass,
            rules(&[], &["bash_write(/w/proj/locked.txt)"], &[]),
            ScriptedApprover::new(vec![]),
        );
        let error = p
            .check_call_with_execution(
                "bash",
                &bash(&command),
                0,
                ExecutionContext::approval_only(true).with_shell_home(Some("/snapshot/user")),
            )
            .await
            .expect_err(&command);
        assert!(error.contains("deny permission rule"), "{command}: {error}");

        let approver = ScriptedApprover::new(vec![Decision::Allow(ApprovalScope::Once)]);
        let p = gate(
            Mode::Manual,
            rules(&[], &[], &["bash_write(/w/proj/locked.txt)"]),
            approver.clone(),
        );
        p.check_call_with_execution(
            "bash",
            &bash(&command),
            0,
            ExecutionContext::approval_only(true).with_shell_home(Some("/snapshot/user")),
        )
        .await
        .unwrap();
        assert_eq!(approver.ask_count(), 1, "{command}");
    }

    // env 只改子进程：外层重定向与后续兄弟命令仍使用原 HOME。
    for command in [
        format!("env {inner}"),
        "env HOME=/w/proj printf x > \"$HOME/locked.txt\"".into(),
        "env HOME=/w/proj true; printf x > \"$HOME/locked.txt\"".into(),
    ] {
        let p = gate(
            Mode::Bypass,
            rules(&[], &["bash_write(/w/proj/locked.txt)"], &[]),
            ScriptedApprover::new(vec![]),
        );
        p.check_call_with_execution(
            "bash",
            &bash(&command),
            0,
            ExecutionContext::approval_only(true).with_shell_home(Some("/snapshot/user")),
        )
        .await
        .unwrap_or_else(|error| panic!("{command}: {error}"));
    }
}
