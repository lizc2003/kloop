# Plan 73 — 删除 Task V2 原生 owner

> 状态：✅ 已完成（2026-08-10；提交 SHA 以本条所在提交为准）
>
> 完成结论：kloop native Task V2 的 schema、strict parser、storage、预算和
> create/get/update/list JSON output 已完整删除 owner。owner string/null 均在副作用前拒绝；
> create 不耗 ID，mixed update 不部分提交。root-only/result-only 与 DAG 契约保持不变。
>
> 依赖：Plan 52、Plan 71、Plan 72

## 背景

Plan 72 已把 Task V2 收紧为 depth-0 root-owned session work graph：child Agent
只通过前台 `run_agent` result 或后台 `SubAgentResult` 回报，由 root 显式推进任务。
随后用真实 `gpt-5.4-mini` 跑四组调用测试时，同样的显式 Task 生命周期一次由模型
主动写入 `owner:"assistant"`，另一次保持 `owner:null`。

这不是隐藏默认值，而是公开 schema 暴露了一个无约束 free-form 字段。kloop 当前没有
Team、assignment、claim、权限、路由或 Task↔execution identity binding，因此 owner 没有
可执行语义。只在 description/system prompt 里劝模型不要推断 owner 不能形成运行时不变量，
也会继续让调用与输出产生无意义差异。

本计划从 **kloop native public Task V2 完整删除 owner**。如果未来真正实现 assignment，
应重新设计绑定真实 identity 的 typed assignee/claim 契约，而不是复活当前字符串标签。

## 当前产品契约

- `task_create` 输入为 `subject/description/blocked_by?`，新任务固定为 pending。
- `task_update` 输入为 `task_id` 加至少一项 `subject?/description?/status?/blocked_by?`。
- create/get/update 的完整 Task 输出为
  `id/subject/description/status/blocked_by/blocks`。
- list 的紧凑输出为 `id/subject/status/blocked_by/blocks`。
- 四个 schema、description、strict parser、storage 和 JSON output 均没有 owner。
- create/update 传 `owner` string 或 null 都命中 unknown-field error；不兼容忽略。
- 失败 create 不消耗 ID；失败 update 不改变任何 snapshot，mixed patch 也不部分提交。
- Plan 72 的 depth-0 catalog/runtime gate、child result-only 边界保持不变。
- DAG、单向状态门、原子候选提交、预算、并发分类、`/clear` 高水位、resume fresh
  registry、server thread isolation 和普通 ToolCall 投影保持不变。
- 没有 assignment/claim/Task↔Agent 自动绑定，也没有 child 私有 task list。

## 非目标

- 不新增 Team、assignee、claim、Task↔Agent/Program/Workflow/Shell 绑定或权限语义。
- 不保留仅供内部使用且没有消费者的 owner 死字段。
- 不迁移或重写 pinned Claude Code 2.1.220 raw/normalized fixture；其中 external
  Task owner 是固定版本事实。
- 不修改 scheduler durable owner、worktree/resource/session ownership、mailbox
  sender/recipient 或 Agent identity 等同名但无关的概念。
- 不改变 Task 状态机、DAG、持久化边界、UI/wire 取舍或四工具拆分。

## 实施

### 1. Registry、parser 与 schema

`kloop/crates/core/src/tools/task.rs`：

- 删除 `MAX_OWNER_CHARS`。
- 删除 `StoredTask`、`TaskView`、`TaskSummary`、`TaskCreateInput`、`TaskPatch`
  上的 owner 字段。
- 删除 `OwnerPatch`、`optional_owner` 和 owner 文本校验。
- create/update allowed-field list 不再含 owner，由现有 `strict_object` 统一拒绝。
- 四个 ToolDefinition 的 schema/description 与 create/get/update/list JSON output
  不再出现 owner。
- create 的 ID 分配和 update 的 candidate-then-commit 锁顺序不变。

### 2. 回归与 child result-only 边界

- 用整对象断言 create/get/update/list 的字段集合稳定且无 owner。
- create 带 owner string/null 均失败；下一次合法 create 仍取得同一 ID。
- update 带 owner string/null 均失败；含合法 status 的 mixed patch 也整体失败，
  get/list snapshot 不变。
- schema properties 均无 owner。
- root-only forged/custom-allowlist/`call_tool` gate 保持原测试覆盖；成功输出改断言
  owner key 缺席。
- foreground/background child result-only 测试不再伪造或由 root 写 owner，只验证
  child Task 调用被拒、结果正常返回以及 root 后续显式更新成功。

### 3. Native parity 与防篡改门

- Plan 52 native report 的 root lifecycle 不再写 owner，所有 native Task record
  必须没有 owner。
- report 增加 owner-field gate：native create/update 传 owner 均 strict reject。
- `verify.py` 增加 mutation-negative：向 native Task JSON 伪造 owner 时 verifier 必须拒绝。
- matrix 的 kloop TaskList/TaskUpdate notes 明确当前没有 assignment/owner 字段；
  Claude Code external owner 事实保持不变。
- 同步 generated `tool-matrix.json` 与 `static-evidence.jsonl` 的 claim/range。

### 4. 当前文档与历史

- README、HANDOFF、capability report 和 refs README 改为 owner-free 当前契约。
- Plan 71/72 顶部仅追加 Plan 73 supersession 注记；当时 owner 契约与真实
  dogfood 值保持原样，避免改写历史。

## 验证计划

1. Task 定向单测：四 schema/output 无 owner，owner string/null strict reject，失败
   create/update 无 ID 或 snapshot mutation。
2. root-only catalog/runtime 与 foreground/background result-only 定向回归。
3. Plan 52 exact native report、matrix check、full/corpus verifier及 owner mutation-negative。
4. `cargo fmt --all --check`、all-target clippy、workspace tests、server tests和
   `git diff --check`。
5. mock/headless smoke 验证 Task ToolCall 输出字段无 owner。
6. 真实 OpenAI Chat `gpt-5.4-mini` 跑显式 lifecycle、依赖错误恢复与 owner strict
   rejection；Anthropic 跑至少显式 lifecycle。仅记录 tool/event/字段判据，不记录
   key、endpoint 或 raw provider response。
7. 独立审查确认只移除 native Task owner，没有伤及外部 fixture 或其他 ownership。

## 完成记录

- `task.rs` 的数据模型、输入、schema、description、parser、输出与字符预算已 owner-free；
  Task 回归覆盖四工具完整字段集合、string/null unknown-field、失败 create ID 不前进及
  mixed update snapshot 不变。
- Plan 52 native report 采集四个 Task schema 并锁 exact properties/required/
  `additionalProperties:false`；owner gate 覆盖 create/update 的 string/null 四条，
  verifier 另有伪 owner schema/output mutation-negative。matrix、generated artifact 与
  static evidence 已同步，固定 Claude Code fixture 未改。
- README、HANDOFF、capability report、refs README 已改为当前 owner-free 契约；
  Plan 71/72 只追加 supersession 注记，保留当时 owner 值与 dogfood 历史。

验证（2026-08-10）：

- `cargo fmt --all --check`：通过。
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`：通过。
- `cargo test --workspace --all-targets --all-features`：通过；包括 core 633、server
  integration 28、TUI 139 以及其余 workspace suites，0 failed。
- Plan 52 exact native report：通过（1 passed）；Task 定向 10 tests、root-only gate、
  foreground/background result-only 回归均通过。
- `build_matrix.py --check`：通过（62 rows、7 pairs、108 profile bridges、218 fixtures）。
- `verify.py --corpus-only` 与 full exact-binary verifier：最终均通过；scripted provider
  suite 7 passed。
- `git diff --check`：通过。
- `--mock --headless --json`：create/create/update/get/list 均走普通 ToolCall，输入与
  输出没有 owner。
- OpenAI Chat `gpt-5.4-mini`：真实 strict owner probe 失败；下一次合法 create 仍为
  ID `1`。随后 blocker/dependent 生命周期含一次预期 dependency failure，最终 get/list
  均 completed；完整 Task keys 为 `blocked_by/blocks/description/id/status/subject`，list
  省略 description，无 owner。
- Anthropic `claude-sonnet-4-6`：真实 create → in_progress → completed → get → list
  五调用全部成功；完整/紧凑 key 集均无 owner。
- 两次真实 provider 仅保留上述过滤后的 tool/event/字段判据；临时 HOME 与原始输出已清理，
  未记录 key、endpoint 或 raw response。初次用 fresh HOME 调 `cargo run` 因 rustup HOME
  隔离失败，改用已编译二进制；非 PTY 重定向两轨无事件 18 分钟后终止并清理，按既有验收
  纪律改 PTY 后通过。
- full verifier 初跑先发现并清理 generated `__pycache__`；并发重跑两次命中既有 Plan 62
  autocrlf restore self-test 的时序失败，isolated probe 5/5 通过，测试负载结束后的最终 full
  run 通过。
- 独立审查先发现 native report 未锁 Task schema、owner null/mixed patch parity 两项 P2；
  补齐后复核确认均解决，无残留 finding。另一路定向审计确认当前 native owner stale claim
  为 0，历史/external/unrelated ownership 均保留。
