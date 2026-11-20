// plan 287：写入被拒落在已知的工具缓存里时提议整个缓存根，其它情况照旧提议父目录。

#[cfg(unix)]
fn plan287_home(label: &str) -> PathBuf {
    std::fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join(crate::resource_id::fresh(label).unwrap())
        .join("home")
}

#[cfg(unix)]
fn plan287_vars(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<std::ffi::OsString> + use<> {
    let vars: Vec<(String, String)> = vars
        .iter()
        .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
        .collect();
    move |name: &str| {
        vars.iter()
            .find(|(known, _)| known == name)
            .map(|(_, value)| value.into())
    }
}

#[cfg(unix)]
#[test]
fn plan287_a_tool_cache_proposes_its_root() {
    let home = plan287_home("plan287-defaults-");
    let user_config = if cfg!(target_os = "macos") {
        home.join("Library/Application Support")
    } else {
        home.join(".config")
    };
    for (refused, offered) in [
        (
            home.join("go/pkg/mod/cache/download/github.com/x/y/@v/v1.5.3.info123.tmp"),
            home.join("go/pkg/mod"),
        ),
        (
            home.join("go/pkg/mod/github.com/x/y@v1.5.3/go.mod"),
            home.join("go/pkg/mod"),
        ),
        (
            user_config.join("go/telemetry/local/upload.token"),
            user_config.join("go/telemetry"),
        ),
        (home.join(".npm/_cacache/tmp/abc"), home.join(".npm")),
        (
            home.join(".cargo/registry/cache/index.crates.io-1/x.crate"),
            home.join(".cargo/registry"),
        ),
        (home.join(".cargo/git/db/x/HEAD"), home.join(".cargo/git")),
        (
            home.join(".m2/repository/org/x/y/1.0/y-1.0.jar"),
            home.join(".m2/repository"),
        ),
        (
            home.join(".gradle/caches/modules-2/files-2.1/x/y.jar"),
            home.join(".gradle/caches"),
        ),
        // 不在任何缓存根下：照旧是父目录。
        (home.join(".tool/tmp/arg0"), home.join(".tool/tmp")),
        (home.join(".cargo/.package-cache"), home.join(".cargo")),
        (home.join("go/bin/tool"), home.join("go/bin")),
    ] {
        assert_eq!(
            proposed_write_dir(&refused, Some(&home), plan287_vars(&[]), |_| false, |_| false),
            Some(offered),
            "{}",
            refused.display()
        );
    }
}

#[cfg(unix)]
#[test]
fn plan287_environment_moves_the_cache_roots() {
    let home = plan287_home("plan287-env-");
    let elsewhere = home.parent().unwrap().join("elsewhere");
    let path = |relative: &str| elsewhere.join(relative);
    let text = |relative: &str| path(relative).to_str().unwrap().to_string();
    let gomodcache = text("gomod");
    let gopath = format!("{}:{}", text("gopath"), text("second"));
    let npm = text("npm-cache");
    let cargo = text("cargo");
    let gradle = text("gradle");
    let xdg = text("xdg");
    let vars = [
        ("GOMODCACHE", gomodcache.as_str()),
        ("npm_config_cache", npm.as_str()),
        ("CARGO_HOME", cargo.as_str()),
        ("GRADLE_USER_HOME", gradle.as_str()),
        ("XDG_CONFIG_HOME", xdg.as_str()),
    ];
    let mut cases = vec![
        (path("gomod/cache/download/x/@v/v1.info"), path("gomod")),
        (path("npm-cache/_cacache/tmp/abc"), path("npm-cache")),
        (path("cargo/registry/cache/x.crate"), path("cargo/registry")),
        (path("gradle/caches/x.jar"), path("gradle/caches")),
        // 环境变量指向别处时，默认位置就不再是缓存根。
        (home.join("go/pkg/mod/cache/download/x/@v/v1.info"), home.join("go/pkg/mod/cache/download/x/@v")),
    ];
    if !cfg!(target_os = "macos") {
        cases.push((path("xdg/go/telemetry/local/x"), path("xdg/go/telemetry")));
    }
    for (refused, offered) in cases {
        assert_eq!(
            proposed_write_dir(&refused, Some(&home), plan287_vars(&vars), |_| false, |_| false),
            Some(offered),
            "{}",
            refused.display()
        );
    }
    // 没有 GOMODCACHE 时取 GOPATH 的第一项；相对路径的值不认。
    assert_eq!(
        proposed_write_dir(
            &path("gopath/pkg/mod/x@v1/go.mod"),
            Some(&home),
            plan287_vars(&[("GOPATH", gopath.as_str())]),
            |_| false,
            |_| false
        ),
        Some(path("gopath/pkg/mod"))
    );
    assert_eq!(
        proposed_write_dir(
            &home.join("go/pkg/mod/x@v1/go.mod"),
            Some(&home),
            plan287_vars(&[("GOMODCACHE", "relative/mod")]),
            |_| false,
            |_| false
        ),
        Some(home.join("go/pkg/mod"))
    );
}

/// 缓存根是家目录的祖先、`/`，或通不过授权检查时，退回父目录，不扩大；父目录本身都不能授权时
/// 什么都不提议。
#[cfg(unix)]
#[test]
fn plan287_a_cache_root_that_is_too_wide_falls_back_to_the_parent() {
    let home = plan287_home("plan287-wide-");
    let ancestor = home.parent().unwrap().to_str().unwrap().to_string();
    let refused = home.join("project/out/x.o");
    for value in [ancestor.as_str(), "/"] {
        assert_eq!(
            proposed_write_dir(
                &refused,
                Some(&home),
                plan287_vars(&[("GOMODCACHE", value)]),
                |_| false,
                |_| false
            ),
            Some(home.join("project/out")),
            "{value}"
        );
    }
    let refused = PathBuf::from("/tmp/m*d/x@v1/go.mod");
    assert_eq!(
        proposed_write_dir(
            &refused,
            Some(&home),
            plan287_vars(&[("GOMODCACHE", "/tmp/m*d")]),
            |_| false,
            |_| false
        ),
        None
    );
}

/// `$HOME` 经符号链接指向真实家目录时，日志报的是真实路径：真实家目录和它的祖先同样不能被
/// 提议，无论是作为缓存根还是作为父目录（复审 P2）。
#[cfg(unix)]
#[test]
fn plan287_a_symlinked_home_is_still_home() {
    let base = std::fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join(crate::resource_id::fresh("plan287-link-").unwrap());
    let real = base.join("real-home");
    std::fs::create_dir_all(real.join("project")).unwrap();
    let link = base.join("home-link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let link_text = link.to_str().unwrap().to_string();

    // 缓存根就是家目录（经链接拼写）：解析后的根是真实家目录，退回父目录。
    assert_eq!(
        proposed_write_dir(
            &real.join("project/out.o"),
            Some(&link),
            plan287_vars(&[("GOMODCACHE", link_text.as_str())]),
            |_| false,
            |_| false
        ),
        Some(real.join("project"))
    );
    // 被拒文件直接在真实家目录下：父目录就是家目录，什么都不提议。
    assert_eq!(
        proposed_write_dir(&real.join("out.o"), Some(&link), plan287_vars(&[]), |_| false, |_| false),
        None
    );
    assert!(!grantable_write_dir(&real, Some(&link)));
    assert!(!grantable_write_dir(&base, Some(&link)));
    assert!(grantable_write_dir(&real.join("project"), Some(&link)));
    std::fs::remove_dir_all(&base).unwrap();
}

/// 上提之前先看原始落点（复审 P2）：策略已经决定了被拒路径所在目录的写权限（例如缓存里某个
/// 子目录被显式禁写）时，授权缓存根也解除不了这次拒绝，什么都不提议；只有缓存根本身被策略
/// 决定时，退回父目录。
#[cfg(unix)]
#[test]
fn plan287_the_refused_location_is_checked_before_lifting() {
    let home = plan287_home("plan287-decided-");
    let cache = home.join(".npm");
    let denied = cache.join("_cacache/locked");
    let refused = denied.join("entry");
    assert_eq!(
        proposed_write_dir(&refused, Some(&home), plan287_vars(&[]), |dir| {
            dir.starts_with(&denied)
        },
        |_| false),
        None
    );
    assert_eq!(
        proposed_write_dir(&refused, Some(&home), plan287_vars(&[]), |dir| dir == cache, |_| false),
        Some(denied.clone())
    );
    assert_eq!(
        proposed_write_dir(&refused, Some(&home), plan287_vars(&[]), |_| false, |_| false),
        Some(cache)
    );
}

/// 被拒的就是禁写目录本身（例如重命名它）时，父目录和缓存根都解除不了这次拒绝，什么都不提议
/// （第二轮复审 P2）。
#[cfg(unix)]
#[test]
fn plan287_a_refusal_of_the_denied_directory_itself_offers_nothing() {
    let home = plan287_home("plan287-denied-self-");
    let denied = home.join(".npm/_cacache/locked");
    assert_eq!(
        proposed_write_dir(&denied, Some(&home), plan287_vars(&[]), |dir| {
            dir.starts_with(&denied)
        },
        |_| false),
        None
    );
}

/// 候选目录经符号链接指向家目录或 `/` 时，按解析后的落点判断，且提议返回的也是物理路径。
#[cfg(unix)]
#[test]
fn plan287_an_aliased_candidate_is_judged_where_it_lands() {
    let base = std::fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join(crate::resource_id::fresh("plan287-alias-").unwrap());
    let home = base.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let to_home = base.join("cache-to-home");
    std::os::unix::fs::symlink(&home, &to_home).unwrap();
    let to_root = base.join("cache-to-root");
    std::os::unix::fs::symlink("/", &to_root).unwrap();

    // 缓存根按链接拼写命中，但它落在家目录上：退回父目录（链接下一级，落在家目录里面）。
    let cache = to_home.to_str().unwrap().to_string();
    assert_eq!(
        proposed_write_dir(
            &to_home.join("_cacache/tmp/x"),
            Some(&home),
            plan287_vars(&[("npm_config_cache", cache.as_str())]),
            |_| false,
            |_| false
        ),
        Some(home.join("_cacache/tmp"))
    );
    assert!(!grantable_write_dir(&to_home, Some(&home)));
    // 落在 `/` 上的链接本身不能授权；被拒文件直接在它下面时什么都不提议。
    assert!(!grantable_write_dir(&to_root, Some(&home)));
    let cache = to_root.to_str().unwrap().to_string();
    assert_eq!(
        proposed_write_dir(
            &to_root.join("x"),
            Some(&home),
            plan287_vars(&[("npm_config_cache", cache.as_str())]),
            |_| false,
            |_| false
        ),
        None
    );
    std::fs::remove_dir_all(&base).unwrap();
}

/// 父目录名里有 glob 字符、写不成规则时，缓存根仍能覆盖这次写入；不在缓存里时什么都不提议
/// （第二轮复审 P3）。
#[cfg(unix)]
#[test]
fn plan287_an_unnameable_parent_still_lifts_to_the_cache_root() {
    let home = plan287_home("plan287-bracket-");
    assert_eq!(
        proposed_write_dir(
            &home.join(".npm/_cacache/chunk[1]/x"),
            Some(&home),
            plan287_vars(&[]),
            |_| false,
            |_| false
        ),
        Some(home.join(".npm"))
    );
    assert_eq!(
        proposed_write_dir(&home.join("out[1]/x.o"), Some(&home), plan287_vars(&[]), |_| false, |_| false),
        None
    );
    assert_eq!(
        proposed_write_dir(Path::new("relative/out"), Some(&home), plan287_vars(&[]), |_| false, |_| false),
        None
    );
}

/// 已记住的目录不能改指任意新位置。沙箱应用 verdict 时还要处理在途的目录变化。
#[cfg(target_os = "macos")]
#[tokio::test]
async fn plan287_a_remembered_grant_that_now_leads_home_is_dropped() {
    let home = std::env::home_dir().expect("home");
    let base = std::fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join(crate::resource_id::fresh("plan287-moved-").unwrap());
    let kept = base.join("cache");
    std::fs::create_dir_all(&kept).unwrap();
    let moved = base.join("moved-cache");
    std::os::unix::fs::symlink(&home, &moved).unwrap();
    let redirected = base.join("redirected-cache");
    let unrelated = base.join("ordinary-dir");
    std::fs::create_dir(&unrelated).unwrap();
    std::os::unix::fs::symlink(&unrelated, &redirected).unwrap();
    let rule = |dir: &Path| format!("sandbox_write({})", dir.display());
    let approver = ScriptedApprover::new(vec![]);
    let p = gate(
        Mode::Manual,
        rules(&[&rule(&kept), &rule(&moved), &rule(&redirected)], &[], &[]),
        approver.clone(),
    );
    let policy = crate::sandbox::SandboxPolicy::workspace(&test_cwd(), &[], false);
    let verdict = p
        .check_call_with_execution(
            "bash",
            &bash("printf x > out.txt"),
            0,
            crate::sandbox::ExecutionContext::sandboxed(Some(&policy)),
            None,
        )
        .await
        .unwrap();
    assert_eq!(verdict.grants.writable, vec![kept.clone()]);
    let refuses = |policy: &crate::sandbox::SandboxPolicy, path: &Path| {
        !policy.already_decides_writes(path)
    };
    // 用独立的策略判断这次授权贡献的根，避免测试进程本来就能写临时目录。
    let ungranted = crate::sandbox::SandboxPolicy {
        writable_roots: Vec::new(),
        ..policy
    };
    assert!(!refuses(&ungranted.with_grants(&verdict.grants), &kept));
    std::fs::rename(&kept, base.join("old-cache")).unwrap();
    std::os::unix::fs::symlink(&unrelated, &kept).unwrap();
    let applied = ungranted.with_grants(&verdict.grants);
    assert!(refuses(&applied, &kept));
    assert!(refuses(&applied, &unrelated));

    let denial = crate::sandbox::SandboxDenial {
        kind: crate::sandbox::DenialKind::Write,
        evidence: "file-write-create".into(),
        writes: Vec::new(),
    };
    let covered_by_moved = CapabilityRequest::Write {
        dirs: vec![moved.join("sub")],
    };
    let escalation = p
        .escalate_capability("touch x", &denial, &covered_by_moved, 0)
        .await;
    assert_ne!(
        escalation.outcome,
        EscalationOutcome::Approved(EscalationApproval::Remembered),
        "a grant that now leads home does not answer for its subdirectories"
    );
    assert_eq!(approver.ask_count(), 1);
    std::fs::remove_dir_all(&base).unwrap();
}

/// 被拒的是禁写路径或受保护子路径的祖先（拒绝的正是改名）时，授权哪一级都解除不了，什么都不提议；
/// 在它里面新建文件照旧可以提议（第三轮复审 P2）。
#[cfg(unix)]
#[test]
fn plan287_a_refused_rename_of_a_guarded_ancestor_offers_nothing() {
    // 不能放在临时目录下：那里本来就是工作区策略的可写根，策略已经“决定”了它的写权限。
    let home = PathBuf::from("/Users/kloop-plan287-nobody");
    let locked = home.join(".npm/_cacache/locked");
    let policy = crate::sandbox::SandboxPolicy::workspace(&test_cwd(), &[], false)
        .with_denied_write_path(&locked);
    let offer = |refused: &Path| {
        proposed_write_dir(
            refused,
            Some(&home),
            plan287_vars(&[]),
            |dir| policy.already_decides_writes(dir),
            |path| policy.guards_rename(path),
        )
    };
    assert!(policy.guards_rename(&home.join(".npm/_cacache")));
    assert!(!policy.guards_rename(&home.join(".npm/_cacache/other")));
    assert_eq!(offer(&home.join(".npm/_cacache")), None);
    assert_eq!(offer(&locked), None);
    assert_eq!(offer(&home.join(".npm/_cacache/new-entry")), Some(home.join(".npm")));
}

/// 带尾斜杠、`.` 或 `//` 的旧写授权不是规范字面量：按别名过滤，不进入可写根，也不把对应目录算作已记住。
#[cfg(unix)]
#[tokio::test]
async fn plan287_non_canonical_write_grant_spellings_are_dropped() {
    let base = std::fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join(crate::resource_id::fresh("plan287-spelling-").unwrap());
    let kept = base.join("cache");
    std::fs::create_dir_all(&kept).unwrap();
    let kept_text = kept.display();
    let base_text = base.display();
    let trailing_slash = PathBuf::from(format!("{kept_text}/"));
    let dot_segment = PathBuf::from(format!("{base_text}/./cache"));
    let double_slash = PathBuf::from(format!("{base_text}//cache"));

    assert!(usable_write_grant(&kept));
    for bad in [&trailing_slash, &dot_segment, &double_slash] {
        assert!(!usable_write_grant(bad), "{}", bad.display());
    }

    let policy = crate::sandbox::SandboxPolicy::workspace(&test_cwd(), &[], false);
    #[cfg(target_os = "macos")]
    let sandbox_policy = policy.clone();
    let ungranted = crate::sandbox::SandboxPolicy {
        writable_roots: Vec::new(),
        ..policy
    };
    let applied = ungranted.with_grants(&SandboxGrants {
        network: false,
        writable: vec![
            trailing_slash.clone(),
            dot_segment.clone(),
            double_slash.clone(),
        ],
    });
    assert!(applied.writable_roots.is_empty());
    assert!(!applied.already_decides_writes(&kept));

    let rule = |dir: &Path| format!("sandbox_write({})", dir.display());
    let approver = ScriptedApprover::new(vec![]);
    let p = gate(
        Mode::Manual,
        rules(
            &[
                &rule(&trailing_slash),
                &rule(&dot_segment),
                &rule(&double_slash),
            ],
            &[],
            &[],
        ),
        approver.clone(),
    );
    #[cfg(target_os = "macos")]
    {
        let verdict = p
            .check_call_with_execution(
                "bash",
                &bash("printf x > out.txt"),
                0,
                crate::sandbox::ExecutionContext::sandboxed(Some(&sandbox_policy)),
                None,
            )
            .await
            .unwrap();
        assert!(verdict.grants.writable.is_empty());
    }
    let denial = crate::sandbox::SandboxDenial {
        kind: crate::sandbox::DenialKind::Write,
        evidence: format!("file-write-create {kept_text}/x"),
        writes: Vec::new(),
    };
    let request = CapabilityRequest::Write {
        dirs: vec![kept.clone()],
    };
    let escalation = p.escalate_capability("touch x", &denial, &request, 0).await;
    assert_ne!(
        escalation.outcome,
        EscalationOutcome::Approved(EscalationApproval::Remembered),
        "non-canonical spellings must trigger fresh approval for the physical path"
    );
    assert_eq!(approver.ask_count(), 1);
    std::fs::remove_dir_all(&base).unwrap();
}

/// 经符号链接别名传入的写拒绝，解析到真实落点后仍需过策略决定与改名保护检查，
/// 既不直接提议策略已决定的目录，也不越过它上提到缓存根。
#[cfg(unix)]
#[test]
fn plan287_aliased_refusals_still_check_policy_where_they_land() {
    let base = std::fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join(crate::resource_id::fresh("plan287-aliased-policy-").unwrap());
    let home = base.join("home");
    let real = base.join("real-cache");
    let locked = real.join("locked");
    let granted = real.join("granted");
    let guarded = real.join("guarded");
    let guarded_locked = guarded.join("locked");
    let normal = real.join("normal");
    for dir in [&home, &locked, &granted, &guarded_locked, &normal] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let alias = base.join("alias-cache");
    std::os::unix::fs::symlink(&real, &alias).unwrap();

    let policy = crate::sandbox::SandboxPolicy {
        writable_roots: Vec::new(),
        ..crate::sandbox::SandboxPolicy::workspace(&test_cwd(), &[], false)
    }
    .with_denied_write_path(&locked)
    .with_denied_write_path(&guarded_locked)
    .with_grants(&SandboxGrants {
        network: false,
        writable: vec![granted.clone()],
    });
    let cache_text = alias.to_str().unwrap().to_string();
    let offer = |refused: &Path, cache: Option<&str>, is_unlink: bool| {
        let vars = match cache {
            Some(cache) => plan287_vars(&[("npm_config_cache", cache)]),
            None => plan287_vars(&[]),
        };
        proposed_write_dir(
            refused,
            Some(&home),
            vars,
            |dir| policy.already_decides_writes(dir),
            |path| is_unlink && policy.guards_rename(path),
        )
    };

    for cache in [Some(cache_text.as_str()), None] {
        // 父目录经别名落在禁写目录或已授权可写根上：不能提议，也不能越过它上提缓存根。
        assert_eq!(
            offer(&alias.join("locked/entry"), cache, /*is_unlink*/ false),
            None
        );
        assert_eq!(
            offer(&alias.join("granted/entry"), cache, /*is_unlink*/ false),
            None
        );
        // 被拒条目自身的物理落点就是禁写目录。
        assert_eq!(
            offer(&alias.join("locked"), cache, /*is_unlink*/ false),
            None
        );
        // 被拒条目自身的物理落点命中改名保护：unlink 拒绝不提议，其他写操作照常提议。
        assert_eq!(
            offer(&alias.join("guarded"), cache, /*is_unlink*/ true),
            None
        );
        assert_eq!(
            offer(&alias.join("guarded"), cache, /*is_unlink*/ false),
            Some(real.clone())
        );
    }
    assert_eq!(
        offer(
            &alias.join("normal/entry"),
            Some(cache_text.as_str()),
            /*is_unlink*/ false
        ),
        Some(real.clone())
    );
    assert_eq!(
        offer(&alias.join("normal/entry"), None, /*is_unlink*/ false),
        Some(normal)
    );
    std::fs::remove_dir_all(&base).unwrap();
}
