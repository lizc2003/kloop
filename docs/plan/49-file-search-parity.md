# Plan 49 — 文件与搜索工具对齐

> 状态：未开工
>
> 母计划：Plan 48
>
> 依赖：Plan 48
>
> 固定目标：Claude Code 2.1.220；版本、大小与 SHA-256 以 `refs/claude-code-2.1.220/manifest.json` 为准。

## 背景

Plan 48 只纵向打通了 Glob。Read、Write、Edit 和 Grep 目前主要停留在注册面，不能据工具同名、相似 schema 或 kloop 已有测试宣称行为一致。

本计划收敛文件与搜索行为簇：Read、Write、Edit、Glob、Grep，以及 stale-read、路径边界、分页、排序和截断。Worktree 生命周期留给 Plan 56，Notebook 留给 Plan 57。

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

kloop 侧优先复用：

- `kloop/crates/core/src/tools/fs.rs`
- `kloop/crates/core/src/tools/search.rs`
- `kloop/crates/core/src/tools/mod.rs`
- `kloop/crates/core/src/permissions.rs`
- 上述文件内现有单元测试和工具注册测试

## 目标

1. 对五个工具逐项走完 registration → schema → parser → executor → permission → concurrency → output → lifecycle。
2. 固定路径解析、相对/绝对路径、目录/文件、缺失路径、敏感路径和 cwd 边界。
3. 固定 Read 的 offset/limit、文本/图片/二进制分支、长行和总输出截断。
4. 固定 Write/Edit 的创建、覆盖、唯一替换、失败原子性、stale-read 和并发修改语义。
5. 固定 Glob/Grep 的排序、过滤、分页、结果上限、字符上限、错误输入和并发分类。
6. 只有同 profile、同输入的 CC fixture 与 kloop golden 成对后，才把对应维度标为 `same`。

## 开工证据闸门

1. 从精确 2.1.220 bundle 补 Read、Write、Edit、Grep 的注册条件、schema、parser、权限和结果适配静态链；每条写入 `static-evidence.jsonl`。
2. 为 Grep 找到真实可见 profile；clean profile 的缺席继续保留为负观察，不转成 `missing`。
3. 扩 `collect.py`、fake provider 和合成 workspace，只在临时根内执行。
4. 每个新 case 同时保存 raw/normalized capture、hash、profile 条件和受限 normalization 规则。
5. 新增或改变的 deterministic profile 连跑两次，normalized bytes 必须一致。
6. 在产品修改前先更新 matrix 的已证实单元格；证据不足的单元格保持 `unknown`。

公开文档、旧逆向源码和参考仓只能帮助设计 probe，不能裁决 2.1.220 契约。

## 实施切片

### 0. 注册与 schema

- 固定五个工具的可见条件、名称/alias、原始 schema 和工具顺序。
- 比较 kloop 注册条件；名称适配和 schema 差异单列，不提前统一命名。

### 1. Read

- 覆盖最小读取、offset/limit 边界、空文件、超长文件、长行、目录、缺失文件、非 UTF-8、图片和 PDF/二进制分支。
- 固定输出行号、分页提示、截断提示和错误 envelope。
- 核验只读权限、敏感路径检查和并发安全分类。

### 2. Write 与 Edit

- 覆盖新建、覆盖、父目录、重复/缺失 old string、replace-all、权限拒绝和取消。
- 用受控并发修改复现 stale-read；分别记录读取状态、审批前后和实际写入点。
- 验证失败不产生半写文件，且权限和 stale-read 顺序有明确证据。

### 3. Glob 与 Grep

- 延用 Plan 48 Glob cases，补权限和并发。
- 为 Grep 补最小输入、缺字段、错类型、无匹配、坏 regex、glob/type/context 边界和大结果集。
- 分别固定排序、匹配格式、分页/截断、过滤解析和 `.gitignore`/隐藏文件策略。

### 4. 产品与回归

- 只实现 fixture 已裁决且适用于 kloop 的差距。
- 安全性或产品价值不同的行为明确标 `intentional-diff`，并增加回归测试。
- 同步 generator、matrix、manifest、evidence 和 verifier，禁止手工改生成物绕过规则。

## 非目标与有意保留

- 不实现 NotebookRead/NotebookEdit 或 LSP。
- 不处理 Worktree 创建、保留和清理。
- 不逐字节复制 UI 文案或 prompt。
- 不因 CC 行为削弱 kloop 敏感路径、权限或原子写入防线。
- 不预先移除 kloop 的排序、`.gitignore` 或路径保护策略；先用证据决定是对齐还是有意差异。
- 不提交目标二进制、真实 HOME/config/session、凭据或用户仓库内容。

## Fixture 与测试

至少覆盖：

- 最小合法输入、缺必填、错类型、空值和边界值；
- 成功、典型失败、权限拒绝和取消；
- stale-read、并发读取/写入、读写同一路径；
- 100+ 文件、长路径、长行、长输出和截断提示；
- CC raw/normalized fixture 与 kloop 整对象 golden 的成对断言。

验证命令：

```bash
python3 -B refs/claude-code-2.1.220/verify.py
cd kloop
cargo test -p kloop-core tools::fs::tests
cargo test -p kloop-core tools::search::tests
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p kloop -- --mock
cd ..
git diff --check
```

具体测试模块名若开工时已变化，以当时代码为准，不通过删测试缩小覆盖。

## 文档同步

完成时同步本 plan、`docs/plan/HANDOFF.md`、`refs/README.md`、`kloop/README.md` 和 `docs/capability-report.md`。同步 matrix 计数，但不改写 Plan 48 当时的历史数字。

## 完成标准

- 当前平台可执行的五个工具八维链均有 fixture 与 kloop 证据。
- 每个旧 `compatible`、`missing`、`unknown` 都已裁决，或以不可运行条件和后续归属明确保留。
- `same` 全部通过成对 golden 门；有意差异有理由和回归。
- verifier、focused tests、workspace 门禁、mock 和 diff check 全绿。
- 一次提交，提交信息带 `plan49`。

## 开工时定 / 问用户

- CC stale-read 与 kloop 安全写入策略冲突时，保留哪些更严格边界。
- Glob/Grep 的排序、`.gitignore`、隐藏文件和截断策略哪些作为有意差异保留。
- Grep 的实际注册 gate 取得证据后，是否需要兼容层或条件化注册。
