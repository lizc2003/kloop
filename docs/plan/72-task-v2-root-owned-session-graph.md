# Plan 72 — root-owned session Task graph

> 状态：✅ 已完成（2026-08-10；提交 SHA 以本条所在提交为准）
>
> 完成结论：Task V2 已从 all-Agent shared editing 收紧为 depth-0 root-owned work graph。
> depth>0 catalog 不含四工具，forged/custom-allowlist/`call_tool` 调用在 hooks、permissions
> 和 registry handler 前拒绝；前台/后台 child 只返回结果，由 root 显式推进任务。
>
> 真实验收：使用 gitignored `.kloop/env.local` 的现有 OpenAI provider 配置，root 创建
> blocker/dependent，前台 child 返回后 root 完成 blocker，后台 child 回灌后 root 再推进 dependent。
> 事件流中 child Task tool event 为 0；最终任务 `1`/`2` 均 completed，owner 分别为
> `root-after-foreground`/`root-after-background`，任务 `2` 保留 `blocked_by:["1"]`。
> 只保留 tool/event 判据，不保留 key、endpoint 或 raw provider response。
>
> 依赖：Plan 52、Plan 66、Plan 70、Plan 71

## 背景

Plan 71 将 Task V2 做成 main 与所有真实 child Agent 共同读写的 session graph。kloop 当前没有 Team、teammate、claim 或 assignment；普通 child 是 main 派出的独立执行资源，经前台 tool result 或后台 `SubAgentResult` 回报。让 child 直接改全局任务、依赖和 owner 会把 root 编排状态与 child 内部计划混在一起。

本计划保留 TaskRegistry 和四个原生工具，但把能力收紧为 **depth-0 root-owned session work graph**。child 只执行 prompt 并返回结果，root 决定何时推进任务。

## 当前契约

- 只有 `ToolCtx.depth == 0` 能看到并执行 `task_create/task_get/task_update/task_list`。
- 所有 depth>0 前台/后台 child、fork skill child、Program/Workflow 启动的真实 child catalog 均无 Task V2。
- runtime gate 在 custom allowlist、deferred discovery、pre-tool hook、permission 和 registry handler 之前拒绝 stale/forged/`call_tool` 包装调用；显式 allowlist 不能授予 root capability。
- `Config.tasks: Arc<TaskRegistry>` 继续随 child Config clone，作为内部 session service 实现细节；child 没有模型 capability，也没有私有 task list。
- 前台 child 结果经 `run_agent` tool result；后台 child 结果经 parent Inbox 的 `SubAgentResult`。两者都不自动改 Task，root 根据结果显式 `task_update`。
- owner 仍只是标签；不做 Team、claim、权限、mailbox 路由或 Task↔`agent-N`/Program/Workflow/Shell 绑定。
- Task DAG、状态单向推进、候选图原子校验、预算、稳定 ID、`/clear` 高水位、resume fresh graph、server-thread 隔离和普通 ToolCall 投影不变。
- Program/Workflow JavaScript 继续不能直接调用 Task V2；不新增 Task UI、wire item、持久化或删除。

## 实施

1. `core/src/tools/mod.rs`：Task defs 只进 depth 0；`run_one` 加 root-only gate；保留 reserved name、handler 和并发/权限分类。
2. `core/src/tools/task.rs`：catalog 测试改为 root-only；`tools/mod.rs` 锁四工具 forged/custom-allowlist/`call_tool` rejection、registry 不变和 gate-before-hook。
3. `core/src/tools/subagent.rs`：前台/后台共享写测试改为 child 调用被拒、结果正常返回/回灌、root 后续显式完成。
4. `core/src/tools/plan52_parity_tests.rs` 与 refs verifier/matrix/evidence：depth-one Task 列表为空，报告 root lifecycle 与四个 child runtime rejection；固定 Claude Code raw/normalized corpus 不改。
5. README/HANDOFF/capability/refs/mock 更新为 root-owned/result-only；Plan 71 只追加 superseded 注记，保留历史事实。

## 非目标

- Agent-local 结构化 checklist。
- Team、lead/teammate、任务领取、原子 claim 或自动分派。
- child 只读 Task 视图；第一版是四工具全部 root-only。
- child completion 自动完成任务或失败自动回退。
- durable/hydrated graph、Task UI、筛选、分页或删除。

## 验证

- root depth 有四工具，child depth 无；四个 forged 调用和显式 allowlist/`call_tool` 均 fail closed，hook 不运行。
- foreground/background child 无法改图，仍正常回报；root 可在结果后更新。
- TaskRegistry 全部图/状态/clear/并发测试继续通过。
- Plan 52 exact report、server ordinary ToolCall/thread isolation、matrix check、full/corpus verifier通过。
- `cargo fmt --all --check`、all-target clippy、workspace tests、mock/headless smoke、`git diff --check` 全绿。
- 真实 provider dogfood：root 建依赖任务并派前台/后台 child；child 只回报，root 显式推进，最终 list 一致；不记录凭据、endpoint 或 raw response。

实际结果（2026-08-10）：

- root-only catalog/runtime、allowlist/`call_tool` bypass、gate-before-hook、foreground/background result-only 定向测试：passed。
- `cargo test -p kloop-core`：632 passed；`cargo test --workspace`：passed。
- `cargo test -p kloop-server` 初验暴露并行负载下既有 10 秒 server-line watchdog 过紧；失败 selector 与 serial suite 均 passed，将测试 watchdog 收到 30 秒后默认并行全套 40 passed。
- `cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`：passed。
- Plan 52 exact report、matrix `--check`、full exact-binary verifier、corpus-only verifier：passed。
- interactive/headless JSON mock：passed；Task 仍只投影为普通 toolCall。
- 真实 provider 前台+后台 child dogfood：passed；child Task tool event 为 0，root 显式更新后的最终图一致。
- 独立代码审查：未发现需阻塞提交的问题。
- `git diff --check`：passed。
