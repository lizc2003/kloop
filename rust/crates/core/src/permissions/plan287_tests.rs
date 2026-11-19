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
            proposed_write_dir(&refused, Some(&home), plan287_vars(&[])),
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
            proposed_write_dir(&refused, Some(&home), plan287_vars(&vars)),
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
            plan287_vars(&[("GOPATH", gopath.as_str())])
        ),
        Some(path("gopath/pkg/mod"))
    );
    assert_eq!(
        proposed_write_dir(
            &home.join("go/pkg/mod/x@v1/go.mod"),
            Some(&home),
            plan287_vars(&[("GOMODCACHE", "relative/mod")])
        ),
        Some(home.join("go/pkg/mod"))
    );
}

/// 缓存根是家目录的祖先、`/`，或通不过授权检查时，退回父目录，不扩大。
#[test]
fn plan287_a_cache_root_that_is_too_wide_falls_back_to_the_parent() {
    let home = plan287_home("plan287-wide-");
    let ancestor = home.parent().unwrap().to_str().unwrap().to_string();
    let refused = home.join("project/out/x.o");
    for value in [ancestor.as_str(), "/"] {
        assert_eq!(
            proposed_write_dir(&refused, Some(&home), plan287_vars(&[("GOMODCACHE", value)])),
            Some(home.join("project/out")),
            "{value}"
        );
    }
    let refused = PathBuf::from("/tmp/m*d/x@v1/go.mod");
    assert_eq!(
        proposed_write_dir(&refused, Some(&home), plan287_vars(&[("GOMODCACHE", "/tmp/m*d")])),
        Some(PathBuf::from("/tmp/m*d/x@v1"))
    );
}
