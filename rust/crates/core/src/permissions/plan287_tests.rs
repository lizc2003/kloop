// plan 287：写入被拒落在已知的工具缓存里时提议整个缓存根，其它情况照旧提议父目录。

fn plan287_home(label: &str) -> PathBuf {
    std::env::temp_dir()
        .join(crate::resource_id::fresh(label).unwrap())
        .join("home")
}

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
            proposed_write_dir(&refused, Some(&home), plan287_vars(&[]), |_| false),
            Some(offered),
            "{}",
            refused.display()
        );
    }
}

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
            proposed_write_dir(&refused, Some(&home), plan287_vars(&vars), |_| false),
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
            |_| false
        ),
        Some(path("gopath/pkg/mod"))
    );
    assert_eq!(
        proposed_write_dir(
            &home.join("go/pkg/mod/x@v1/go.mod"),
            Some(&home),
            plan287_vars(&[("GOMODCACHE", "relative/mod")]),
            |_| false
        ),
        Some(home.join("go/pkg/mod"))
    );
}

/// 缓存根是家目录的祖先、`/`，或通不过授权检查时，退回父目录，不扩大；父目录本身都不能授权时
/// 什么都不提议。
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
            |_| false
        ),
        Some(real.join("project"))
    );
    // 被拒文件直接在真实家目录下：父目录就是家目录，什么都不提议。
    assert_eq!(
        proposed_write_dir(&real.join("out.o"), Some(&link), plan287_vars(&[]), |_| false),
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
#[test]
fn plan287_the_refused_location_is_checked_before_lifting() {
    let home = plan287_home("plan287-decided-");
    let cache = home.join(".npm");
    let denied = cache.join("_cacache/locked");
    let refused = denied.join("entry");
    assert_eq!(
        proposed_write_dir(&refused, Some(&home), plan287_vars(&[]), |dir| {
            dir.starts_with(&denied)
        }),
        None
    );
    assert_eq!(
        proposed_write_dir(&refused, Some(&home), plan287_vars(&[]), |dir| dir == cache),
        Some(denied.clone())
    );
    assert_eq!(
        proposed_write_dir(&refused, Some(&home), plan287_vars(&[]), |_| false),
        Some(cache)
    );
}

/// 被拒的就是禁写目录本身（例如重命名它）时，父目录和缓存根都解除不了这次拒绝，什么都不提议
/// （第二轮复审 P2）。
#[test]
fn plan287_a_refusal_of_the_denied_directory_itself_offers_nothing() {
    let home = plan287_home("plan287-denied-self-");
    let denied = home.join(".npm/_cacache/locked");
    assert_eq!(
        proposed_write_dir(&denied, Some(&home), plan287_vars(&[]), |dir| {
            dir.starts_with(&denied)
        }),
        None
    );
}

/// 候选目录经符号链接指向家目录或 `/` 时，按解析后的落点判断：授权时 `with_grants` 会把真实路径
/// 加进可写根（第二轮复审 P2）。
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
            |_| false
        ),
        Some(to_home.join("_cacache/tmp"))
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
            |_| false
        ),
        None
    );
    std::fs::remove_dir_all(&base).unwrap();
}

/// 父目录名里有 glob 字符、写不成规则时，缓存根仍能覆盖这次写入；不在缓存里时什么都不提议
/// （第二轮复审 P3）。
#[test]
fn plan287_an_unnameable_parent_still_lifts_to_the_cache_root() {
    let home = plan287_home("plan287-bracket-");
    assert_eq!(
        proposed_write_dir(
            &home.join(".npm/_cacache/chunk[1]/x"),
            Some(&home),
            plan287_vars(&[]),
            |_| false
        ),
        Some(home.join(".npm"))
    );
    assert_eq!(
        proposed_write_dir(&home.join("out[1]/x.o"), Some(&home), plan287_vars(&[]), |_| false),
        None
    );
}
