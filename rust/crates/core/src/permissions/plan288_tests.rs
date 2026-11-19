// plan 288：macOS 的沙箱让 `/bin/ps` 自己出沙箱运行；打印别的进程环境变量的写法要先问。

#[cfg(target_os = "macos")]
#[test]
fn plan288_ps_environment_flags_are_asked_about() {
    for command in [
        "ps -E",
        "ps -axE",
        "ps -E -p 1",
        "ps -p 1 -o pid -E",
        "ps eww",
        "ps auxe",
        "sudo ps -E",
        "timeout 5 ps -E",
        "ps $FLAGS",
        "ps aux | grep x; ps -E -p 1",
        "for f in -E; do ps $f; done",
    ] {
        let call = CallFacts::gather("bash", &bash(command), &test_cwd(), None);
        assert_eq!(
            call.hazard("bash").map(|hazard| hazard.tag),
            Some("process environment"),
            "{command}"
        );
    }
    for command in [
        "ps aux",
        "ps -ef",
        "ps -ax -o pid,etime,command",
        "ps -o etime -p 1",
        "ps -Oe",
        "ps -p $PID -o pid=",
        "ps -U root",
        "ps -axo pid,command | grep -i kloop | head -5",
    ] {
        let call = CallFacts::gather("bash", &bash(command), &test_cwd(), None);
        assert_eq!(call.hazard("bash").map(|hazard| hazard.tag), None, "{command}");
    }
}

/// manual 下普通的 `ps` 不问；打印环境变量的 `ps` 每次都问。
#[cfg(target_os = "macos")]
#[tokio::test]
async fn plan288_the_gate_asks_only_for_process_environment() {
    let command = "ps -axo pid,command | grep -i kloop | head -5";
    let approver = ScriptedApprover::new(vec![]);
    let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
    p.check_call("bash", &bash(command), 0, true).await.expect(command);
    assert_eq!(approver.ask_count(), 0);

    let command = "ps -E -p 1";
    let approver = ScriptedApprover::new(vec![]);
    let p = gate(Mode::Manual, rules(&[], &[], &[]), approver.clone());
    p.check_call("bash", &bash(command), 0, true).await.expect_err(command);
    assert_eq!(approver.ask_count(), 1);
    assert!(approver.asked()[0].description.contains("[process environment]"));
}
