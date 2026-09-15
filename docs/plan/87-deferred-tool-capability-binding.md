# Plan 87 — CodeWhale 借鉴：deferred tool capability binding

> 状态：✅ 已完成（2026-08-14）
>
> 依赖：Plan 16、Plan 54、Plan 63、Plan 86；参考快照 `refs/codewhale` 当前观察 HEAD `5e3ac84c5cb925b4c90c34dfe582b75f04605cb1`，固定历史审计基线 `b494236312ef3ac36489c83706a0b11ab73935a1`

## Context

kloop 已完成 Plan 16/54 的 deferred ToolSearch、ToolSource immutable snapshot/generation、stale call rejection 和 `from_program` 的 discovery gate 分层。CodeWhale 当前可借鉴的下一步不是重做 ToolSearch，而是让 unlock/capability cache 与真实 workspace、worktree、permission/policy revision 绑定：不能把一个上下文中发现的工具能力误用到另一个工作区、隔离 worktree、child authority 或刷新后的 policy。

当前 `unlocked_tools` 主要是 session 级 name→generation 共享状态，ToolSearch/dispatch 主要按 source generation 防 stale；Plan 63 已把 permission/session cache 按 WorkspaceId 分区，但 deferred unlock 尚未形成同样明确的 binding。Plan 86 的 compaction、resume、fork 和 child 生命周期也需要明确 unlock 是否重置或重新发现。

## 契约与范围

### 本次吸收

1. discovery/unlock receipt 至少绑定 ToolSource generation、EffectiveWorkspace/WorkspaceId、worktree identity、permission/policy revision、authority/depth scope 和 qualified tool identity；裸 name→generation 不能单独作为授权事实。
2. ToolSearch 返回的 definition、qualified name、source generation 和 binding 必须来自同一 immutable snapshot；dispatch 与真正 `call_at_generation` wire call 都验证该 binding。
3. source refresh、workspace/worktree 切换、permission/policy revision 变化、child authority 变化时旧 unlock fail closed，必须重新 discovery；fork/compaction/resume 不隐式跨 workspace 复制 unlock。
4. `from_program` 只豁免 deferred discovery/locked gate，继续受 permission、sandbox、hooks、concurrency、source generation 和 provider/tool call contract 约束。
5. 当前 provider tool array 仍保持稳定；unlock 不在同一 request 动态增删工具定义，不新增 prompt prefix churn。

## 关键文件

- `rust/crates/core/src/config.rs`
- `rust/crates/core/src/tools/mod.rs`
- `rust/crates/core/src/tools/tool_search.rs`
- `rust/crates/core/src/tools/codemode.rs`
- `rust/crates/core/src/tools/subagent.rs`
- `rust/crates/core/src/agent.rs`
- `rust/crates/core/src/permissions.rs`
- `rust/crates/core/src/worktree.rs`
- `rust/crates/protocol/src/lib.rs`（仅必要的内部/非 public receipt 类型）
- `rust/crates/provider/src/anthropic.rs` 与 provider tests（只做 request stability evidence）

## 非目标

- 不重做 ToolSearch ranking/select、MCP catalog refresh、permission policy 本身或 ToolSource ownership。
- 不把 unlock 变成永久权限，不写 durable rollout，不建全局 tool schema cache。
- 不动态改写当前 provider tool array，不绕过 `call_tool`、permission、sandbox、hooks 或 concurrency。
- 不实现 child route provenance、MCP health、provider terminal 或 child billing。

## 为什么不能与其他计划合并

这是 capability 授权与缓存失效边界；Plan 88 解释执行来源/血缘，Plan 89 解释 provider 终态，Plan 90 解释 MCP 控制面 readiness。它们的 identity、owner 和 lifecycle 不同，混合会让“已发现”“有权调用”“来自哪个 child”“provider 是否可用”变成不可审计的一个布尔值。

## 实施与测试方向

补齐 generation threshold 0/at/past、refresh race、same-name collision、select/keyword/max/bad input、direct/call_tool/program bypass、parent→child shared 与 isolated worktree、workspace switch、policy allow/deny/ask ordering，以及 compaction/resume/fork 后 unlock 生命周期测试。验证 provider request bytes/cache 不因 unlock churn 改变，并用 mutation-negative 证明 stale binding fail closed。

## 完成标准

unlock receipt 与 workspace/worktree/policy/authority/source generation 绑定；所有 stale/跨 scope reuse fail closed；现有 ToolSearch、Program、permission、sandbox、provider tool array 和 child authority 语义不回归；无新增依赖、public protocol 字段、second registry 或 durable unlock truth。

## 实施结果（2026-08-14）

- `DeferredToolUnlocks` 以 session-memory receipt 替代裸 `name → generation` map。receipt 绑定 source owner slot、qualified tool name、definition generation、`WorkspaceId`/effective workspace cwd、session worktree transition epoch、project/mode/workspace-session permission epochs、agent identity/depth 与 tool allowlist。
- `ToolSource::definition_snapshot` 与 `call_at_generation` 形成 discovery-to-wire 两道 binding seam；dispatch 在 pre-hook 前冻结 workspace/receipt，hook 后复核 discovery capability。`run_program` 的 callable manifest 与同一次 provider request 一起冻结 source owner/generation/readonly verdict，并一路传入 foreground/background `CoreBridge`；`from_program` 只越过 deferred discovery gate，sampling 后的同名 owner hop、generation refresh 或新工具出现都会 fail closed，其他 source generation、hooks、permission、sandbox、concurrency 与 provider/tool contract 仍完整执行。
- policy refresh/invalidation、workspace-session approval、mode transition 和 session worktree transition 都在各自状态锁内与单调 epoch 原子发布；普通 Config clone 与 same-session compaction 保留同 authority receipt，sub-agent 和 resume/fork 新 Config 使用 fresh state，TUI 原地 rewind 与 `/clear` 显式清空，isolated worktree 不继承；receipt 不进入 history、rollout、protocol 或 durable store。
- ToolSearch 只记录 receipt，不改 provider tool array 或 deferred notice；它作为 receipt publication ordering barrier，同一 assistant response 内 search→call 按请求顺序确定执行。dispatch 在 source binding 两侧复核动态 defer 分类，refresh、same-name owner collision、workspace/worktree、permission/policy、authority/allowlist 与 mutation-negative 回归覆盖 stale fail-closed 边界。

## 验证记录（2026-08-14）

- `cargo test -p kloop-core`：690 passed；
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`：通过；
- `cargo test --workspace`：通过（2 项真实 provider credential tests ignored）；
- `cargo run --locked -p kloop -- --mock`、`--mock --headless`、`--mock --headless --json`：通过，JSON smoke 解析 46 行 NDJSON；
- `cargo fmt --all -- --check`：通过；
- `git diff --check`：通过；
- 真实 Anthropic rail（`claude-sonnet-4-6`）：`real_agent_program_workflow_contract` 57.78s、`real_local_agent_mailbox_contract` 58.12s，均通过；
- 真实 OpenAI Chat rail（`gpt-5.5`）：`real_agent_program_workflow_contract` 66.04s、`real_local_agent_mailbox_contract` 53.53s，均通过。首次 primitive 运行暴露模型在 fenced background Program source 后保留一个 LF；验收现与 foreground 契约一致，允许尾部 CR/LF 做 prompt 等价比较，同时继续逐字节验证 artifact 等于实际 tool input；
- 未执行 Linux sandbox/CI、Windows、物理终端与 Desktop E2E。

提交 SHA 以本条所在提交为准。
