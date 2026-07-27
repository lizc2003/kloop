# Plan 51 — 后台任务与 Monitor 对齐

> 状态：未开工
>
> 母计划：Plan 48
>
> 依赖：Plan 50
>
> 固定目标：Claude Code 2.1.220；版本身份以 Plan 48 manifest 为准。

## 背景

Plan 48 的 `bash-background` fixture 捕获了 CC 的 task start/update/completion notification，但这不等于 Monitor 已被注册或完整后台状态机已被证明。`monitor@clean-cli` 当前八个维度全部为 `unknown`。

kloop 已有后台 shell 和后台 agent 两套生命周期。现有能力不能直接冒充 CC Monitor，也不能因实现方便先强行合并。

## 当前证据与差距

对应 matrix 行：`monitor@clean-cli`。

已知：

- clean profile 未观察到 Monitor；未知其 feature/profile gate。
- Bash fixture 证明一条显式后台成功路径及完成通知顺序。
- 自动后台化、stall、轮询、失败、取消和会话清理尚未被纵向核验。
- kloop 没有名为 Monitor 的 model-visible 工具。

优先复用：

- `kloop/crates/core/src/tools/bash.rs`
- `kloop/crates/core/src/background_tasks.rs`
- `kloop/crates/core/src/tools/task.rs`
- `kloop/crates/core/src/agent.rs` 的 inbox/step 边界
- 相关事件、server/headless 投影和生命周期测试

## 目标

1. 先裁决 Monitor 的 surface_kind、注册条件和 schema。
2. 固定显式后台、可能的自动后台化和前台 Bash 的边界。
3. 固定 running/completed/failed/cancelled/stalled 状态与可见输出。
4. 固定轮询、等待、通知、重复读取和未知 ID 行为。
5. 固定父 turn 取消、会话退出、CLI 退出和异常时的进程树清理。
6. 评估 shell 与 agent 后台任务是否需要共享基础设施，但不以内部统一替代外部契约证据。

## 开工证据闸门

- 从 exact bundle 找到 Monitor 构造点、gate、schema、call、render/result mapping。
- 建立真实暴露 Monitor 的隔离 profile；clean 未见只保留负观察。
- 扩 fake provider 驱动 background start → status/update → completion/failure/cancel。
- 使用确定性 barrier、短任务和可控 stall 信号，避免依赖不稳定 wall-clock。
- 保存 raw/normalized lifecycle 顺序；任务 ID、PID 和时间只按声明规则归一化。
- 对 kloop shell/agent 两套状态分别建立 golden，再决定适配面。

## 实施切片

### 0. Monitor 注册与 schema

确定它是 model-visible tool、internal adapter 还是条件化控制面；找不到可运行 profile 时保留 `unknown`，不造 `missing` 结论。

### 1. Bash 后台状态机

- 显式后台启动、前台转后台、输出累积、完成、失败、kill。
- task ID 生命周期、重复调用和回收后查询。
- 自动后台阈值只有 fixture 证明后才进入产品决定。

### 2. Monitor 与通知

- 固定轮询/等待输入、返回形状、stall 检测和 completion notification。
- 固定通知进入下一 sampling step、空闲父 agent 或下一 turn 的时机。
- 防止同一终态重复回灌。

### 3. 清理与上限

- 父 turn 取消、会话关闭、进程退出、异常 drop 和进程树清理。
- 任务表上限、淘汰和已完成保留期只在证据或 kloop 明确产品需求后实现。

### 4. 产品与回归

只实现已裁决行为；若不增加 model-visible Monitor，必须用有证据的 `intentional-diff` 或 `missing` 结论说明兼容边界。

## 非目标与有意保留

- 不重新设计 Plan 50 的前台 parser、permission 或 sandbox。
- 不把内部 background registry 自动暴露成工具。
- 不把子 agent completion inbox 直接当 Monitor。
- 不按旧参考仓的轮询或 LRU 语义推断 2.1.220。
- 不留后台进程、真实 cron 或用户会话作 fixture。

## Fixture 与测试

至少覆盖：

- 显式后台成功、失败、取消；
- 若存在，自动后台化的边界前后；
- running → completed/failed/cancelled/stalled 的完整事件序列；
- 输出为空、大输出、部分输出和终态后查询；
- 未知 ID、重复 wait/monitor、重复 kill；
- 父 turn abort、session close、process exit 后无 orphan；
- shell 与 agent 后台路径独立回归。

验证：

```bash
python3 -B refs/claude-code-2.1.220/verify.py
cd kloop
cargo test -p kloop-core background_tasks
cargo test -p kloop-core tools::bash::tests
cargo test -p kloop-core tools::task::tests
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p kloop -- --mock
cd ..
git diff --check
```

## 文档同步

完成时同步本 plan、HANDOFF、refs/README、kloop README、capability report 和 parity 产物。明确 Monitor 是否实现，以及 shell/agent 后台两条链的边界。

## 完成标准

- Monitor 注册条件已被真实 profile 裁决，或保留带不可运行理由的 `unknown`。
- 可执行后台状态和通知链均有 raw/normalized fixture 与 kloop golden。
- 取消、退出和异常后无遗留进程或重复通知。
- 所有门禁全绿，一次提交，提交信息带 `plan51`。

## 开工时定 / 问用户

- 是否新增 model-visible Monitor，取决于注册和执行 fixture。
- 自动后台化的条件、stall 时钟和通知投递位置。
- shell/agent 两套任务表是否共享底层状态；外部语义未一致前不强制合并。
