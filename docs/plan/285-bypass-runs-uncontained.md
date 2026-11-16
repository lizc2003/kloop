# Plan 285 — bypass 下 bash 不进沙箱 ✅

> 2026-11-16 立并完成。来源：用户转来一份 bypass 会话的自查报告：Jenkins/SLS 凭据只在
> Keychain 里，`security find-generic-password` 失败；git over SSH 读不到 known_hosts、
> 私钥和 agent；内网域名 DNS 解析失败。结论是“bypass 模式下仍被打扰”。

## 一、原因

bypass 只免审批，命令照样先在沙箱里跑，被拒后免审批脱沙箱重跑。三种情况走不到重跑：

1. **SSH 身份**：`~/.ssh` 的读取被拒（`DenialKind::Read`）任何模式都不升级。git over SSH
   先因 DNS 被拒、自动放开网络重跑，再读 known_hosts/私钥被拒，就此停住。
2. **Keychain**：基础 seatbelt 配置不放行 `mach-lookup com.apple.SecurityServer`（只有网络
   配置放行），而拒绝分类只认写、网络、读三类，系统服务连接被拒既不升级也不提示，模型
   只看到 “item could not be found”。
3. **DNS**：真实失败的命令被拒网络后 bypass 会自动放开；报告里的探测脚本捕获异常后以 0
   退出，零退出码只看输出文本里的拒绝痕迹，所以没触发。NO_PROXY 的结论是绕出来的。

bypass 下沙箱剩下的作用基本只有运行时挡凭据读取，代价是上面这些说不清的失败和重跑两遍。

## 二、决定（用户确认）

bypass 下 bash 不进沙箱。保留：deny 规则；明确拼出的凭据读取硬拒；字面正文点名凭据路径的
调用（`Protection::Required`）仍只在沙箱里跑。接受的代价：bypass 下用变量绕开拼写的凭据
读取不再有运行时兜底。manual 不变。

## 三、实现

- 权限门第 4 层（bypass）：执行计划是 `Contained` 且不需要正文保护时改为 `SandboxPlan::None`，
  授权清空。执行层只有 `sandbox_for` 一个入口，前台、后台与 scheduler 都照 plan 执行，
  不用另改。已记住的升级（`Escalated`）与正文保护的调用保持原样。
- `escalate_sandbox`/`escalate_capability` 的 bypass 自动放行保留：会话中途切进 bypass
  时，已在沙箱内跑的调用仍可免审批重跑。
- bash 工具描述写的是 “When OS sandboxing is active …”，bypass 下不算错，不改提示词。

## 四、验证

- 新增 `permissions/plan285_tests.rs`（macOS）：带真实沙箱策略时，bypass 的前台/后台
  调用 `SandboxPlan::None`、`Protection::Clear`、无授权、不问；manual 同一批调用仍
  `Contained`。bypass 下 deny 与 `cat ~/.ssh/id_rsa`、`cat .env` 照旧硬拒，heredoc 凭据
  探针仍 `Contained` + `Protection::Required`。
- 真实 seatbelt：`plan285_bypass_lets_programs_use_credentials_manual_keeps_them_contained`
  用脚本读合成的 `home/.ssh/sentinel`，bypass 读到、manual 被挡；原
  `escalation_in_bypass_attributes_the_retry_without_an_approver` 改为
  `bypass_runs_outside_the_sandbox_from_the_first_attempt`：越界写入第一次就成功，结果里
  没有升级说明与内核拒绝记录。
- 旧测试里 bypass 断言 `Contained` 的改用 `allowed_in(mode, …)`。
- `cargo test -p kloop-core --lib` 全部 1436 项通过（1 ignored，真实 seatbelt 用例实际运行）；
  fmt、`clippy -p kloop-core --all-targets -D warnings`。未跑全量 `make check`、parity，
  Linux/Windows 未实测（非 macOS 本来没有 shell 沙箱）。
