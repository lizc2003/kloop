# Plan 129 — 把「记住」放在了它永远用不上的地方

> 来源：2026-09-09 晚，用户截图：
>
> ```
> set -eu
> tmp=$(mktemp -d /tmp/review-44e9be49.XXXXXX)
> git archive 44e9be49… | tar -x -C "$tmp"
> cd "$tmp"
> ⚠ the OS sandbox blocked this (network) — run it without the sandbox?
>   panic: httptest: failed to listen on a port: listen tcp6 [::1]:0: bind: …
> > 1. Yes
>   2. No, and tell kloop what to do differently
> ```
>
> 「为什么没有存 permissions 的选项」。

## 一、plan 127 的记忆，在最需要它的场景下从不出现

截图里那条命令带 `tmp=$(mktemp -d …)`——变量赋值加命令替换。`analyze_bash`
（`shell.rs:89`）是白名单解析：只认 `cmd arg arg` 加 `&& || ; |`，遇到赋值、命令替换、
重定向就整条判为 `Opaque`。plan 127 的 `escalation_remember_payload` 因此返回 `None`，
`approval_scopes` 只剩 `Once`，前端只画得出 Yes / No。

**这是我在 plan 127 里写死的行为**，理由写的是「跟普通 bash 审批一个规矩」，还配了测试
`an_opaque_command_offers_no_remember` 锁住它。

漏掉的是：**审查里的实证脚本几乎全是 opaque**。`mktemp` + `git archive` + `cd` +
`go test` 这套组合必然带赋值和命令替换——正是 plan 126 教出来的隔离到目标 revision 的
做法。于是记忆功能被精确地放在了它永远用不上的地方：用户 `permissions.json` 里那条
`sandbox_escalate(go test *)` 之所以存得进去，只因为那一次是裸的 `go test …`。

顺带一条正面读数：截图里 `blocked this (network)` 加原始 panic 行，是 plan 127 第 3 条
（拒绝分类 + 证据）在真实场景里生效的样子。

## 二、做了什么

粒度按能不能解析分两档：

| 脚本形态 | 记什么 | 落盘 |
|---|---|---|
| 可解析（`go test …`） | 两词前缀 `sandbox_escalate(go test *)` | 可写进项目 |
| **opaque**（带赋值/替换） | **本工作区会话的全部沙箱升级** | **只在内存，退出即失效** |

- `BLANKET_ESCALATION_SIGNATURE = "sandbox_escalate:*"` 进会话缓存，**永不落盘**：这么粗
  的同意无法写成一条规则，也不该活过这次 sitting。
- `approval_scopes` 现在**两档都给**（`Once` + `WorkspaceSession`），只有 `Project` 仍然
  要求一条写得下来的规则。
- 提示里的 `remember_rules` 在 opaque 时显示 `every sandbox escalation this session`。
  **留空会被读成「只记这一条」**，而它的真实范围是全部——说清楚是知情同意和误读之间的
  全部差别。
- 通配同意同样被**执行前**的 `sandbox_escalation_remembered` 读到（plan 127 补记那处），
  所以命中时连那一遍注定失败的沙箱内执行也跳过，不是只跳过提问。

## 三、非目标

- **不放开 loopback**（plan 127 第一节，用户拍板：本机 `HTTP_PROXY` 在 127.0.0.1:7897，
  放开等于放开外网）。
- **不做任务边界压缩**（用户，2026-09-09：「任务边界这个，我觉得没有意义」）。本轮
  `b7fbdaf2^..f0e5edc7` 的 141.4 min 拆开是 118 轮 × 约 83 秒，工具段只有 1.3 分钟——
  慢的全部是生成 16.4 万 output token 的时间，而轮数被 3 次压缩推高，起点 181.8k 又是
  那 3 次的来源。方案（turn 入口按 60% 阈值先压）与数据都记在这里备查，但按用户判断
  不做。
- **不给 opaque 提供 `Project` 档**。durable 规则必须能被写下来并在重启后由同一个解析器
  复核（plan 127 第一条的约束），通配做不到。

## ✅ 已完成（2026-09-09；提交 SHA 以本条所在提交为准）

`crates/core/src/permissions.rs`：两个常量、`escalate_sandbox` 的 scope 选择与落地分支、
`sandbox_escalation_remembered` 的通配短路。纯权限层改动，无 wire/schema 变化。

### 测试

`an_opaque_command_can_still_be_remembered_for_the_session`（替换原来锁住旧行为的
`an_opaque_command_offers_no_remember`）：opaque 脚本拿到 `Once + WorkspaceSession` 而**没有**
`Project`；提示里的 rule 文本是 `every sandbox escalation this session`；批准之后另一条
opaque 脚本和一条可解析命令都直接放行、全程只问一次；并断言 `sandbox_escalation_remembered`
对它为真——即沙箱内那一遍也被跳过。按 plan 91 的教训先 `--list` 证明命中再 `--exact` 跑到
`1 passed`。

### 验证

`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -D warnings`、
`cargo test --workspace`，各自单独跑并当场取退出码（HANDOFF 111(b2)）。
