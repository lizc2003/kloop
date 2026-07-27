# Plan 52 — Agent、Task 与 Team 对齐

> 状态：未开工
>
> 母计划：Plan 48
>
> 依赖：Plan 48
>
> 固定目标：Claude Code 2.1.220；版本身份以 Plan 48 manifest 为准。

## 背景

Plan 48 只证明了一个本地 Agent 子请求及结果生命周期。Task registry、team mailbox、ListAgents、cloud/remote 和并发上限没有纵向证据。

kloop 的 `task`、`todo`、`wait`、`stop_agent` 与 CC 的 Agent、TaskCreate/Get/List/Update/Output/Stop 不是自动别名；内部 inbox/steering 也不是 model-visible SendMessage。

## 当前证据与差距

对应 matrix 行：

- `agent@allow-cli`
- `task-create@clean-cli`
- `task-get@clean-cli`
- `task-list@clean-cli`
- `task-update@clean-cli`
- `task-output@clean-cli`
- `task-stop@clean-cli`
- `send-message@clean-cli`
- `list-agents@clean-cli`

当前结论：

- Agent registration 为 `intentional-diff`；schema/parser/executor/output/lifecycle 只有本地最小 case 的 `compatible`；permission/concurrency 未知。
- 六个 Task* 与 kloop task/todo 表面在 registration/schema 为 `intentional-diff`，运行时维度未知。
- SendMessage 在 CC clean fixture 可见而 kloop 无对应工具，因此 registration/schema 为 `missing`。
- ListAgents 未在 clean profile 观察到，不能推断其不存在。
- 所有 Plan 48 profile 都是 `team=false`、`remote=false`、`depth=0`。

优先复用：

- `kloop/crates/core/src/tools/task.rs`
- `kloop/crates/core/src/agent_type.rs`
- `kloop/crates/core/src/tools/todo.rs`
- `kloop/crates/core/src/agent.rs` 的 inbox/steering seam
- `kloop/crates/core/src/tools/mod.rs` 注册和并发分批
- 对应 task、todo、取消、事件与回灌测试

## 目标

1. 区分 Agent spawn、Task registry、Team roster/mailbox 和 remote/cloud 分支。
2. 固定 Agent 的输入、权限、同步/后台、停止、结果、错误、父子生命周期和并发。
3. 固定 Task* 的状态模型、依赖、owner、输出、删除/停止和通知。
4. 固定 SendMessage/ListAgents 的可见条件、邮箱顺序和终态回灌。
5. 只在真实 profile 下裁决 team/remote/depth；当前平台无法运行的分支保留 `unknown`。
6. 评估兼容层时保留 kloop 现有 task/todo 能力，不用改名制造假 parity。

## 开工证据闸门

- 从 `cc-agent-entry`、alias normalization 和统一工具适配器追完整 Agent/Task/Team 静态链。
- 新增 `team=true`、`remote=true`、depth>0 等 profile 前，必须确认本机入口和隔离条件真实成立。
- fake provider 驱动父子多轮、并行、失败、取消、后台和 mailbox；不得调用真实云 agent 或用户团队。
- 每个 Task* 单独采 schema/parser/executor case，不能用一条注册 fixture推断共享状态机。
- 保存消息/通知严格顺序，随机 agent/task ID 只按结构化规则归一化。
- 对 kloop 现有 task/todo/wait/stop 分别建立 golden，再决定 adapter 或有意差异。

## 实施切片

### 0. surface 与注册条件

固定 Agent、Task*、SendMessage、ListAgents 的名称/alias、surface_kind、gate 和 profile。

### 1. 本地 Agent

- 最小输入、错类型、agent type、model/isolation 参数。
- 权限、同步与后台 spawn、并发、停止、成功/失败/取消。
- 子结果在父 step 边界、父终答后和父空闲时的投递。

### 2. Task registry

- Create/Get/List/Update/Output/Stop 各自 schema 和状态转换。
- blockedBy/blocks、owner、metadata、删除与终态。
- 与执行型 Agent 的关联不能凭名称推断。

### 3. Team 与 mailbox

- SendMessage 目标、恢复已完成 agent、消息顺序、未知 recipient。
- ListAgents 的 roster/status/owner 与 profile gate。
- team/remote 无法 hermetic 运行时保持 unknown，并写明证据缺口。

### 4. 并发与回灌

- 取得并行上限、排队、取消隔离和父子 completion 的真实证据。
- 复用现有 inbox step-boundary seam；Interrupted 子结果是否回灌由 fixture 裁决。

### 5. 产品与回归

选择独立工具、兼容 adapter 或有意保留。每项选择都更新 matrix 和测试，不移除 kloop-only 编排能力。

## 非目标与有意保留

- 不把 `run_program` 当 Agent 或 Workflow。
- 不假定 CC 的 10 路上限或 cloud/team 行为。
- 不在本计划实现 Worktree 创建/清理；留 Plan 56。
- 不把内部 inbox 暴露为 SendMessage，除非完整契约证据和产品决定都支持。
- 不接真实 team、remote service、凭据或用户 mailbox。

## Fixture 与测试

至少覆盖：

- Agent 最小成功、坏输入、失败、取消、后台、并行和停止；
- 父 turn 继续、终答、空闲和退出时的子结果投递；
- 六个 Task* 的合法/非法输入、状态转换和依赖；
- SendMessage 成功、未知对象、已结束对象和恢复投递；
- ListAgents 可见条件、空 roster 和多状态；
- 并发上限、排队和取消不串扰。

验证：

```bash
python3 -B refs/claude-code-2.1.220/verify.py
cd kloop
cargo test -p kloop-core tools::task::tests
cargo test -p kloop-core tools::todo::tests
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p kloop -- --mock
cd ..
git diff --check
```

## 文档同步

完成时同步本 plan、HANDOFF、refs/README、kloop README、capability report 与 parity 产物。必须区分已跑本地分支和仍 unknown 的 team/remote 分支。

## 完成标准

- 当前可执行 Agent/Task/Team 分支八维链有证据。
- 现有 intentional-diff/missing/unknown 均被裁决或保留明确不可运行理由。
- 并发、取消、回灌和父子清理有确定性测试。
- 所有门禁全绿，一次提交，提交信息带 `plan52`。

## 开工时定 / 问用户

- Agent 与 kloop `task` 的公开命名和兼容 adapter。
- 是否实现独立 Task registry，还是保留 todo/wait/stop 状态模型。
- team、remote、mailbox 与并发上限对 kloop 的产品适用性。
