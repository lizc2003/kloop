struct LiteralProbe {
    root: std::path::PathBuf,
    ctx: crate::tools::ToolCtx,
    asked: Arc<AtomicUsize>,
}

impl LiteralProbe {
    fn new(mode: crate::permissions::Mode) -> Self {
        let root = std::env::temp_dir().join(crate::resource_id::fresh("plan239-live-").unwrap());
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let home = root.join("home");
        for name in [".ssh", ".aws", ".gnupg"] {
            std::fs::create_dir_all(home.join(name)).unwrap();
            std::fs::write(home.join(name).join("sentinel"), "PLAN239-SYNTHETIC-SECRET").unwrap();
        }
        let asked = Arc::new(AtomicUsize::new(0));
        let approver = Arc::new(CountingApprover {
            decision: crate::permissions::Decision::Allow(crate::permissions::ApprovalScope::Once),
            asked: asked.clone(),
        });
        let base = test_ctx(0, "plan239-live");
        let mut cfg = base.cfg.test_clone();
        cfg.cwd = root.clone();
        cfg.permissions = Arc::new(
            crate::permissions::Permissions::new(
                mode,
                &Default::default(),
                root.clone(),
                Some(approver),
            )
            .unwrap(),
        );
        cfg.sandbox = Some(Arc::new(
            SandboxPolicy::workspace(&root, &[], false).with_credential_roots(&home),
        ));
        Self {
            root,
            ctx: crate::tools::ToolCtx {
                cfg: Arc::new(cfg),
                ..base
            },
            asked,
        }
    }

    fn command(&self, action: &str) -> String {
        let root = self.root.display();
        // 脚本是合成的本地程序；正文里的路径只参与字符串比较，读取由参数单独触发。
        format!(
            r#"cat > {root}/manifest <<'MANIFEST'
path sample: {root}/home/.aws/sentinel
literal data: $(touch {root}/substitution-ran) `touch {root}/backtick-ran`; 中文
MANIFEST
cat > {root}/probe.sh <<'SCRIPT'
path='{root}/home/.ssh/sentinel'
case "$1" in
compare) [ "$path" = '{root}/home/.ssh/sentinel' ] && echo literal-ok ;;
read) /bin/cat "$path" ;;
move) /bin/mv "$path" '{root}/moved'; /bin/cat '{root}/moved' ;;
move-parent) parent="${{path%/*/*}}"; moved='{root}/moved-home';
    /bin/mv "$parent" "$moved"; hidden=.s; hidden=${{hidden}}sh;
    /bin/cat "$moved/$hidden/sentinel" ;;
esac
SCRIPT
echo attempt >> {root}/attempts
/bin/sh {root}/probe.sh {action}"#
        )
    }
}

impl Drop for LiteralProbe {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[tokio::test]
async fn plan239_original_rust_probe_builds_and_runs_under_protection() {
    let probe = LiteralProbe::new(crate::permissions::Mode::Manual);
    let root = probe.root.display();
    let command = format!(
        r#"mkdir -p {root}/src
cat > {root}/Cargo.toml <<'TOML'
[package]
name = "plan239_probe"
version = "0.1.0"
edition = "2024"
TOML
cat > {root}/src/main.rs <<'RUST'
fn main() {{
    let path = "{root}/home/.ssh/sentinel";
    assert!(path.ends_with("sentinel"));
    println!("rust-literal-ok");
    if std::env::args().any(|arg| arg == "read") {{
        match std::fs::read_to_string(path) {{
            Ok(secret) => println!("{{secret}}"),
            Err(_) => {{ eprintln!("Permission denied"); std::process::exit(7); }}
        }}
    }}
}}
RUST
CARGO_HOME={root}/cargo-home cargo run --offline --quiet --manifest-path {root}/Cargo.toml"#
    );
    let (out, error) = run_tool(
        "bash",
        json!({"command": command, "timeout_ms": 60000}),
        &probe.ctx,
    )
    .await;
    assert!(!error, "{out}");
    assert!(out.contains("rust-literal-ok"), "{out}");
    let command = format!("{command} -- read");
    let (out, error) = run_tool(
        "bash",
        json!({"command": command, "timeout_ms": 60000}),
        &probe.ctx,
    )
    .await;
    assert!(!error, "{out}");
    assert!(!out.contains("PLAN239-SYNTHETIC-SECRET"), "{out}");
    assert!(
        out.contains("requires credential read/write protection"),
        "{out}"
    );
    assert_eq!(probe.asked.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn plan239_real_foreground_literal_program_keeps_credential_isolation() {
    use crate::permissions::Mode;
    for mode in [Mode::Manual, Mode::Bypass] {
        let probe = LiteralProbe::new(mode);
        let (out, error) =
            run_tool("bash", bash_input(&probe.command("compare")), &probe.ctx).await;
        assert!(!error, "{out}");
        assert!(out.contains("literal-ok"), "{out}");
        assert!(!probe.root.join("substitution-ran").exists());
        assert!(!probe.root.join("backtick-ran").exists());
        assert_eq!(
            std::fs::read_to_string(probe.root.join("attempts")).unwrap(),
            "attempt\n"
        );
        for action in ["read", "move", "move-parent"] {
            let (out, error) =
                run_tool("bash", bash_input(&probe.command(action)), &probe.ctx).await;
            assert!(!error, "{out}");
            assert!(!out.contains("PLAN239-SYNTHETIC-SECRET"), "{out}");
            assert!(
                out.contains("Operation not permitted") || out.contains("Permission denied"),
                "{out}"
            );
            assert!(
                out.contains("requires credential read/write protection"),
                "{out}"
            );
            assert!(!out.contains("disable_sandbox: true"), "{out}");
            assert!(!out.contains("Re-ran without"), "{out}");
            assert!(!probe.root.join("moved").exists());
            assert!(!probe.root.join("moved-home").exists());
        }
        assert_eq!(
            std::fs::read_to_string(probe.root.join("attempts")).unwrap(),
            "attempt\nattempt\nattempt\nattempt\n"
        );
        assert_eq!(probe.asked.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn plan239_real_background_preserves_protection_and_suppresses_escape_hint() {
    let probe = LiteralProbe::new(crate::permissions::Mode::Manual);
    for action in ["compare", "read"] {
        let (out, error) = run_tool(
            "bash",
            json!({"command": probe.command(action), "background": true}),
            &probe.ctx,
        )
        .await;
        assert!(!error, "{out}");
        let id = bg_id(&out);
        let (out, error) = run_tool(
            "bash_output",
            json!({"bash_id": id, "block": true, "timeout_ms": 10000}),
            &probe.ctx,
        )
        .await;
        assert!(!error, "{out}");
        assert!(!out.contains("PLAN239-SYNTHETIC-SECRET"), "{out}");
        if action == "compare" {
            assert!(out.contains("literal-ok"), "{out}");
        } else {
            assert!(
                out.contains("requires credential read/write protection"),
                "{out}"
            );
            assert!(!out.contains("disable_sandbox: true"), "{out}");
        }
    }
    assert_eq!(probe.asked.load(Ordering::SeqCst), 0);
    assert_eq!(
        std::fs::read_to_string(probe.root.join("attempts")).unwrap(),
        "attempt\nattempt\n"
    );
    probe.ctx.cfg.shutdown_background_work(&probe.ctx.ui).await;
}

#[tokio::test]
async fn plan239_direct_executor_and_remembered_escalation_cannot_drop_protection() {
    use crate::permissions::{ApprovalScope, Decision, EscalationOutcome, Mode, Permissions};
    let mut probe = LiteralProbe::new(Mode::Manual);
    for background in [false, true] {
        let (out, error) = run_tool("bash", json!({"command": probe.command("compare"), "disable_sandbox": true, "background": background}), &probe.ctx).await;
        assert!(error, "{out}");
        assert!(out.contains("sensitive path"), "{out}");
    }
    let workspace = probe.ctx.cfg.effective_workspace();
    let error = super::super::run_foreground_bash(
        &probe.command("compare"),
        None,
        None,
        &probe.ctx,
        &workspace,
    )
    .await
    .err()
    .unwrap();
    assert!(
        error
            .to_string()
            .contains("requires credential read/write protection"),
        "{error}"
    );
    assert!(!probe.root.join("attempts").exists());

    let approver = Arc::new(CountingApprover {
        decision: Decision::Allow(ApprovalScope::WorkspaceSession),
        asked: probe.asked.clone(),
    });
    let mut cfg = probe.ctx.cfg.test_clone();
    cfg.permissions = Arc::new(
        Permissions::new(
            Mode::Manual,
            &Default::default(),
            probe.root.clone(),
            Some(approver),
        )
        .unwrap(),
    );
    probe.ctx.cfg = Arc::new(cfg);
    assert!(matches!(
        probe
            .ctx
            .cfg
            .permissions
            .escalate_sandbox("value=$DYNAMIC", None, 0)
            .await,
        EscalationOutcome::Approved(_)
    ));
    assert!(
        probe
            .ctx
            .cfg
            .permissions
            .sandbox_escalation_remembered(&probe.command("compare"))
    );
    let (out, error) = run_tool("bash", bash_input(&probe.command("compare")), &probe.ctx).await;
    assert!(error, "{out}");
    assert!(
        out.contains("requires credential read/write protection"),
        "{out}"
    );
    assert!(!probe.root.join("attempts").exists());
    assert_eq!(
        probe.asked.load(Ordering::SeqCst),
        1,
        "preflight never asks to discard protection"
    );
}

#[tokio::test]
async fn plan239_sandbox_start_failure_does_not_retry_without_protection() {
    let mut probe = LiteralProbe::new(crate::permissions::Mode::Bypass);
    let mut cfg = probe.ctx.cfg.test_clone();
    let mut shells = (*cfg.shell_programs).clone();
    shells.bash.as_mut().unwrap().executable = probe.root.join("missing-shell");
    cfg.shell_programs = Arc::new(shells);
    probe.ctx.cfg = Arc::new(cfg);
    let (out, error) = run_tool("bash", bash_input(&probe.command("compare")), &probe.ctx).await;
    assert!(!out.contains("Re-ran without"), "{out}");
    assert!(!out.contains("disable_sandbox: true"), "{out}");
    assert!(
        error || out.contains("requires credential read/write protection"),
        "{out}"
    );
    assert!(!probe.root.join("attempts").exists());
}

#[tokio::test]
async fn plan239_symlinked_credential_root_is_denied_under_both_names() {
    let mut probe = LiteralProbe::new(crate::permissions::Mode::Bypass);
    let home = probe.root.join("home");
    let keys = probe.root.join("keys");
    std::fs::rename(home.join(".ssh"), &keys).unwrap();
    std::os::unix::fs::symlink(&keys, home.join(".ssh")).unwrap();
    let mut cfg = probe.ctx.cfg.test_clone();
    cfg.sandbox = Some(Arc::new(
        SandboxPolicy::workspace(&probe.root, &[], false).with_credential_roots(&home),
    ));
    probe.ctx.cfg = Arc::new(cfg);
    let command = probe.command("read");
    for path in [
        keys.join("sentinel"),
        home.join(".aws/sentinel"),
        home.join(".gnupg/sentinel"),
    ] {
        // 读取发生在生成后的程序内；三个凭据根及符号链接的规范名字都须由 OS 拒绝。
        let original = home.join(".ssh/sentinel");
        let command = command.replace(original.to_string_lossy().as_ref(), &path.to_string_lossy());
        let (out, error) = run_tool("bash", bash_input(&command), &probe.ctx).await;
        assert!(!error, "{out}");
        assert!(!out.contains("PLAN239-SYNTHETIC-SECRET"), "{out}");
        assert!(
            out.contains("requires credential read/write protection"),
            "{out}"
        );
        assert!(!out.contains("Re-ran without"), "{out}");
    }
    std::fs::create_dir_all(probe.root.join("outside")).unwrap();
    std::os::unix::fs::symlink(probe.root.join("outside"), keys.join("escape")).unwrap();
    let command = probe
        .command("compare")
        .replace(".ssh/sentinel", ".ssh/escape/sentinel");
    let (out, error) = run_tool("bash", bash_input(&command), &probe.ctx).await;
    assert!(error, "{out}");
    assert!(out.contains("sensitive path"), "{out}");
}
