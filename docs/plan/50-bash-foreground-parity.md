# Plan 50 — Bash 前台工具对齐

> 状态：未开工
>
> 母计划：Plan 48
>
> 依赖：Plan 48
>
> 固定目标：Claude Code 2.1.220；版本身份以 Plan 48 manifest 为准。

## 背景

Plan 48 已采集 Bash 的成功、失败、坏输入、timeout、显式后台和默认权限拒绝。当前 matrix 的 registration、schema、parser、executor、output、lifecycle 为 `compatible`，permission 为 `intentional-diff`，concurrency 为 `unknown`。

本计划只收敛前台 Bash。自动后台化、完成通知、stall 和 Monitor 留给 Plan 51。

## 当前证据与差距

对应 matrix 行：`bash@allow-cli`。

已有 fixture：

- `bash-success`
- `bash-failure`
- `bash-missing-command`
- `bash-wrong-command-type`
- `bash-timeout`
- `bash-default-permission`
- `bash-background`，仅作为前后台边界线索

kloop 侧优先复用：

- `kloop/crates/core/src/tools/bash.rs`
- `kloop/crates/core/src/shell.rs`
- `kloop/crates/core/src/permissions.rs`
- `kloop/crates/core/src/tools/mod.rs`
- 对应 parser、进程组、取消、权限和工具并发测试

## 目标

1. 固定前台 Bash 的 schema、参数默认、错类型和空值处理。
2. 固定 spawn、stdout/stderr、exit code、timeout、取消和输出截断。
3. 固定 permission、shell safety、sandbox 与实际执行的顺序。
4. 固定 timeout/取消后的进程组清理，不能遗留子进程。
5. 取得按调用输入判断并发安全的精确证据，再决定 kloop 分类。
6. 安全策略不等价时保留 `intentional-diff`，不为名称 parity 降低防线。

## 开工证据闸门

- 从 `cc-bash-entry` 追到 schema、parser、permission、spawn、timeout/cancel、result mapping 和 concurrency 判定。
- 增补 foreground-only raw/normalized fixtures；全部命令在临时 cwd 和独立 process group 内运行。
- 覆盖 cancellation 的不同阶段：审批前、spawn 前、运行中、进程已退出后。
- 新增并发 case，证明两个调用是否重叠、输出是否隔离、取消是否串扰。
- 为每个拟标 `same` 的行为补 kloop 同输入 golden。
- 未取得 sandbox 或平台分支证据时继续 `unknown`，不能按公开说明补齐。

## 实施切片

### 0. schema 与 parser

固定 command、timeout、background 相关字段在前台路径的接受范围、默认和验证顺序。

### 1. permission 与 sandbox

- 对照 deny → 工具自查 → safety/content ask → bypass → allow → fallback ask 的真实调用顺序。
- 核验 shell AST 分类、只读/危险判定、敏感路径和 opaque command。
- 不将 CC 较宽松策略自动移植进 kloop。

### 2. 前台执行生命周期

- 固定环境、cwd、stdout/stderr 合并或分离、非零退出、signal、timeout 和取消结果。
- 验证进程组终止、pipe 关闭和 wait/reap。
- 固定 head/tail 或字符上限、截断提示和错误 envelope。

### 3. concurrency

- 用可控 barrier 运行多个只读与写入命令。
- 记录 CC 实际并发分类是静态、按输入还是 profile-dependent。
- 仅在证据后调整 `is_concurrency_safe` 或 dispatch 分批。

### 4. 产品与回归

只实现被 fixture 裁决的差距；同步 matrix、manifest、static evidence、verifier 和 focused tests。

## 非目标与有意保留

- 不实现自动后台化、Monitor、stall 或后台完成回灌。
- 不改变后台 agent 生命周期。
- 不移除 kloop shell AST、deny、敏感路径和 sandbox 防御。
- 不改 `run_program`。
- 不使用用户 cwd、真实 shell rc、凭据或外部服务做 golden。

## Fixture 与测试

至少覆盖：

- 成功、非零退出、command not found、空 command、错类型；
- timeout 边界、主动取消、忽略 SIGTERM 的子进程和多级进程树；
- 大 stdout/stderr、无换行、非 UTF-8、截断；
- 默认拒绝、session allow、deny 优先、sandbox 分支；
- 两个前台调用的重叠、输出隔离和独立取消。

验证：

```bash
python3 -B refs/claude-code-2.1.220/verify.py
cd kloop
cargo test -p kloop-core tools::bash::tests
cargo test -p kloop-core permissions::tests
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p kloop -- --mock
cd ..
git diff --check
```

## 文档同步

完成时同步本 plan、HANDOFF、refs/README、kloop README、capability report 与 parity 产物。Plan 51 的后台状态不得在本计划完成记录中冒充已完成。

## 完成标准

- 前台 Bash 八维链有精确 CC fixture 与 kloop 证据。
- permission 差异有安全理由和回归；concurrency 不再靠猜测。
- timeout/取消后无遗留进程。
- 所有门禁全绿，一次提交，提交信息带 `plan50`。

## 开工时定 / 问用户

- permission/sandbox 与 CC 冲突时，哪些 kloop 更严格行为明确保留。
- timeout/cancel/output envelope 采用精确适配还是兼容层。
- 并发 fixture 证实 CC 策略后，kloop 是否需要调整动态分类。
