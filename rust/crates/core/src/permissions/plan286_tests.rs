// plan 286：`rm` 只删 `/tmp`、`$TMPDIR` 里的东西时不算危险命令；其它形状照旧询问。

#[cfg(unix)]
fn plan286_hazard(command: &str, cwd: &Path) -> Option<&'static str> {
    CallFacts::gather("bash", &bash(command), cwd, None)
        .hazard("bash")
        .map(|hazard| hazard.tag)
}

/// `$TMPDIR` 的值，去掉末尾的 `/`，拼路径时不出现 `//`。
#[cfg(unix)]
fn plan286_tmpdir() -> String {
    std::env::temp_dir()
        .to_str()
        .unwrap()
        .trim_end_matches('/')
        .to_string()
}

#[cfg(unix)]
#[test]
fn plan286_literal_scratch_removals_are_not_hazards() {
    let tmpdir = plan286_tmpdir();
    for command in [
        "rm -f /tmp/kloop-plan286-loop.log".to_string(),
        "rm -rf /tmp/kloop-plan286-probe && mkdir /tmp/kloop-plan286-probe".to_string(),
        "rm -rf -- /tmp/kloop-plan286-a /tmp/kloop-plan286-b".to_string(),
        "rm --recursive --force /tmp/kloop-plan286-a".to_string(),
        format!("rm -f {tmpdir}/kloop-plan286-fixture/expected.txt"),
        "timeout 5 rm -rf /tmp/kloop-plan286-a".to_string(),
        "cd /tmp && rm -rf /tmp/kloop-plan286-probe".to_string(),
        "rm -f /tmp/kloop-plan286-loop.log; cargo test -p kloop-core 2>&1 | tail -5".to_string(),
        // 只有命令位置上的 `xargs`/`sudo` 才取消豁免；同名的临时文件不算（第二轮复审 P3）。
        "rm -f /tmp/xargs /tmp/sudo".to_string(),
    ] {
        assert_eq!(plan286_hazard(&command, &test_cwd()), None, "{command}");
    }
}

#[cfg(unix)]
#[test]
fn plan286_other_removals_stay_destructive() {
    let tmpdir = plan286_tmpdir();
    for command in [
        // 临时根本身。
        "rm -rf /tmp".to_string(),
        "rm -rf /tmp/".to_string(),
        "rm -rf /tmp/.".to_string(),
        "rm -rf /private/tmp".to_string(),
        format!("rm -rf {tmpdir}"),
        format!("rm -rf {tmpdir}/"),
        // glob、花括号：落点在运行时才定。
        "rm -rf /tmp/*".to_string(),
        "rm -rf /tmp/kloop-plan286-?".to_string(),
        "rm -rf /tmp/kloop-plan286-{a,b}".to_string(),
        // `..` 逃出临时目录。
        "rm -rf /tmp/../etc".to_string(),
        "rm -rf /tmp/kloop-plan286-missing/../../etc".to_string(),
        // 混入非临时目标、相对路径。
        "rm -rf /tmp/kloop-plan286-a build".to_string(),
        "rm -rf /tmp/kloop-plan286-a ~/notes".to_string(),
        "rm -rf build".to_string(),
        "cd /tmp && rm -rf kloop-plan286-probe".to_string(),
        // 操作数后面的选项、`--`：BSD 与 GNU 语义不同。
        "rm /tmp/kloop-plan286-a -rf".to_string(),
        "rm -rf /tmp/kloop-plan286-a -- /tmp/kloop-plan286-b".to_string(),
        // 提权。
        "sudo rm -rf /tmp/kloop-plan286-a".to_string(),
        "sudo -u root rm -rf /tmp/kloop-plan286-a".to_string(),
        // 未知值：变量、xargs、循环绑定。
        "rm -rf \"$TMPDIR/kloop-plan286-a\"".to_string(),
        "printf '/tmp/kloop-plan286-a\\n' | xargs rm -rf".to_string(),
        "for d in /tmp/kloop-plan286-a; do rm -rf \"$d\"; done".to_string(),
        // xargs 从 stdin 往后追加操作数：固定参数里的临时路径不能替追加的那些作保。
        "printf 'src\\n' | xargs rm -rf /tmp/kloop-plan286-a".to_string(),
        "find . -name '*.o' -print0 | xargs -0 rm -f /tmp/kloop-plan286-a".to_string(),
        "printf 'src\\n' | timeout 5 xargs rm -rf /tmp/kloop-plan286-a".to_string(),
        "timeout 5 sudo rm -rf /tmp/kloop-plan286-a".to_string(),
    ] {
        assert_eq!(
            plan286_hazard(&command, &test_cwd()),
            Some("destructive"),
            "{command}"
        );
    }
}

/// 路径中间的符号链接指出临时目录时按真实落点判断；工作区本身在临时目录里时，删工作区内的
/// 东西照旧询问，删它旁边的临时文件不问。
#[cfg(unix)]
#[test]
fn plan286_resolution_decides_where_the_removal_lands() {
    let base = std::env::temp_dir().join(crate::resource_id::fresh("plan286-").unwrap());
    let workspace = base.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::os::unix::fs::symlink("/usr", base.join("link")).unwrap();
    let base_text = base.to_str().unwrap();
    let workspace_text = workspace.to_str().unwrap();

    let escaping = format!("rm -rf {base_text}/link/share");
    assert_eq!(plan286_hazard(&escaping, &test_cwd()), Some("destructive"));
    let inside_workspace = format!("rm -rf {workspace_text}/src");
    assert_eq!(plan286_hazard(&inside_workspace, &workspace), Some("destructive"));
    let whole_workspace = format!("rm -rf {workspace_text}");
    assert_eq!(plan286_hazard(&whole_workspace, &workspace), Some("destructive"));
    // 工作区的上级目录：递归删除会把工作区一起删掉（复审 P2）。
    let workspace_parent = format!("rm -rf {base_text}");
    assert_eq!(plan286_hazard(&workspace_parent, &workspace), Some("destructive"));
    let beside_workspace = format!("rm -rf {base_text}/scratch");
    assert_eq!(plan286_hazard(&beside_workspace, &workspace), None);

    std::fs::remove_dir_all(&base).unwrap();
}

/// `$TMPDIR` 被设成 `/`、家目录或家目录的祖先时不算临时目录。
#[cfg(unix)]
#[test]
fn plan286_a_misconfigured_tmpdir_is_not_scratch() {
    let home = std::fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join(crate::resource_id::fresh("plan286-home-").unwrap());
    std::fs::create_dir_all(&home).unwrap();
    let tmp = std::fs::canonicalize("/tmp").unwrap();
    let roots = facts::scratch_roots_from(
        &[
            PathBuf::from("/"),
            home.clone(),
            home.parent().unwrap().to_path_buf(),
            PathBuf::from("/tmp"),
            tmp.clone(),
            PathBuf::from("/kloop-plan286-does-not-exist"),
        ],
        Some(&home),
    );
    assert_eq!(
        roots.iter().map(|root| root.canonical.clone()).collect::<Vec<_>>(),
        vec![tmp]
    );
    std::fs::remove_dir_all(&home).unwrap();
}

/// manual 下，清理临时日志再跑测试的整条命令不再问危险命令；删工作区里的目录照旧问。
#[cfg(unix)]
#[tokio::test]
async fn plan286_the_gate_no_longer_asks_for_scratch_cleanup() {
    let command = "rm -f /tmp/kloop-plan286-loop.log && cargo test -p kloop-core 2>&1 | tail -5";
    let approver = ScriptedApprover::new(vec![]);
    let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
    p.check_call("bash", &bash(command), 0, true).await.expect(command);
    assert_eq!(approver.ask_count(), 0);

    let command = "rm -rf build && cargo test -p kloop-core 2>&1 | tail -5";
    let approver = ScriptedApprover::new(vec![]);
    let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
    p.check_call("bash", &bash(command), 0, true).await.expect_err(command);
    assert_eq!(approver.ask_count(), 1);
}

/// `$TMPDIR` 嵌在 `/tmp` 下时，`/tmp` 里通向它的目录和它本身都不是可随手删的临时文件（复审 P2）；
/// 它里面的东西照旧豁免。
#[cfg(unix)]
#[test]
fn plan286_a_tmpdir_nested_in_tmp_is_not_scratch_itself() {
    let tmp = std::fs::canonicalize("/tmp").unwrap();
    let user = tmp.join(crate::resource_id::fresh("plan286-nested-").unwrap());
    let nested = user.join("T");
    std::fs::create_dir_all(&nested).unwrap();
    let roots = facts::scratch_roots_from(&[tmp.clone(), nested.clone()], None);
    let cwd = test_cwd();
    let canonical = facts::canonical_cwd(&cwd);
    let root = facts::PathRoot {
        cwd: &cwd,
        canonical: &canonical,
    };
    let text = |path: &Path| path.to_str().unwrap().to_string();
    for (operand, scratch) in [
        (text(&user), false),
        (text(&nested), false),
        (text(&nested.join("probe.log")), true),
        (text(&user.join("sibling")), true),
    ] {
        assert_eq!(
            facts::operand_is_scratch(&operand, root, &roots),
            scratch,
            "{operand}"
        );
    }
    std::fs::remove_dir_all(&user).unwrap();
}

/// `$TMPDIR` 本身是符号链接时，递归删除链接所在的上级目录会移除会话正用着的临时目录入口，
/// 不豁免；链接旁边的东西照旧豁免（第二轮复审 P3）。
#[cfg(unix)]
#[test]
fn plan286_a_symlinked_tmpdir_entry_is_guarded() {
    let tmp = std::fs::canonicalize("/tmp").unwrap();
    let job = tmp.join(crate::resource_id::fresh("plan286-job-").unwrap());
    let elsewhere = std::fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join(crate::resource_id::fresh("plan286-real-").unwrap());
    std::fs::create_dir_all(&job).unwrap();
    std::fs::create_dir_all(&elsewhere).unwrap();
    let link = job.join("T");
    std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
    // 用 `/tmp/...` 的原拼写给出 `$TMPDIR`，和环境变量里常见的写法一样。
    let spelled_job = Path::new("/tmp").join(job.file_name().unwrap());
    let roots = facts::scratch_roots_from(&[PathBuf::from("/tmp"), spelled_job.join("T")], None);
    let cwd = test_cwd();
    let canonical = facts::canonical_cwd(&cwd);
    let root = facts::PathRoot {
        cwd: &cwd,
        canonical: &canonical,
    };
    let text = |path: &Path| path.to_str().unwrap().to_string();
    for (operand, scratch) in [
        (text(&spelled_job), false),
        (text(&job), false),
        (text(&link), false),
        (text(&job.join("sibling")), true),
        (text(&elsewhere.join("probe.log")), true),
    ] {
        assert_eq!(
            facts::operand_is_scratch(&operand, root, &roots),
            scratch,
            "{operand}"
        );
    }
    std::fs::remove_dir_all(&job).unwrap();
    std::fs::remove_dir_all(&elsewhere).unwrap();
}

/// `$TMPDIR` 的拼写里，`..` 跟在符号链接后面：目录项的真实上级要先经过链接再回退，词法折叠会算错
/// （第三轮复审 P3）。删除真实上级不豁免。
#[cfg(unix)]
#[test]
fn plan286_a_tmpdir_spelled_through_a_link_and_dotdot_is_guarded() {
    let tmp = std::fs::canonicalize("/tmp").unwrap();
    let id = crate::resource_id::fresh("plan286-dotdot-").unwrap();
    let base = tmp.join(&id);
    let elsewhere = std::fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join(format!("{id}-real"));
    std::fs::create_dir_all(base.join("a")).unwrap();
    std::fs::create_dir_all(base.join("other/y")).unwrap();
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::os::unix::fs::symlink(base.join("other/y"), base.join("a/link")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, base.join("other/T")).unwrap();
    // 物理上：`/tmp/<id>/a/link/..` 是 `/tmp/<id>/other`，它下面的 `T` 才是 `$TMPDIR` 的入口。
    let spelled = Path::new("/tmp").join(&id).join("a/link/../T");
    let roots = facts::scratch_roots_from(&[PathBuf::from("/tmp"), spelled], None);
    let cwd = test_cwd();
    let canonical = facts::canonical_cwd(&cwd);
    let root = facts::PathRoot {
        cwd: &cwd,
        canonical: &canonical,
    };
    let text = |path: &Path| path.to_str().unwrap().to_string();
    for (operand, scratch) in [
        (text(&base.join("other")), false),
        (text(&base.join("a")), true),
        (text(&elsewhere.join("probe.log")), true),
    ] {
        assert_eq!(
            facts::operand_is_scratch(&operand, root, &roots),
            scratch,
            "{operand}"
        );
    }
    std::fs::remove_dir_all(&base).unwrap();
    std::fs::remove_dir_all(&elsewhere).unwrap();
}
