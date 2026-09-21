# Plan 49 — 文件与搜索工具对齐

> 状态：✅ 已完成（2026-07-29；同日补齐 executable parity 与 corpus CI）
>
> 母计划：Plan 48
>
> 依赖：Plan 48
>
> 固定目标：Claude Code 2.1.220；版本、大小与 SHA-256 以 `refs/claude-code-2.1.220/manifest.json` 为准。

## 背景

Plan 48 只建立了 Claude Code 2.1.220 的工具矩阵与采集基线，并纵向打通了 Glob 的部分行为。开工时 Read、Write、Edit 和 Grep 主要停留在注册面，不能据工具同名、相似 schema 或 kloop 已有测试宣称行为一致。

本计划收敛文件与搜索行为簇：Read、Write、Edit、Glob、Grep，以及 stale-read、路径边界、分页、排序、截断、并发和生命周期。Worktree 生命周期留给 Plan 56，Notebook/LSP 留给 Plan 57。

kloop 开工时还有两个独立产品缺口：Read 尚无有界输出和读取状态；Write/Edit 直接覆盖文件，既不能可靠阻止 stale update，也不能保证写入失败时原文件不出现半写。因此本轮按以下硬顺序推进：

```text
精确 bundle 静态链
  → 隔离、确定性的 CC fixture
  → matrix 证据检查点
  → kloop 产品实现
  → paired kloop golden
  → verifier、文档与全量回归
```

精确 2.1.220 bundle 与隔离黑盒 fixture 是 CC 契约的唯一裁决源；固定参考源码只用于设计 probe。安全性冲突时保留 kloop 更严格的敏感路径、读取新鲜度和失败原子性，并标为 `intentional-diff`，不为追求字面一致而降级。

## 完成裁决

对应 matrix 行：

- `read@clean-cli`
- `write@clean-cli`
- `edit@clean-cli`
- `glob@allow-cli`
- `grep@clean-cli`
- `grep@allow-cli`

最终结论：

- Read/Write/Edit 的注册与 schema 为 `compatible`；Read、Glob、Grep 的只读并发、Write/Edit 串行分类，以及 Glob/Grep 的无孤儿调用 lifecycle 都有 CC executor hook 与 kloop real-dispatch 成对动态证据，标为 `same`。其余结果按实际语义分成 `compatible` 或 `intentional-diff`，不把名字相同外推成逐字节一致。
- 只有最终成功且完整展示的 Read 建立 session mutation observation。kloop 对 existing Write/Edit 要求完整 fresh Read；CC 接受部分 Write 资格、unread/partial Edit，并可对唯一 old string stale-recover，因此执行和 lifecycle 保留有意差异。
- kloop mutation 用 normalized-path keyed lock、同目录 `create_new` 临时文件、权限保留、sync、最终版本复核、atomic rename、parent sync 和失败清理；symlink/非普通文件拒绝。
- Read 文本按字符安全地限制为 7k，PDF 与非 UTF-8 明确拒绝；图片仍为结构化 block。Grep 已补 `context`、`-o`、数字字符串、单文件路径格式和分页；Glob 精确 100 项并把 program 数组与模型字符预算分开。
- Glob/Grep 继续尊重 `.gitignore`、跳 VCS，并在读取前过滤 deny/sensitive 路径并报告 hidden count。CC 的 allowed-path fixture 会列 ignored/VCS/sensitive 内容，这些安全/产品差异不移除。
- Grep 在 CC clean profile 受条件 gate 隐藏、allow profile 才可见；kloop 始终注册。clean 注册标 `intentional-diff`，不是 `missing`。

### 证据与 matrix 闭环（2026-07-29）

CC 侧可重放 corpus、generated pair contract 与 kloop executable report 已共同进入 generator、verifier 和 CI：

- 精确 2.1.220 静态证据现为 95 条：除 adapter/media/permission/result/条件注册链与 kloop code/golden anchors 外，新增了 per-call PreToolUse/PostToolUse bundle call-site 和 kloop parity report 锚点。
- capture 为 73 组：40 组 schema v1、33 组 schema v2；18 个 determinism group 的 normalized bytes 完全一致。`search-policy-concurrency` 继续是 singleton，因为并发组内合法 start/finish 顺序不能被归一化成伪确定性。
- matrix 为 55 行、440 个维度单元：70 `compatible`、83 `intentional-diff`、20 `missing`、237 `unknown`、22 `n/a`、8 `same`。
- `paired-parity.json` 由 `build_matrix.py` 同步生成，3 个 contract 精确覆盖全部 8 个 `same` 单元。`verify.py` 用固定 cargo selector 运行 `tools::plan49_parity_tests::emit_plan49_parity_report`，再比较 CC/kloop 的规范化调用输入、start/finish、result、偏序和 workspace projection；fixture profile 与 matrix cell 不同时，contract 必须逐 cell 声明由精确 bundle 证据支撑的 profile bridge。缺 pair/bridge、输入漂移、0 tests matched、事件缺失/重排或 scenario 漂移都会 fail closed。旧的 row-level `kloop_golden: true` 声明已移除。
- CC 并发 fixture 通过 descriptor-locked hook barrier 证明 Read/Glob/Grep 真正同时进入 per-call execution；dependent `mutation-seriality-1/2` 则证明 Edit/Edit/Write/Write 严格串行。两者都不靠 wall-clock 阈值猜测。
- interactive No/Esc 不再由 harness 强杀：composer 恢复后发送固定 follow-up，第一次 continuation sampling 明确没有被拒调用的 result，下一请求再携带唯一 `is_error` settlement result 后正常 final。kloop 同样保留合法 error pairing，但交付时机和文案仍是 lifecycle/output `intentional-diff`。批准后的 Write stale check、Edit stale recovery 及 workspace 结果继续由既有 fixture 固定。
- `grep@clean-cli` 的 parser/executor/output/lifecycle 因精确 search-tools gate 不适用于该 profile，改为有负注册证据且要求 allow-profile downstream coverage 的 `n/a`；不是把未知静默删除。
- 默认 `verify.py` 仍校验本机 exact binary 与 bundle bytes；`verify.py --corpus-only` 只跳过这两项，仍运行 hashes、tamper、matrix/pair、provider、secret/local-network 和 Rust semantic report，并已接入 macOS/Linux CI。
- 237 个 `unknown` 属于 Plan 50–59 的其他工具簇或当前 profile/平台尚未运行的维度；Plan 49 没有为降低数字越界外推。

该闭环只宣称文件/搜索簇完成，不表示 kloop 已全工具对齐或可替换 Claude Code。

kloop 侧优先复用：

- `rust/crates/core/src/tools/fs.rs` 的文件参数解析和现有工具测试；
- `rust/crates/core/src/tools/search.rs` 的 Rust 原生搜索、过滤、排序、分页和预算；
- `rust/crates/core/src/tools/mod.rs` 的 `builtin_defs`、`all_tool_defs`、`is_concurrency_safe`、`dispatch_tools`、`run_one`、`str_arg` 和 `resolve_path`；
- `rust/crates/core/src/permissions.rs` 的权限顺序、敏感路径和 read deny 过滤；
- `rust/crates/cli/src/user_config.rs` 的 temp → sync → rename → parent sync → cleanup 原子替换模式，只借设计，不跨 crate 调用私有实现。

## 目标

1. 对五个工具逐项走完 registration → schema → parser → executor → permission → concurrency → output → lifecycle。
2. 固定路径解析、相对/绝对路径、目录/文件、缺失路径、敏感路径和 cwd 边界。
3. 固定 Read 的 offset/limit、文本/图片/二进制分支、长行和总输出截断。
4. 固定 Write/Edit 的创建、覆盖、唯一替换、失败原子性、stale-read 和并发修改语义。
5. 固定 Glob/Grep 的排序、过滤、分页、结果上限、字符上限、错误输入和并发分类。
6. 只有同输入的 CC fixture 与 kloop executable report 经 generated pair contract 语义比较后，才把对应维度标为 `same`；fixture profile 必须与 matrix cell 一致，或由 contract 逐 cell 声明 exact-bundle profile bridge，且 bridge evidence 覆盖该维度并被 cell 引用。

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

公开文档、旧源码参考和参考仓只能帮助设计 probe，不能裁决 2.1.220 契约。

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

新增 `rust/crates/core/src/file_state.rs`，并在 `config.rs`/`lib.rs` 接入有界 session state：

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
- pre-hook 后只允许叶文件缺失；直接父目录必须已存在。先 canonicalize 并打开父目录句柄，permission/preview 使用该 canonical target，审批后复核父目录身份。
- 同一路径通过 session keyed lock 串行。
- 锁内 blocking critical section 的 target read、同目录独占临时文件、最终复核、rename、cleanup 和 parent sync 全部相对保留的目录句柄执行，不重新遍历已批准的父路径。
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
- 更新 `build_matrix.py`、`tool-matrix.json`、`paired-parity.json` 和 `verify.py`：每个非 `unknown`/`n/a` 单元引用 exact evidence；`same` 必须引用覆盖该精确 cell 的 generated `pair:` contract，并执行比较 CC fixture projection 与 kloop real-dispatch report。
- 新增 Read/Write/Edit/Grep 静态 anchor 完整性检查；禁止手工修改生成物绕过 generator。
- 同步本 plan、`docs/plan/HANDOFF.md`、`refs/README.md`、`rust/DESIGN.md` 和 `docs/capability-report.md`。只更新当前 matrix 计数，不改写 Plan 48 的历史数字。

## 最终裁决的有意差异

- Grep 在 kloop 始终注册，不复制 CC clean/allow profile 的 search-tools gate。
- Read 的 7k 字符预算只授权实际完整展示的行；PDF 与非 UTF-8 文本明确失败，不通过 lossy conversion 建写权限。
- existing Write/Edit 必须完整 fresh Read；不接受 CC 的 partial Write、unread/partial Edit 或 stale recovery。
- CC 可为新文件隐式创建缺失父目录；kloop 只允许叶文件缺失，要求直接父目录已存在，并以审批前打开的目录句柄固定提交边界。
- Glob/Grep 尊重 `.gitignore`、跳过 VCS internals，并硬过滤 deny/sensitive 路径；输出显式说明 hidden count。
- kloop 的成功/拒绝/取消文案保持 canonical provider history 合法；CC No/Esc 的 error pairing 只在后续 continuation settlement request 中出现，kloop 则在当前工具轮立即返回，时序差异明确保留。

这些差异都在 matrix 中标 `intentional-diff` 并有 paired fixture/golden 或静态链。其他 profile/平台不可运行的维度继续保留 `unknown`，归 Plan 50–59 各自裁决。

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
python3 -B refs/claude-code-2.1.220/verify.py --corpus-only

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

## 完成记录 ✅（2026-07-29）

- 新增 `FileState`：以规范化绝对路径保存内容 SHA-256 + 元数据版本与实际可见 Read 范围，容量、内存、range 数均有硬上限并按最旧访问序确定淘汰；同路径 keyed lock 串行 mutation。状态只活在 session 内，不写 rollout；resume、task 子 agent 和独立 worktree 均 fresh。
- `read_file` 在 permission 前 canonicalize 并 no-follow/nonblocking 打开 regular-file descriptor，权限规则仍保留原始 alias spelling，审批等待中 alias 改指敏感文件也只读已批准 inode；Unix 多 hard-link regular file 因无法按 pathname 安全分类同一 inode 的其他名字而 fail closed；只有最终成功 tool result 才提交 staged observation。post-hook 取消、permission preview、错误与拒绝不授权。文本用 `split('\n')` 保留尾空行，支持 0/数字字符串，EOF/空文件有明确提示，7k 字符截断不切 UTF-8 且只记录完整展示的行；图片保留结构化 block，PDF/non-UTF-8/普通 binary 明确失败。
- existing `write_file` 与全部 `edit_file` 要完整 fresh Read；Read 后被删除也按 stale 拒绝，不退化成新建。pre-hook 后只允许 leaf 缺失，直接父目录必须已存在；canonicalize 父目录并打开 no-follow directory handle 后，permission deny/sensitive/acceptEdits 与审批 preview 使用同一 effective target。批准后复核父目录 dev/inode，随后 target read、同目录 exclusive temp、保权限、sync、最终复核、rename、cleanup 和 parent sync 全部走 descriptor-relative `openat`/`renameat`/`unlinkat`，不重新遍历父路径；审批等待中把 `inside-link/newdir` 换成指向 `.git/hooks` 的 symlink 会拒绝且不落 hook。mutation 尝试先清旧 authority；cwd 内 alias 共用 lock，ancestor symlink 不得逃出 workspace，symlink leaf/非普通目标拒绝。最终成功刷新 observation，post-hook 取消保留已提交文件但不留下资格。target open 带 `O_NONBLOCK`，FIFO swap 不挂 worker；temp name 在 rename 前按 dev/inode + bytes 绑定 opened FD，new file 仍按 0666 经 umask，existing mode 原样保留。严格 hostile same-UID 仍可竞争最后一次 identity check→`renameat` 的微小 namespace 窗口，本 plan 不把 descriptor boundary 宣称成文件系统事务。
- Grep 补齐 `context`、`-o`、数字字符串分页、single-file content 格式和 CC-shaped 分页提示；`usize::MAX` 级 offset/head limit 使用 saturating arithmetic，不 panic/回绕。每个 walker candidate 先通过与 Read 共用的 canonical parent FD + no-follow leaf open 绑定 inode，deny/sensitive 同看 original + resolved path，再由 `search_reader` 搜已打开 descriptor；filter 后 leaf 换成敏感 symlink 也不会泄漏，多 hard-link candidate 作为不可安全分类的 inode alias 隐藏并计数。Glob 的空 pattern、100 项 cap、字符截断后的实际 shown/remaining 计数与 program path array 已锁定。Read/Grep/Glob 统一使用 UTF-8 安全 7k 模型文本预算，搜索继续尊重 `.gitignore`、跳 VCS、过滤 deny/sensitive 并报告 hidden count。
- CC `scripted-read-edit` 原把两个 Read 放同轮，重采时合法并发结果顺序翻转导致 determinism pair 失败；已拆成顺序 round，保留 stale Edit 语义且不扩大 normalization。Plan 48 的 40 份历史 capture/hash 已恢复不动，只更新该 paired fixture。
- `static-evidence.jsonl` 为 95 条，`tool-matrix.json` 由 generator 生成 55 行/440 单元：70 `compatible`、83 `intentional-diff`、20 `missing`、237 `unknown`、22 `n/a`、8 `same`。`paired-parity.json` 的 3 个 contract 精确覆盖全部 8 个 `same`；verifier 运行固定 Rust report test 后比较规范化调用输入、生命周期、result、偏序与 workspace，并要求跨 profile cell 有 exact-bundle bridge，不再接受不可执行的 `kloop_golden` 声明。
- 裁决：kloop 不复制 CC 的隐式缺失父目录创建、partial Write qualification、unread/partial Edit、unique-match stale recovery、clean-profile Grep gate或广义 ignored/VCS/sensitive 搜索可见性。PTY 复核订正旧结论：No/Esc 在恢复 composer 后先产生一次无 result 的 continuation sampling，随后 settlement request 带唯一 error tool_result 再自然 final；kloop 的立即 error pairing 仍记 lifecycle/output `intentional-diff`。文件/搜索簇完成不外推 Plan 50–59。
- 证据补强新增 descriptor-locked PreToolUse barrier、dependent Edit/Edit/Write/Write deterministic pair、真实 `dispatch_tools` event/call report、同输入比较、跨 profile exact-bundle bridge、generated pair coverage 门和 `--corpus-only` CI。Plan 48 的 40 份历史 capture/hash 保持不动。
- 验证：`collect.py identity`、`collect.py list`、73-case corpus 重放/changed-case publication、`build_matrix.py`/`--check`、full exact-binary `verify.py`、`verify.py --corpus-only`、Plan 49 parity report test、focused fs/search/file_state tests、workspace fmt/clippy/tests、mock 与 `git diff --check` 全绿。
- 提交：产品实现为 `208e7cf`；本证据补强与完成记录在同一个 `test(plan49)` follow-up 提交中，SHA 以本行所在提交为准。
