# Plan 127 — 同一个「是」，问了一遍又一遍

> 来源：2026-09-08 晚，plan 126 落地后的当轮 dogfood。用户截了一张图：
>
> ```
> go test -race ./upstream/aliyunimage ./gateway/handler -run 'Test(...)' -count=1
> ⚠ the OS sandbox blocked this — run it without the sandbox?
> > 1. Yes
>   2. No, and tell kloop what to do differently
> ```
>
> 两句话：「老是询问，能询问一次，然后把 permission 写到项目里，不要再问了吗」、
> 「以及要告诉具体被什么拦了」。

## 一、先纠正一条归因

plan 126 把那 107 分钟的阻塞归给了 `GOCACHE` 不在可写根内。**那条归因没有直接证据**
——`escalate_sandbox` 批准后返回的是 `ESCALATED_PREFIX` 加无沙箱结果，沙箱内的原始
失败输出**在那一刻被丢弃了**，事后无从查证。

这次用四个最小实验把它查清了（近似 kloop 的可写根 + 网络开关，`sandbox-exec`
直接跑用户那条命令）：

| 策略 | 结果 |
|---|---|
| 可写根齐全 + 允许网络 | 通过 |
| 同上 + `deny network*` | `httptest: failed to listen on a port` ← 复现截图 |
| 放开 loopback（bind + inbound + outbound） | 直接通过 |
| 只放 bind + inbound，不放 outbound | `dial tcp 127.0.0.1:55553: connect: operation not permitted` |

**真凶是网络，不是文件写。** `httptest.NewServer()` 既要 bind 本地端口、也要客户端
connect 回去，两样都在 `allow_network = false` 的封锁里。plan 126 的缓存目录改动对这
一类毫无帮助——它修的是另一个真实但不同的洞。

**放开 loopback 这条路被否掉了**（用户拍板，2026-09-08）：这台机器
`HTTP_PROXY=http://127.0.0.1:7897`，实测放开 loopback outbound 之后沙箱内
`curl https://example.com` 返回 200（走的就是那个本地代理），而 `nc 1.1.1.1 53` 仍被
拒。**在任何跑本地代理的开发机上，放开 loopback 等于放开外网**，沙箱的网络隔离就没
了。逐个放开沙箱资源本来也是打地鼠：真正的出口是让「是」只说一次。

## 二、代价有多大：63 分钟里的 20 分钟

plan 126 的 skill 三句已经生效。同一个 agent、同一个仓库：

| | 组 1（旧 skill） | `d9952d70`（新 skill） |
|---|---|---|
| 建临时树 | 12 | **0** |
| 写探针测试 | 11 | **0** |
| 命中率 | 50.6% | **88.8%** |
| 未缓存 in | 2.98M | **741k** |
| 结论 | 无（用户放弃） | **turn completed** |

但同一个提交 `d9952d70`，kloop 63 分钟、codex 39 分钟。把墙钟拆成模型段与工具段：

| | kloop | codex |
|---|---|---|
| 模型段 | 45 轮，中位 19s，合计 **43 min** | 62 轮，中位 19s，合计 **38 min** |
| 生成速率 | 20.0 tok/s | 20.2 tok/s |
| **工具段** | 44 次，合计 **20 min** | 61 次，合计 **1 min** |

**模型侧两边一模一样。** 差距全在工具段，而那 20 分钟里三次调用占了 19.8 分钟：

```
[556s] 一轮并发三个 go test —— 每个都要单独审批
[356s] go test -race ./upstream/aliyunimage ./gateway/handler
[275s] go test 五个包
```

codex 跑的是**同一批包**（`go test ./upstream/aliyunimage ./gateway/handler -count=1`），
它的工具段最慢 **12 秒**。差 20–45 倍，因为 kloop 每个 `go test` 都要走
「沙箱内跑 → 被 httptest 的网络拦住 → 弹审批 → 等人点 → 无沙箱**重跑一遍**」。
codex 快，是因为它把批准记住了：既不问，也不跑两遍。

## 三、为什么原来记不住

`escalate_sandbox` 的审批是硬编码的单档：

```rust
approval_scopes: vec![ApprovalScope::Once],
remember_rules: None,
```

前端因此只画得出 Yes / No——普通 bash 审批那三档（这次 / 本工作区 / 写进项目）一档
都没有。更死的是下面这行：

```rust
Decision::Allow(ApprovalScope::WorkspaceSession | ApprovalScope::Project)
| Decision::Deny => EscalationOutcome::Declined,
```

**就算前端给了更宽的选项，用户选了也当拒绝。**

而且不能直接复用普通 bash 的 `remember_payload`：它记的是 `bash(go test *)` 这类
「不用问就能跑」的规则，沙箱升级要记的是「这条命令可以**在沙箱外**跑」。记了前者也
没用——下次照样先在沙箱里跑、照样失败、照样问。

## 四、做了什么

**1. 独立的规则类别。** 新增 `Rule::SandboxEscalatePrefix`，规则串
`sandbox_escalate(<两词前缀> *)`，走既有的 `ProjectAllowRules::parse` 落进
`~/.kloop/projects/v1/<ProjectId>/permissions.json`。两个方向都不互通：
`bash(...)` 与整工具 `bash` 规则都**不**授予升级权（`matches_escalation_argv`），
`sandbox_escalate(...)` 也**不**让命令通过普通 gate（`matches_argv`）。*可以运行*和
*可以不受约束地运行*是两种权限。

**2. 三档审批 + 先查记忆。** `escalate_sandbox` 先算 payload、先查项目规则与本工作区
会话缓存，命中直接 `Approved` 不问；未命中才问，并按 `remember.is_some()` /
`project.can_persist()` 决定提供哪几档。落盘失败仍然放行这一次（用户已经说了是），只是
记不住——与普通 gate 的 project 档同构。粒度也与普通 bash 审批一致：两词前缀。**但
read-only 段不跳过**——`remember_payload` 会跳过只读命令，这里不能，因为一条只读命令
既然被拒到了这一步，它就是被同意的内容的一部分。opaque 脚本（管道、重定向）产生不出
payload，因此永远只有 `Once`。

**3. 说清被什么拦了。** `is_likely_sandbox_denied` 从 `bool` 变成
`classify_sandbox_denial() -> Option<SandboxDenial>`，带 `DenialKind`
（`Write` / `Network` / `Unclassified`）和**匹配到的那一行**。分类逐行判断——一份
build log 在写拒绝上方十行提到 socket，不能把写判成网络。审批提示因此变成：

```
⚠ the OS sandbox blocked this (network) — run it without the sandbox?
  listen tcp6 [::1]:0: bind: operation not permitted
```

同一行也进了升级后的结果前缀（`escalated_prefix`），因为无沙箱那一跑会顶掉沙箱内的
输出——**那是「为什么解除了约束」的唯一记录**，第一节那条错误归因就是这么来的。
分类不猜：说不出资源的就报 `unclassified`，猜错会把读者送去拧错的旋钮。

## 五、非目标

- **不放开 loopback**（第一节，用户拍板）。
- **不动重试预算**（plan 126 第六节那条：`MAX_ATTEMPTS = 3` + 250ms 退避，18 秒烧完
  一个 turn）。它与 `escalate_sandbox` 超时是同一类——turn 被外部挂住时要能自己走
  出来——但都不在本片。
- **不给 `escalate_sandbox` 加超时。** 探查过：要动 TUI/CLI/server 三个 approver，
  且 CLI 侧撞 plan 80 的「Tokio blocking stdin read 不可取消」。记忆先落地之后，触发
  次数本来就该塌下去。
- **不碰模型段。** 45 轮对 62 轮、中位 19s 对 19s、20.0 对 20.2 tok/s——两边一样，
  这里没有可捡的东西。

## ✅ 已完成（2026-09-08；提交 SHA 以本条所在提交为准）

`crates/core/src/sandbox/mod.rs`、`crates/core/src/permissions.rs`、
`crates/core/src/tools/bash.rs`，README 的沙箱一节同步了两条新行为。

### 测试

- `a_remembered_escalation_is_not_asked_again`：workspace 档答一次后，同前缀不同参数
  的命令不再问；换一条命令仍然问（脚本 approver 已耗尽 → 拒绝，正是它确实问了的证明）。
- `an_opaque_command_offers_no_remember`：带重定向的脚本只拿到 `Once`、
  `remember_rules` 为 `None`，两次都问。
- `escalation_rules_and_bash_rules_do_not_substitute_for_each_other`：四个方向逐条断言。
- `the_escalation_prompt_says_what_was_blocked`：notice 里有 `(network)` 和原文那一行。
- `a_denial_is_classified_and_carries_the_line_that_proves_it` /
  `classification_does_not_read_hints_across_lines` /
  `the_escalated_marker_carries_the_denial`。

按 plan 91 的教训先 `--list` 证明四条新 permissions 测试命中再跑。

### 验证

`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -D warnings`、
`cargo test --workspace`（33 个 target、1443 passed、0 failed）全绿。

### 下一轮复查

同形状的审查任务，看工具段是否从 20 分钟塌回分钟级、`go test` 是否只跑一遍。本片的
63 min / 20 min 工具段就是基线。
