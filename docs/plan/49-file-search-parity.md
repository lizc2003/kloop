# Plan 49 — 文件与搜索工具对齐

> 状态：进行中（2026-07-28）
>
> 母计划：Plan 48
>
> 依赖：Plan 48
>
> 固定目标：Claude Code 2.1.220；版本、大小与 SHA-256 以 `refs/claude-code-2.1.220/manifest.json` 为准。

## 背景

Plan 48 只建立了 Claude Code 2.1.220 的工具矩阵与采集基线，并纵向打通了 Glob 的部分行为。Read、Write、Edit 和 Grep 目前主要停留在注册面，不能据工具同名、相似 schema 或 kloop 已有测试宣称行为一致。

本计划收敛文件与搜索行为簇：Read、Write、Edit、Glob、Grep，以及 stale-read、路径边界、分页、排序、截断、并发和生命周期。Worktree 生命周期留给 Plan 56，Notebook/LSP 留给 Plan 57。

kloop 当前还有两个独立产品缺口：Read 尚无有界输出和读取状态；Write/Edit 直接覆盖文件，既不能可靠阻止 stale update，也不能保证写入失败时原文件不出现半写。因此本轮按以下硬顺序推进：

```text
精确 bundle 静态链
  → 隔离、确定性的 CC fixture
  → matrix 证据检查点
  → kloop 产品实现
  → paired kloop golden
  → verifier、文档与全量回归
```

精确 2.1.220 bundle 与隔离黑盒 fixture 是 CC 契约的唯一裁决源；固定参考源码只用于设计 probe。安全性冲突时保留 kloop 更严格的敏感路径、读取新鲜度和失败原子性，并标为 `intentional-diff`，不为追求字面一致而降级。

## 当前证据与差距

对应 matrix 行：

- `read@clean-cli`
- `write@clean-cli`
- `edit@clean-cli`
- `glob@allow-cli`
- `grep@clean-cli`

当前结论：

- Read/Write/Edit 只有 registration、schema 为 `compatible`；parser 到 lifecycle 均未成对核验。
- Glob 的 parser、executor、output 有黑盒 fixture，但只保守标 `compatible`；permission、concurrency 仍为 `unknown`。
- Grep 没出现在 clean CC profile；这只说明该 profile 下未观察到，不能推断工具不存在或不适用。
- 现有 Glob fixture 覆盖最小输入、缺字段、错类型、空 pattern、100 文件上限和字符截断。
- bundle 中 Notebook/Edit stale-read 稳定字符串只是静态线索，不能代替 Edit 完整链或 NotebookRead 注册证据。

### CC 证据检查点（2026-07-28）

产品实现前的 CC 侧闸门已形成可重放 corpus，暂不把任何行为标为 `same`：

- 精确 2.1.220 静态证据增至 82 条，补齐 Read media/result、Write/Edit permission/result/inherited concurrency、Glob permission/concurrency，以及 Glob/Grep 条件注册过滤链。
- capture 增至 71 组：40 组 schema v1、31 组 schema v2；17 个 determinism group 均为两次 normalized bytes 完全一致，same-round concurrency 仍是显式 singleton，不伪装 deterministic pair。
- matrix 增至 55 行，当前为 47 `compatible`、43 `intentional-diff`、20 `missing`、311 `unknown`、19 `n/a`、0 `same`。新增 evidence 只填引用；没有 paired kloop golden 的维度继续保持 `unknown`。
- Read fixture 已固定文本范围/错误/截断，以及 non-UTF-8、PNG、PDF、普通二进制和工作区内 symlink 分支；PDF 页提取在本机缺少 poppler 时保留明确依赖错误，不外推跨平台结果。
- Write/Edit fixture 已固定 prior-read、partial/unread、stale、匹配与 path-kind 行为。interactive pair 证明 stale 检查发生在批准后：Write 拒绝覆盖已外部修改的文件；Edit 在 old string 对当前内容仍唯一时 stale-recover，并保留无关外部修改。
- Write/Edit 的明确 No 与 Esc cancel 均不产生最终 round 的 tool result，workspace 保持不变。交互 fixture 的进程由 harness 在 provider final 或按键后受控收尾；raw returncode 保留实值，normalized 以已声明的 termination policy 消除 0/143 收尾竞态。
- Grep 的可见性由 CLI opt-in 与默认 suppression/final pool filter 共同控制；clean profile 缺席仍只作负观察。search-policy capture 使用 case-scoped allow override，只证明 allowed-path 下的 hidden/VCS/gitignore、敏感文件读取和顺序，不证明默认 ask/deny gate。
- Write/Edit descriptor 未覆写 concurrency/read-only 方法，证据只在完整 descriptor span 零命中并结合 shared adapter defaults 后裁决为继承 `false`；不是仅凭字符串缺席推断。

该检查点只完成 CC 事实取证和 matrix 引用，不修改 kloop 产品。下一步仍从 session-scoped observation 与 stale-safe 原子提交开始。

kloop 侧优先复用：

- `kloop/crates/core/src/tools/fs.rs` 的文件参数解析和现有工具测试；
- `kloop/crates/core/src/tools/search.rs` 的 Rust 原生搜索、过滤、排序、分页和预算；
- `kloop/crates/core/src/tools/mod.rs` 的 `builtin_defs`、`all_tool_defs`、`is_concurrency_safe`、`dispatch_tools`、`run_one`、`str_arg` 和 `resolve_path`；
- `kloop/crates/core/src/permissions.rs` 的权限顺序、敏感路径和 read deny 过滤；
- `kloop/crates/cli/src/user_config.rs` 的 temp → sync → rename → parent sync → cleanup 原子替换模式，只借设计，不跨 crate 调用私有实现。

## 目标

1. 对五个工具逐项走完 registration → schema → parser → executor → permission → concurrency → output → lifecycle。
2. 固定路径解析、相对/绝对路径、目录/文件、缺失路径、敏感路径和 cwd 边界。
3. 固定 Read 的 offset/limit、文本/图片/二进制分支、长行和总输出截断。
4. 固定 Write/Edit 的创建、覆盖、唯一替换、失败原子性、stale-read 和并发修改语义。
5. 固定 Glob/Grep 的排序、过滤、分页、结果上限、字符上限、错误输入和并发分类。
6. 只有同 profile、同输入的 CC fixture 与 kloop golden 成对后，才把对应维度标为 `same`。

## 已拍板边界

- CC stale-read 与 kloop 安全策略冲突时，保留 kloop 更严格的 prior-read、freshness、敏感路径和失败原子性边界，并登记 `intentional-diff`。
- Glob/Grep 的排序、`.gitignore`、hidden/VCS、敏感路径和截断先取证；kloop 现有行为若更安全或有明确产品价值则保留，不为字面一致移除。
- Grep 的真实注册 gate 先由精确 bundle/profile 裁决。只有能自然映射到 kloop 现有 deferred-tool/条件注册机制时才对齐；若属于 CC 专属基础设施，则保持 kloop 搜索能力可用并记录有意差异。
- permission diff preview 即使读取了文件，也不能建立“模型已成功 Read”的写入资格。
- file observation 不落 rollout；恢复会话、task 子 agent 和独立 worktree 均使用 fresh state。Worktree 生命周期仍归 Plan 56。
- session keyed lock 只封住 kloop 内并发；对外部进程只承诺最后复核前的 stale 检测和 rename 的原子可见性，不宣称消除复核与 rename 之间的外部竞态。

## 开工证据闸门

1. 从精确 2.1.220 bundle 补 Read、Write、Edit、Grep 的注册条件、schema、parser、权限、并发和结果适配静态链；每条写入 `static-evidence.jsonl`。
2. 为 Grep 找到真实可见 profile；clean profile 的缺席继续保留为负观察，不转成 `missing`。
3. 扩 `collect.py`、fake provider 和合成 workspace；所有写操作只允许落在每 case 的临时根内。
4. 每个新 case 同时保存 raw/normalized capture、hash、profile 条件和受限 normalization 规则。
5. 新增或改变的 deterministic profile 连跑两次，normalized bytes 必须一致。
6. 在产品修改前先运行 `build_matrix.py` 更新已证实单元格；证据不足的单元格保持 `unknown`。

公开文档、旧逆向源码和参考仓只能帮助设计 probe，不能裁决 2.1.220 契约。

## 实施切片

### 1. 注册、schema 与精确静态链

- 固定五工具的可见条件、名称/alias、原始 schema 和工具顺序。
- 定位 Read、Write、Edit、Grep 的 adapter 入口，以及 registration、schema、parser、permission、concurrency、result mapping；证据记录目标 SHA、byte offset、稳定邻近串和实际覆盖维度。
- 沿最终工具数组条件过滤链确定 Grep 的真实 gate/profile；`grep@clean-cli` 的缺席不能推成 `missing`。
- 对照 kloop 注册与动态并发分类登记差异；证据检查点完成前不改产品代码。

### 2. 隔离 fixture harness

修改 `refs/claude-code-2.1.220/collect.py` 与 `refs/claude-code-2.1.220/stubs/fake_provider.py`，在现有单 tool-use case 之外支持：

- 一个 provider step 返回多个 tool_use，以结构化 start/result 事件顺序验证并发，不用 wall-clock 阈值猜测；
- 跨 round scripted sequence，例如 Read → workspace mutation → Write/Edit，以及连续 Read/Write/Edit；
- 只作用于合成 workspace 的受限 mutation/synchronization action，执行前验证目标位于临时根并把动作与阶段写入 capture；
- PTY 审批在拒绝、取消、批准及批准前后设置确定性同步点，以裁决 permission、stale check 和实际写入顺序；
- `workspace_after` 记录文件内容、存在性和必要元数据。

更新 `manifest.json` 的 case/profile、raw/normalized hash 和 normalization 声明；`verify.py` 对脚本动作白名单、临时根逃逸、capture 文件集合和 determinism fail closed。动态字段只允许归一化临时根、随机 ID、时间、PID 和端口，不能归一化工具顺序、错误、截断提示或状态转换。

### 3. 五工具行为 fixture 与 matrix 检查点

先采 CC raw/normalized fixture，再更新 matrix；检查点完成前不改 kloop 产品。

#### Read

- 最小读取；相对/绝对路径。
- offset/limit 缺省、零、负数、越界和错类型。
- 空文件、长行、长文件、目录、缺失路径、非 UTF-8、图片、PDF 和普通二进制。
- 行号、分页/截断提示、错误 envelope、敏感路径和同轮多 Read。

#### Write

- 新建、覆盖、父目录缺失、空内容、非普通文件和符号链接。
- 未先 Read、完整/部分 Read 后写、Read 后外部修改。
- 权限拒绝、取消、批准以及各分支后的 workspace 状态。

#### Edit

- 唯一/缺失/重复 old string、`replace_all`、空 old/new、同值替换和缺失文件。
- 完整/部分 Read 后编辑；Read 后及审批同步点外部修改。
- 所有失败分支是否保持原内容。

#### Glob 与 Grep

- 延用 Plan 48 Glob cases，补 permission 和 concurrency。
- Grep 覆盖最小输入、缺字段、错类型、空过滤器、坏 regex、无匹配、glob/type/context/multiline、offset/head limit 和大结果集。
- 固定两者的排序、输出格式、结果/字符上限、`.gitignore`、hidden/VCS 和敏感路径策略。

#### Lifecycle 与 concurrency

- 同一会话跨 round 的读取状态、写后再写、错误/取消后状态。
- 只读同批与读写混合批的 start/result 顺序。
- 静态 predicate 与确定性事件证据必须相互印证；不能只凭耗时推断并发。

### 4. Session-scoped 文件观察状态

新增 `kloop/crates/core/src/file_state.rs`，并在 `config.rs`/`lib.rs` 接入有界 session state：

- 以 `resolve_path` 锚定并规范化的绝对路径为 key，记录文件身份/元数据、内容指纹、Read 范围及是否完整。
- 容量和内存设硬上限，并使用确定性淘汰。
- 只有最终返回成功 tool_result 的模型可见 Read 才提交 observation；preview、失败、拒绝和取消均不得建立资格。
- Write/Edit 成功后以新内容刷新 observation；失败或提交结果不确定时清除对应条目。
- 在 `tools/mod.rs::run_one` 的最终结果边界提交或丢弃 staged update，避免 executor 已读但最终返回 `interrupted` 时留下伪 observation。
- 继续保持 hooks → permission → executor → post-hook 的既有顺序。

### 5. Stale-safe 原子 Write/Edit

在 `tools/fs.rs` 中复用 `str_arg`、`resolve_path`，让 Write/Edit 使用同一安全提交 helper：

- prior-read、完整/部分读取资格和 freshness 规则由 fixture 裁决；若 CC 更宽，kloop 保持已记录的严格 fail-closed 边界。
- stale 检查在 permission 批准后、实际写入点重新执行；preview 读取不参与判断。
- 同一路径通过 session keyed lock 串行。
- 锁内 blocking critical section 完成最终复核、Edit 替换计算、同目录独占临时文件写入、flush/sync、原子 rename、父目录 sync 和失败清理。
- 保留既有文件权限；符号链接和非普通文件按 fixture 裁决且不得降低安全性。

### 6. Read、Glob、Grep 产品对齐

- Read 的 parser、行号、分页、总输出预算仅按 fixture 实现；长行与总量使用共享的字符安全截断 helper，不切断 UTF-8。
- 图片继续走 `ToolResultContent` 结构化 block。
- PDF 只有在精确 fixture 和 kloop canonical provider wire 都可表达时才对齐；否则返回明确 unsupported error、增加 paired 回归并标 `intentional-diff`。
- `search.rs` 只修改 fixture 已证明且适用于 kloop 的 parser、排序、过滤、分页和截断差距。
- 保留 `is_concurrency_safe` 的按入参分类和 `dispatch_tools` 的连续 safe/unsafe 分批，并用整对象分类测试和受控事件顺序测试锁定。

### 7. Paired golden、matrix 与文档闭环

- 在 `tools/fs.rs`、`tools/search.rs`、`tools/mod.rs`、`permissions.rs` 的测试中，为 CC fixture 建立同输入的完整 `ToolResultContent`/`ContentBlock`、事件顺序和 workspace-after 断言。
- 用确定性故障注入验证原子写错误时原文件不变、临时文件清理。
- 覆盖 observation 容量/淘汰、partial/full read、取消、写后刷新、同路径竞争和 sub-agent fresh state。
- 更新 `build_matrix.py`、`tool-matrix.json` 和 `verify.py`：每个非 `unknown`/`n/a` 单元引用 exact evidence；`same` 同时引用 CC capture 与 `kloop:` golden，并设置 `kloop_golden: true`。
- 新增 Read/Write/Edit/Grep 静态 anchor 完整性检查；禁止手工修改生成物绕过 generator。
- 同步本 plan、`docs/plan/HANDOFF.md`、`refs/README.md`、`kloop/README.md` 和 `docs/capability-report.md`。只更新当前 matrix 计数，不改写 Plan 48 的历史数字。

## 不提前承诺的行为

- Grep 在哪个 profile 注册、是否 defer，以及 CC 的真实并发 predicate。
- Read 的精确 offset/limit、PDF/二进制、长行和截断格式。
- Write/Edit 是否允许未读新建/覆盖、partial read、空 `old_string`、模糊匹配和符号链接。
- Glob/Grep 的排序、`.gitignore`、hidden 和字符预算。

这些必须先由 2.1.220 fixture 裁决。无法在当前平台确定性运行的分支保留 `unknown`，并写明不可运行条件和后续归属。

## 非目标与有意保留

- 不实现 NotebookRead/NotebookEdit 或 LSP。
- 不处理 Worktree 创建、保留和清理。
- 不逐字节复制 UI 文案或 prompt。
- 不因 CC 行为削弱 kloop 敏感路径、权限或原子写入防线。
- 不提交目标二进制、真实 HOME/config/session、凭据或用户仓库内容。

## Fixture 与测试

至少覆盖：

- 最小合法输入、缺必填、错类型、空值和边界值；
- 成功、典型失败、权限拒绝和取消；
- stale-read、并发读取/写入、读写同一路径；
- 100+ 文件、长路径、长行、长输出和截断提示；
- CC raw/normalized fixture 与 kloop 整对象 golden 的成对断言；
- Read → Edit、Read → external mutation → Edit、并行 Read/Glob/Grep、权限拒绝/批准四条端到端路径。

验证命令：

```bash
python3 -B refs/claude-code-2.1.220/collect.py identity
python3 -B refs/claude-code-2.1.220/collect.py list
python3 -B refs/claude-code-2.1.220/collect.py collect --all
python3 -B refs/claude-code-2.1.220/build_matrix.py
python3 -B refs/claude-code-2.1.220/verify.py

cd kloop
cargo test -p kloop-core tools::fs::tests
cargo test -p kloop-core tools::search::tests
cargo test -p kloop-core file_state::tests
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p kloop -- --mock
cd ..
git diff --check
```

具体测试模块名若开工时已变化，以当时代码为准，不通过删测试缩小覆盖。

## 完成标准

- 当前平台可执行的五工具八维链均有 fixture 与 kloop 证据。
- 每个旧 `compatible`、`missing`、`unknown` 都已裁决，或以不可运行条件和后续归属明确保留。
- `same` 全部通过成对 golden 门；有意差异有理由和回归。
- verifier、focused tests、workspace 门禁、mock 和 diff check 全绿。
- 本 plan 有完成记录、裁决、验证结果和提交号。
- 一次提交，提交信息带 `plan49`。
