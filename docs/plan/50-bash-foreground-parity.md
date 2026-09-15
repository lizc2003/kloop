# Plan 50 — Bash 前台工具对齐

> 状态：✅ 已完成（2026-07-30）
>
> 母计划：Plan 48
>
> 依赖：Plan 48
>
> 固定目标：Claude Code 2.1.220；版本身份以 Plan 48 manifest 为准。

## 背景

Plan 48 已采集 Bash 的成功、失败、坏输入、timeout、显式后台和默认权限拒绝。当前 matrix 的 registration、schema、parser、executor、output、lifecycle 为 `compatible`，permission 为 `intentional-diff`，concurrency 为 `unknown`。

本计划只收敛前台 Bash。自动后台化、完成通知、stall 和 Monitor 不在本计划实现；后续 Plan 51 已独立闭环显式后台终态通知/回灌与清理，并把自动后台化、stall、逐事件 Monitor 留作明确边界。

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

- `rust/crates/core/src/tools/bash.rs`
- `rust/crates/core/src/shell.rs`
- `rust/crates/core/src/permissions.rs`
- `rust/crates/core/src/tools/mod.rs`
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

- 本计划不实现自动后台化、Monitor、stall 或后台完成回灌；显式后台通知/回灌已由 Plan 51 单独完成。
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

完成时同步本 plan、HANDOFF、refs/README、kloop README、capability report 与 parity 产物。后台状态不在本计划冒充完成，后续由 Plan 51 的独立证据与完成记录承接。

## 完成标准

- 前台 Bash 八维链有精确 CC fixture 与 kloop 证据。
- permission 差异有安全理由和回归；concurrency 不再靠猜测。
- timeout/取消后无遗留进程。
- 所有门禁全绿，一次提交，提交信息带 `plan50`。

## 开工时定 / 已裁决

- permission/sandbox：保留 kloop 的 deny/sensitive/safety/explicit-ask 优先顺序、真实 shell AST 和 sandbox 双轴；不复制 CC 更宽松的 sandbox-bypass ask 分支，matrix 记 `intentional-diff`。
- timeout/cancel/output：保留 kloop 原生 `timeout_ms` 与 canonical tool-result 文案，不做逐字节适配层；前台输出采用每 fd 150k bytes 有界 drain、合并后 UTF-8 lossy 且 30k 字符模型预算。timeout/cancel 同步 SIGKILL 全进程组并 reap，明确优于 CC stubborn command 转后台后留下 descendants 的观察。
- concurrency：CC 精确 bundle 与 Pre/Post hook probe 证明按输入 read-only 判定；kloop 现有动态分类和连续 safe batch 正确，无需重写调度器，只补 executable pair 与取消回归。

## 完成记录 ✅（2026-07-30）

- 精确 2.1.220 Bash 证据新增 9 个 capture：schema、输出、timeout tree、cancel tree 各有 normalized bytes 完全一致的 determinism pair，并有一个 barrier/serial concurrency singleton；collector 只在隔离 workspace/process probe 中运行，tree fixture 结束后由 collector 明示 `SIGKILL` 清理 CC 遗留进程。
- schema/parser fixture 锁定必填/空/null/错类型 command，timeout 的 null/字符串/负数/0/600000/越界，background/sandbox/description 错类型及 unknown field；bundle exact locator 锚定输入/输出 schema、spawn、permission+sandbox override、timeout cap、stdio/result mapping、30k inline cap、persisted output 与 process-tree kill 入口。
- 输出 fixture 锁定 stdout 后接 stderr、无换行、空输出提示、非 UTF-8 replacement、非零/信号 envelope 和大输出 persisted-output preview。kloop 不复制 CC 文件化大输出 envelope：前台 stdout/stderr 两条 pipe 并行 drain 到 EOF，每 fd 最多保留 150k bytes，合并模型文本最多 30k 字符并报告省略字符/字节，避免双 pipe 死锁与无界内存。
- CC stubborn timeout 在请求 500ms 时按 1s 文案转后台并返回成功，SIGINT cancel 返回 user-rejected/aborted-tools；两者结果后 TERM-ignoring child/grandchild 仍存活。kloop 有意不复制：前台 shell 独立 process group，timeout/cancel 先 SIGKILL 全组、显式 wait/reap direct child、再确认 residual group 消失后才返回；正常 leader 已退出但组内仍有 descendants 也同步清理。Drop guard 只作 panic/非协议 drop 的兜底。
- dispatch cancellation 现在区分“尚未 spawn”和“前台 Bash 已 spawn”：hook/审批期间取消仍直接 drop 且绝不启动命令；spawn 后取消等待 Bash executor 完成 kill/reap，再返回 interrupted tool_result。两个同批只读 Bash 会共同收到取消、各自清完整进程组，并保留每个 tool_use 的配对结果。
- concurrency fixture 用 per-call hook barrier 证明 `pwd`/`ls -d .` 同批重叠，用 redirection 写文件对证明 opaque calls 串行；精确 bundle 的 `isConcurrencySafe(input) → isReadOnly(input)` locator 与真实 kloop `dispatch_tools` report 共同进入第 4 个 generated pair contract `bash-batching`，覆盖两个 Bash concurrency `same` cell。
- corpus 现为 82 captures、109 条 static evidence、56 行/448 单元 matrix：70 `compatible`、90 `intentional-diff`、20 `missing`、236 `unknown`、22 `n/a`、10 `same`；`paired-parity.json` 共 4 个 contract，全部 10 个 `same` 都由 executable comparator 覆盖。Bash executor/permission/output/lifecycle 保留 `intentional-diff`，未取证面继续 `unknown`。
- Plan 51 后续已独立闭环显式后台 completion/failure/cancel 通知、下一 step 回灌与 session cleanup；自动后台化、stall 和逐事件 Monitor 仍是 intentional boundary。Plan 50 本身只声明前台 Bash 与输入依赖并发闭环，不把既有显式 `run_in_background` 扩张成当时尚未证明的 CC 后台状态机。
- 验证：`build_matrix.py`/`--check`、full exact-binary `verify.py`、`verify.py --corpus-only`、Plan 50 Rust report、focused tools/Bash regression、workspace fmt/clippy/tests、mock 与 `git diff --check` 全绿。
- 提交：本实现、证据与完成记录在同一个 `plan50` 提交中，SHA 以本行所在提交为准。
