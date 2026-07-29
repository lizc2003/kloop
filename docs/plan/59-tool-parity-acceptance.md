# Plan 59 — 工具对齐总体验收

> 状态：未开工
>
> 母计划：Plan 48
>
> 依赖：Plan 49–58
>
> 固定目标：Claude Code 2.1.220；版本身份以 Plan 48 manifest 为准。

## 背景

Plan 49–58 按行为簇补齐单工具纵向证据。Plan 59 不新增工具，也不替代子计划修复缺口；它负责验证这些局部结论在同一精确二进制、同一证据规则和跨工具生命周期下仍然成立。

`tool-matrix.json` 没有、也不需要 `child_plan: 59` 的工具行。不得为了让本计划“有对应行”而伪造 matrix row、删除 unknown 或将 kloop-only 能力折算为 Claude Code parity。

## 当前证据与差距

对应 matrix 行：

- 无专属行；当前验收 matrix 为 55 行。后续 Plan 50–58 若按证据新增 profile/surface 行，以开工时 generator 产物为准，不为 Plan 59 伪造专属 row。

当前结论：

- Plan 48 已固定 2.1.220 identity、collector、manifest、static evidence、matrix schema、38 组历史 fixtures 和 fail-closed verifier。
- Plan 49 已完成，并提供首个可复用的 executable pair 基线：`paired-parity.json` 由 matrix generator 同步生成，固定 Rust report test 运行真实 dispatcher，full/corpus-only verifier 比较 CC/kloop 共同语义；corpus-only 已进 macOS/Linux CI。
- Plan 50–58 在本计划开工前仍必须分别完成；当前不能预写其最终兼容结论。
- 单工具/pair contract 不能自动证明文件/Bash/Worktree、Agent/后台/调度、MCP/Web、plan/PTY 等跨工具链。
- “零 unknown”不是机械验收指标；平台/入口排除行和条件 gate 可保持有证据理由的 `n/a`/`unknown`。
- 只有本计划完成后，才允许重新评估“全工具已对齐”或“可替换 Claude Code”的产品表述。

优先复用：

- Plan 48 的 exact binary identity guard、collector、manifest、matrix generator 与 verifier
- Plan 49–58 的 raw/normalized fixtures、static evidence、kloop executable report/golden、generated pair contract 和完成裁决
- `refs/claude-code-2.1.220/` 下的全部 parity 产物
- kloop 全 workspace 测试、mock smoke test 与前端/session 清理 seam

## 目标

1. 审计 Plan 49–58 的 matrix、manifest、fixture、static evidence、golden 和文档引用完整性。
2. 对所有本机可运行 profile 做确定性重放，确认身份、网络、敏感信息和 normalization 门持续 fail closed。
3. 建立跨工具链 fixtures，验证 cwd、权限、并发、取消、通知、回灌和 cleanup 不因组合而改变。
4. 对每个 `same`、`compatible`、`intentional-diff`、`missing`、`unknown`、`n/a` 检查证据与产品裁决是否匹配。
5. 区分当前平台已执行结论、不可运行分支和 kloop-only 能力，不用聚合数字掩盖边界。
6. 输出可审计的最终 capability 结论；任何功能缺陷退回对应子计划修复。

## 开工证据闸门

- Plan 49–58 全部达到各自完成标准、提交已存在、质量门全绿且文档同步完成。
- exact binary identity 仍与 Plan 48 manifest 完全一致；目标文件漂移时立即停止，不自动升级版本。
- matrix 每行均映射到子计划裁决或明确排除理由；Plan 59 不接管未完成的行为实现。
- 所有非 `unknown`/`n/a` 维度都有可定位 evidence；`static:*` locator 的 `covers` 必须包含当前维度。
- executor/output/lifecycle 等运行维度必须有 CC 调用 fixture，不能只引用 registration capture 或工具描述。
- 双边兼容状态必须同时有 CC 与 kloop 对应证据；所有 `same` 都由 generated pair contract 精确覆盖，并在 verifier 中对同一规范化输入执行共同语义 projection 比较。fixture profile 与 matrix cell 不同时，必须逐 cell 声明 exact-bundle profile bridge，且证据覆盖当前维度；不能退化成布尔声明或源码行号门。
- collector、generator、verifier 可从 clean checkout 运行；生成产物与已提交字节一致。
- 跨工具 case 使用临时 HOME/config/cwd/repo/storage 与本地 stub，不访问公网、真实用户会话或用户仓库。

## 实施切片

### 0. 产物与引用审计

- 校验 Plan 49–58 编号、依赖、完成提交、matrix rows 和文档结论一一对应。
- 校验 raw/normalized hash、profile condition vector、case exact set、static locator 和 golden 引用无悬空。
- 校验 generator byte-deterministic；verifier 对未知行、未引用 fixture、非法 normalization 和敏感信息 fail closed。
- 扩 verifier 校验 evidence 的维度归属、运行维度的执行 fixture，以及双边状态的 CC/kloop 成对证据。

### 1. 单工具 profile 重放

- 重放所有当前平台可执行 profile，并对声明 deterministic 的 case 至少重复运行。
- 比较注册顺序、schema、parser 错误、permission、并发、output 和 lifecycle。
- 对不可运行 profile 保存条件与负证据；不以 clean profile 缺席推断不存在。

### 2. 跨工具链验收

至少建立以下组合：

- 文件与搜索 → 前台 Bash → Worktree：cwd、stale-read、权限、dirty/clean 和恢复；
- Bash/Monitor → Agent/Task → Cron/ScheduleWakeup：后台完成、取消、通知、回灌和 session exit；
- ToolSearch/dynamic MCP/resources → Web：defer refresh、local-only 网络、权限顺序和失败隔离；
- AskUserQuestion/Plan/Workflow → PTY/headless：状态转换、批准/拒绝/取消与进程组清理；
- Notebook/LSP → 文件修改：stale-read、并发请求、取消和外部进程清理。

组合 case 只验证已由子计划裁决的接口，不在本计划猜测或新增隐藏契约。

### 3. 安全、并发与生命周期审计

- 断言没有 stub 外 egress、真实凭据、用户路径、会话 token 或未声明随机值进入产物。
- 覆盖并发工具、排队、取消隔离、超时、断线、崩溃和父会话退出。
- 结束后检查文件、worktree、branch、socket、timer、后台任务、子进程和临时存储无残留。
- kloop 更严格的安全边界优先保留，并以 `intentional-diff` 和用户可见说明记录。

### 4. 最终裁决与产品表述

- 汇总每个行为维度的状态、证据、可运行条件、剩余 unknown 和 kloop-only 能力。
- 功能缺陷退回 Plan 49–58 对应计划修复并重新验收；不在验收提交中顺手实现。
- 只在证据支持的范围内更新 capability report、README 和替换性表述。
- 若仍有阻断性的 missing/unknown 或跨链失败，Plan 59 保持未完成。

## 非目标与有意保留

- 不新增、重写或重命名产品工具来“通过验收”。
- 不伪造 `child_plan: 59`、删除 matrix row、放宽 verifier 或扩大 normalization。
- 不把 `run_program`、`read_offloaded`、`call_tool` 等 kloop-only 能力计作 CC 缺口闭环。
- 不以公开文档、其他 Claude Code 版本、参考仓实现或工具同名替代 2.1.220 fixture。
- 不要求两个平台/入口排除行从 `n/a` 变为可运行，也不要求未取得 profile 的条件分支机械变为非 `unknown`；“零 unknown”不是唯一完成条件。
- 不访问公网、真实 MCP/Web/remote/team 服务或用户数据。
- 不逐字节复制 UI 文案、内部架构或非必要实现细节。

## Fixture 与测试

至少覆盖：

- exact identity、manifest/hash、static evidence、matrix schema、fixture exact set 和 sensitive scan；
- 所有当前可运行 profile 的重复采集与 byte-deterministic regeneration；
- 文件+Bash+Worktree、Agent+后台+调度、MCP+Web、plan+PTY、Notebook+LSP 跨链；
- permission allow/deny、并发/排队、timeout/cancel、失败隔离、通知与回灌；
- session exit、异常、断线与 collector 中断后的 cleanup；
- 两个平台/入口 `n/a` 排除行，以及 remote/team 和依赖条件分支的保守 `unknown` 理由；
- kloop-only 行与 intentional-diff 的用户可见边界。

验证：

```bash
python3 -B refs/claude-code-2.1.220/verify.py
python3 -B refs/claude-code-2.1.220/verify.py --corpus-only
cd kloop
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p kloop -- --mock
cd ..
git diff --check
```

另从 clean checkout 重放 collector、generator 和跨工具 acceptance suite；具体命令以 Plan 49–58 完成后提交的入口为准，不在本计划预写不存在的脚本。

## 文档同步

完成时同步本 plan、HANDOFF、refs/README、kloop README、capability report、matrix、manifest、static evidence、fixture 索引与验收产物。明确目标版本、可运行 profile、所有有意差异、剩余 unknown 和 kloop-only 能力。

## 完成标准

- Plan 49–58 全部完成，引用、提交、产物与文档一致。
- exact identity、确定性重放、敏感信息、local-only 网络和所有质量门全绿。
- 单工具与跨工具链的权限、并发、取消、通知、回灌和 cleanup 均有确定性证据。
- 每个剩余 `unknown` 都有不可运行条件和后续裁决边界；没有用删 row、猜测或放宽 verifier 收敛。
- 没有阻断性的 `missing`、未裁决 intentional-diff 或跨链失败。
- 一次纯验收/文档提交；任何产品修复保留在对应子计划提交中。
- 仅在以上条件全部满足后，重新评估“全工具已对齐”或“可替换 Claude Code”的表述。

## 开工时定 / 问用户

- 哪些 intentional-diff 属于可接受的产品优势，哪些仍阻断替换性声明。
- 不可运行平台与 remote/team 分支需要达到的证据深度和发布说明边界。
- 最终 capability report 使用“行为兼容”“受限兼容”还是继续避免替换性表述。
