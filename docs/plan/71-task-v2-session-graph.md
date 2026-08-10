# Plan 71 — session-scoped Task V2 基础任务图

> 状态：✅ 已完成（2026-08-10；提交 SHA 以本条所在提交为准）
>
> 后续字段修订：Plan 73 已从当前 kloop native Task V2 完整删除无 assignment
> 语义的 `owner`；以下 owner 契约与真实 dogfood 值保留为 Plan 71 历史事实。
>
> 后续修订：Plan 72 将 Task 图收紧为 depth-0 root-owned；普通前台/后台 child
> Agent 不再看到或执行 `task_*`，只返回执行结果，由 root 显式更新。以下共享 child
> 契约与真实验收保留为 Plan 71 当时的历史记录，不代表当前产品面。
>
> 完成结论：已落地共享 `Arc<TaskRegistry>`、四个 strict snake_case 工具、稳定 ID、
> 强依赖/无环/单向状态约束和 clear 高水位，并删除 `todo_write` 及其 Event、TUI、plain、
> native wire、历史 replay 和兼容执行链。Task V2 只走普通 ToolCall，不持久化，也不接 mailbox。
>
> 真实验收：使用 gitignored `.kloop/env.local` 的现有 OpenAI provider 配置，main 创建
> blocker 与 dependent 后依次启动两个后台 child Agent；dependent 在 blocker 未完成时得到
> `task_update` failed，随后 blocker 完成，dependent 再进入 in_progress/completed。最终
> `task_list` 显示任务 `1`/`2` 均 completed、owner 分别为 `blocker-agent` 与
> `dependent-agent-gate-confirmed`，且任务 `2` 保留 `blocked_by:["1"]`。验收记录只保留
> tool/event 判据，不保留 key、endpoint 或 raw provider response。
>
> 依赖：Plan 20、Plan 52、Plan 66、Plan 70

## 背景

kloop 已有前台/后台 Agent 和 Local Agent Mailbox，但多个 Agent 没有共享、结构化、可表达依赖的任务状态。旧 `todo_write` 是每个 Agent 各自持有的无 ID 整表 checklist，不能承担稳定 ID、owner 或依赖图。

本计划新增 kloop 原生 Task V2，并按用户决定直接删除 `todo_write`；不做兼容 alias、同步或迁移。

## 已确认契约

### 工具面

- `task_create {subject, description, owner?, blocked_by?}`
- `task_get {task_id}`
- `task_update {task_id, subject?, description?, status?, owner?, blocked_by?}`
- `task_list {}`

全部使用 snake_case 和 strict parser。`owner:null` 只在 update 中表示清空；`blocked_by` 是完整替换。结果为稳定 JSON，ID 对模型是字符串 `"1"`、`"2"`。

Task 字段为 `id/subject/description/status/owner/blocked_by`；读取时计算只读反向投影 `blocks`。List 返回不含长 description 的紧凑记录并按数值 ID 排序。不做 metadata、active form、filter、pagination、TaskOutput、TaskStop 或 TaskDelete。

### 状态与依赖

- 状态仅有 `pending | in_progress | completed`。
- 允许 `pending → in_progress`、`pending → completed`、`in_progress → completed` 及同状态幂等更新；禁止回退，completed 不可重开。
- 非 pending 状态要求全部直接 blocker 已 completed。
- 缺失依赖、自依赖、重复边和环都在同一 registry 写锁内拒绝；失败不得留下部分 mutation 或消耗 create ID。
- completed 任务保留，首版不删除，因此没有 dangling dependency。

### 所有权与边界

- `Config.tasks: Arc<TaskRegistry>` 是 live runtime/session 过程态；主 Agent、前台/后台子 Agent以及 Program/Workflow 启动的真实 child Agent共享同一 Arc。
- 独立 Config/server thread、新进程和 resume 后的新 runtime 各自从空图和 ID 1 开始；不从 rollout 重建、不写 durable store。
- `/clear` 清图但保留当前 registry 的 ID 高水位，避免仍在运行的 peer 观察到 ID 复用。
- owner 只是标签；不查询 LiveAgentDirectory，不 claim/授权/路由，不接 Local/Team mailbox。
- 四工具走普通 ToolCall event/wire；不新增 Task board、Event 或 native item type。Program JavaScript 不能直接调用，真实 child Agent仍可用。
- task create/update 在 dispatcher 中串行；get/list 可并发读取。权限层将四者视为纯 session-memory 操作，manual/plan mode 均无需用户批准。
- registry 最多 256 个任务；subject/owner 最多 200 字符且单行，description 最多 8 KiB，单任务最多 256 个 blocker。

## 实施

1. 新增 `kloop/crates/core/src/tools/task.rs`：`RwLock<BTreeMap>` registry、单调 allocator、四工具、strict parser、候选图防环、状态门和预算。
2. `Config` 用共享 `Arc<TaskRegistry>` 替换 per-Agent todos；所有 Config 构造点同步，`subagent_from`/`test_clone` clone Arc。
3. 在 `tools/mod.rs`、`permissions.rs`、`codemode.rs`、`commands/clear.rs` 接入注册、dispatch、并发、权限、Program 排除和 clear 语义。
4. 删除 `tools/todo.rs`、`Item::Todo`、TUI `Cell::Todo`、plain checklist、native `type:"todo"`、历史 checklist replay、Skill TodoWrite 映射和旧测试；`todo_write` 只保留 reserved name，防 external ToolSource 冒充，不提供执行兼容。
5. 更新 mock、Plan 52 native report/verifier、README、HANDOFF、capability report 和 refs 导读；固定 Claude Code 2.1.220 raw/normalized corpus与历史 Plan 不改。

## 验证矩阵

- create/get/update/list、owner 设置/清空、ID 顺序/失败不耗号/clear 不复用。
- 缺失、自依赖、重复边、二节点与长环，错误后完整 snapshot 不变。
- blocker 状态门、允许 pending 直达 completed、禁止回退/重开、同一 patch 的候选状态原子校验。
- 多线程 create ID 唯一；独立 registry 不串；parent、foreground child、background child 共享。
- depth 0/1 catalog、custom allowlist、Program 排除、permission/plan mode、dispatcher 并发分类。
- TUI/plain/native/headless 仅普通 ToolCall；无 Todo 类型和专属历史重放。
- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- full/corpus-only exact verifier
- plain/headless mock
- 真实 provider 多 Agent dogfood

实际结果（2026-08-10）：

- `cargo test -p kloop-core tools::task::tests`：9 passed。
- `cargo test -p kloop-core tools::plan52_parity_tests::emit_plan52_parity_report -- --exact`：passed。
- `cargo test -p kloop-server --test server task_graph_is_isolated_per_server_thread`：passed。
- `cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace`：passed。
- `python3 -B refs/claude-code-2.1.220/verify.py` 与 `--corpus-only`：passed。
- `cargo run -p kloop -- --mock` 与 `--mock --headless --json`：passed；Task V2 均投影为普通 ToolCall。
- 真实 provider 后台双 Agent dogfood：依赖门拒绝、完成后推进及最终共享图判据全部 passed。
- `git diff --check`：passed。

## 非目标

- Team task assignment、mailbox notification、owner 自动 claim。
- Task 与 `agent-N`/`program-N`/`workflow-N`/`bg-N` 执行资源绑定。
- 跨进程/跨 thread 持久化、rollout hydration、远程 A2A Task。
- 专属 Task UI、native Task event、删除、筛选或分页。
