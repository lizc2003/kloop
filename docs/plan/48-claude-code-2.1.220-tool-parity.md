# Plan 48 — Claude Code 2.1.220 工具对齐基线与路线图

## 背景

Plan 47 只修复了 grep 的精确空过滤器。kloop 目前没有覆盖全部工具的 Claude Code
golden/parity fixture，不能据工具名、公开文档或 Viewer 的候选差异表宣称“全工具已对齐”。

本轮把目标版本固定为 **Claude Code 2.1.220**，先建立可审计、可重放的版本基线，再按
行为簇推进实现。公开文档只能帮助设计 probe，不能填补 JSON schema、参数解析、权限顺序、
并发、输出和生命周期事实。

## 固定参考载体

目标二进制：本机安装的 `2.1.220 (Claude Code)`,只此一个版本。

二进制本体不进入仓库,它的指纹(大小、SHA-256、构建标识)也不写进文档——采集器在本机
校验,不符立即退出,禁止把其他版本生成的结果混进本基线。

三个代码库只作架构参考：

- 本机的一份 cc 源码参考：不是 2.1.220，不能裁决当前契约。
- `refs/codex`：commit
  `bb21ed4b8d8f74567cd6fecb3c7d4fba795bc6e3`。
- `refs/claw-code`：commit `4ea31c1bc91c4e9bcbd67d51c550c01e127e6d0d`；只取经
  独立核验的局部设计，不把模拟实现当产品语义。

## 目标与边界

Plan 48 是“**版本基线 + fixture 基建 + parity matrix + 后续路线图**”母计划，不在一次
提交中重写所有工具。

对齐的判断单位不是“名字相同”，而是：

```text
注册条件 → schema → parser → executor → permission → concurrency → output → lifecycle
```

目标是：

1. 对模型可见契约和关键生命周期建立精确证据；
2. 能直接对齐的行为标明并进入后续实现计划；
3. wire/名称不同但能力兼容的行为明确记录兼容层；
4. kloop 有价值的独立设计保留为有意差异；
5. Claude Code 产品基础设施专属能力先判定适用性，不因“bundle 中存在”就机械照搬。

## 证据规则

证据优先级固定为：

1. 精确 2.1.220 二进制身份；
2. 精确 bundle 静态证据；
3. 2.1.220 黑盒 fixture；
4. kloop 当前代码与测试；
5. 三个参考库在固定 commit 上的架构交叉核验。

公开文档、Viewer 表和旧 plan 备忘不得为 parity matrix 单元格作证。

每条证据记录：来源类型、目标版本/commit、claim、定位信息和覆盖到的链路环节：

- bundle：byte offset + 附近稳定字符串/符号；
- fixture：profile、case、raw capture SHA-256、normalized fixture 路径；
- kloop：repo-relative `file:line` + 对应测试；
- reference：仓库 commit + `file:line`，并标注“仅架构参考”。

已确认的 2.1.220 静态锚点先入账（锚点本身只在本机的语料里，不抄进文档）。

这些锚点只是起点；没有追完整链或黑盒 fixture 的维度仍标 `unknown`。

## 仓库产物

新增 `refs/claude-code-2.1.220/`：

```text
manifest.json                 # 目标身份、profile、采集命令、raw hashes
static-evidence.jsonl         # bundle claim + offset + 稳定定位串
tool-matrix.json              # 机器可读 parity matrix
collect.py                    # 仅 Python 标准库；采集/归一化入口
verify.py                     # identity、schema、hash、fixture 稳定性与敏感信息校验
fixtures/
  raw/                        # 隔离环境中的原始请求/事件/结果
  normalized/                 # 声明式归一化后的 golden
stubs/                        # 本地 fake provider、Web/MCP fixture 服务
```

不提交目标二进制、真实 HOME、真实配置、key、token、用户会话或用户仓库内容。

## Profile 设计

实际工具集受 feature、平台、入口、权限模式、plan/worktree/team/remote、MCP、defer 和 depth
影响，不存在一张无条件静态全量表。

每个 fixture 带完整条件向量：

- `platform`
- `entrypoint`
- `feature_flags`
- `permission_mode`
- `plan`
- `worktree`
- `team`
- `remote`
- `mcp`
- `defer`
- `depth`

不跑完整笛卡尔积。采用：

1. 一个隔离的 macOS arm64 clean CLI 基准 profile；
2. 每次只改变一个条件的单因素 profile；
3. bundle 已证实有交互的成对 profile；
4. 不能在当前平台真实运行的条件标 `unknown`，不推断结果。

## Parity matrix

一行表示“逻辑能力 + profile + 可见变体”。至少包含：

- `id`、`family`、`surface_kind`；
- `cc_name`、`kloop_name`；
- `profile` 与完整条件向量；
- `registration`、`schema`、`parser`、`executor`、`permission`、`concurrency`、
  `output`、`lifecycle`；
- `evidence_refs`、`child_plan`、`notes`。

`surface_kind` 区分 model-visible tool、internal adapter、workflow primitive、MCP/server surface；
不能因为 bundle 中出现 `StructuredOutput`、`Workflow` 等名称就直接判为所有 profile 下的模型工具。

每个行为维度只能使用：

- `same`
- `compatible`
- `intentional-diff`
- `missing`
- `unknown`
- `n/a`

`same` 必须有 CC fixture 与 kloop golden 配对。只有名称、描述、schema 或旧参考实现时不得标
`same`。`unknown` 必须显式保留，不能通过删行制造“已覆盖”。

## 黑盒 fixture 方法

### 注册面

使用隔离 HOME、合成 cwd 和本地 fake provider 启动精确二进制，捕获实际发送给模型的完整工具
数组：

- 保留工具定义顺序；
- 保留原始 schema；
- 保存启动参数与环境变量白名单；
- 每个 profile 独立采集；
- 采集前执行 identity guard。

### 执行面

fake provider 返回确定性的 tool use，让 2.1.220 真正执行，再捕获下一轮 tool result。每个
被纵向核验的工具至少覆盖：

- 最小合法输入；
- 缺必填字段；
- 错类型；
- 空值/边界值；
- 成功输出；
- 典型失败；
- 权限、取消、截断或生命周期分支（适用时）。

需要真实交互审批的 fixture 用 PTY 单列，不能用 headless 结果替代。Web 和 MCP 只接本地 stub；
禁止把公网和实时服务结果做成 golden。

### 原始与归一化

同时保存 raw capture 和 normalized golden。raw 文件写 SHA-256 到 manifest。

只允许归一化：临时根目录、随机 ID、时间、PID、端口。归一化规则必须在 fixture 中声明。
工具顺序、schema、状态、错误文案、截断提示和生命周期事件不得随意归一化。

clean profile 连跑两次，normalized 结果必须完全一致。

## Plan 48 最低覆盖

所有已知工具先进入 registration matrix：

- Read/Write/Edit/Glob/Grep/Bash；
- NotebookRead/NotebookEdit；
- WebFetch/WebSearch；
- Skill/ToolSearch；
- AskUserQuestion；
- Agent、TaskCreate/Get/List/Update/Output/Stop 及兼容别名；
- Enter/ExitPlanMode、Enter/ExitWorktree；
- Monitor；
- CronCreate/Delete/List、ScheduleWakeup；
- SendMessage/ListAgents；
- LSP；
- StructuredOutput、Workflow；
- 动态 MCP、ListMcpResources、ReadMcpResource/Dir；
- 平台/入口专属 PowerShell、SendUserFile。

先纵向打通六个 anchor：

1. **Glob**：搜索、排序、100 文件/字符截断；
2. **ToolSearch**：defer、`select:`、动态 MCP 刷新；
3. **ExitPlanMode**：状态转换和真实用户批准；
4. **WebFetch**：schema、URL/domain 权限、重定向/认证错误；
5. **Agent**：spawn 权限、本地/cloud/team/mailbox 分支；
6. **Bash**：前台、timeout、后台、取消，以及与 Monitor 的生命周期边界。

其他工具在 Plan 48 至少完成注册条件盘点；完整行为留给对应子计划。未知项不得猜。

## kloop 基线

从现有代码登记，不因“已有测试”自动判 parity：

- `rust/crates/core/src/tools/mod.rs`：`all_tool_defs`、`builtin_defs`、
  `is_concurrency_safe`、`dispatch_tools`、`run_one`；
- `rust/crates/core/src/permissions.rs`：`Permissions::check_call`；
- `rust/crates/core/src/tools/bash.rs` 与 `background_tasks.rs`：两类后台生命周期；
- `rust/crates/core/src/tools/task.rs`：同步/后台子 agent；
- 各具体工具 parser/executor 与现有测试。

## 后续编号计划

| Plan | 行为簇 | 主要范围 | 依赖 |
|---|---|---|---|
| 49 | 文件与搜索 | Read/Write/Edit/Glob/Grep；stale-read、路径、分页、截断 | 48 |
| 50 | Bash 前台 | schema、parser、timeout、cancel、sandbox、permission、输出 | 48 |
| 51 | 后台与 Monitor | 自动后台化、通知、stall、进程树、清理 | 50 |
| 52 | Agent/Task/Team | Agent、Task*、SendMessage、回灌、并发上限 | 48 |
| 53 | 交互与控制 | AskUserQuestion、Plan、Workflow、StructuredOutput | 48 |
| 54 | 发现与扩展 | Skill、ToolSearch、defer、MCP、`call_tool` 兼容层 | 48 |
| 55 | Web/remote | WebFetch、WebSearch、remote 条件、本地 stub | 54 |
| 56 | Worktree | Enter/ExitWorktree、task isolation、dirty/clean/cleanup | 49、50 |
| 57 | Notebook/LSP | Notebook*、LSP、平台和依赖条件 | 48 |
| 58 | 调度 | Cron、ScheduleWakeup、会话生命周期 | 51、52 |
| 59 | 总体验收 | 全 profile golden、跨工具链、剩余 unknown 闭环 | 49–58 |

每个子计划开工前，先补齐该簇的静态链和 fixture，再决定“对齐、兼容或有意保留”；不得边猜
边实现。

## 明确保留与排除

先保留并单列：

- `run_program`：kloop 专有编排能力；
- `read_offloaded`：kloop 专有超长结果回读；
- `call_tool`：弱模型的 deferred-tool 兼容层；
- Web/Skill 当前自有行为：先标 `intentional-diff` 或 `unknown`；
- 后台 Bash 在 Plan 48 建基线时没有 CC 自动后台化、完成通知回灌、stall 和 Monitor；其中显式后台终态通知、下一 step 回灌与 session 清理由 Plan 51 完成，自动后台化/stall/逐事件 Monitor 仍保留为有据的兼容边界；
- 并行 task 当前没有 CC 的 10 路上限。

Plan 48 不做：

- 不修改 `rust/crates/core/src/tools/*.rs`、权限、TUI/server/native protocol 行为；
- 不实现或重写任何产品工具；
- 不逐字节复刻 UI 文案和 prompt；
- 不研究其他 Claude Code 版本；
- 不根据公开文档补契约；
- 不修改或推送三个参考库；
- 不提交二进制、凭据、真实配置或真实会话；
- 不依赖公网 Web/MCP；
- 不推断未运行平台；
- 不顺手统一命名、注册结构或权限架构；
- 不移除 kloop 专有能力。

## 完成标准

- exact-binary identity guard、matrix schema 校验、fixture hash 校验和敏感信息扫描全绿。
- clean profile 连跑两次，normalized fixture 完全一致。
- 本地 stub 外没有网络访问。
- 所有已知工具进入 registration matrix；六个 anchor 的完整行为链有明确证据。
- 每个 `missing`、`compatible`、`unknown` 都映射到后续计划或明确排除理由。
- kloop-only 工具单列，不计入 CC 缺口。
- `refs/README.md`、`docs/plan/HANDOFF.md` 与 `rust/DESIGN.md` 同步基线结论；在完成前不得
  使用“全工具已对齐”或“可替换 Claude Code”的表述。
- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- `cargo run -p kloop -- --mock`
- `git diff --check`
- 一次提交。

## 完成记录 ✅（2026-07-27）

- 目标身份已固定并由采集器/校验器双重守卫：`2.1.220 (Claude Code)`，指纹只在本机核验；
  二进制未入仓。
- hermetic collector 已落地 6 个完整条件 profile 和 38 组 raw/normalized captures。采集环境从
  白名单构造，使用临时 HOME/config/cwd、本地 fake provider/Web/MCP，禁 telemetry/updater/error
  reporting，公网代理指向不可用 loopback；clean determinism pair 的 normalized bytes 完全相同。
- 六个 anchor 已建立纵向黑盒证据：Glob 的输入/排序/100 上限与截断；ToolSearch 的强制 defer、
  `select:`/关键词/坏输入和 MCP `tools/list_changed` 100→101 刷新；ExitPlanMode 的 outside/headless
  与独立 PTY approve/reject/cancel；WebFetch parser 与 domain-safety 顺序负证据；Agent 本地子请求/
  结果生命周期；Bash 前台、失败、timeout、后台完成通知和默认权限拒绝。
- PTY 驱动固定 30×100 窗口、响应 CPR、保存原始 transcript 与脚本，并在 approve/reject/cancel
  后清理整个进程组。reject 采用分步按键，避免 Ink 状态更新竞态；无遗留 Claude 进程。
- `manifest.json` 登记全部 capture 的命令、条件、raw/normalized hash、return code、signal、规则集
  和 determinism group；normalization 只处理临时根/目标路径/结构化发现的随机 ID、时间、PID 和
  loopback port，不改工具顺序、schema、权限结论、错误文案或 lifecycle。
- `static-evidence.jsonl` 共 46 条固定 locator；`tool-matrix.json` 由 byte-deterministic 的
  `build_matrix.py` 生成 43 行并引用全部 38 fixtures。当前 344 个维度单元为：47
  `compatible`、43 `intentional-diff`、20 `missing`、215 `unknown`、19 `n/a`、0 `same`。
  `run_program`、`read_offloaded`、`call_tool` 作为 kloop-only 单列；所有未决项映射 Plan 49–59。
- WebFetch 的本地成功/redirect/auth/error/large cases 在 2.1.220 domain-safety 层先返回
  `Unable to verify if domain 127.0.0.1 is safe to fetch.`，local Web stub 请求数严格为 0；因此这些
  fixture 只裁决权限顺序，executor/redirect/auth/large-body 仍保守标 `unknown`，没有伪造成功面。
- `verify.py` 已 fail-closed 校验 identity、profile/case/file exact set、hash、raw→normalized 重算、
  determinism、环境/本地网络、PTY 三分支、ToolSearch/Agent/Bash lifecycle、Web 期望、静态 locator、
  matrix 证据门和敏感信息；`python3 -B refs/claude-code-2.1.220/verify.py` 全绿。
- 本轮只新增基线、fixture、matrix、verifier 与文档；没有修改 kloop 工具、权限、TUI 或原生协议
  产品行为。Plan 49–59 尚未执行，本记录不表示“全工具已对齐”或“可替换 Claude Code”。
- 验证：`python3 -B refs/claude-code-2.1.220/collect.py identity`、`collect.py list`、完整 38-case
  `collect.py collect --all`、`verify.py`、`cargo fmt --all --check`、
  `cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace`、
  `cargo run -p kloop -- --mock`、`git diff --check` 全绿。
- 提交：本次（plan 48，见 git log）。
