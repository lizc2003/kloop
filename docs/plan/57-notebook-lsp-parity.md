# Plan 57 — Notebook 与 LSP 工具对齐

> 状态：✅ 已完成（2026-08-03）
>
> 母计划：Plan 48
>
> 依赖：Plan 49、Plan 56
>
> 固定目标：Claude Code 2.1.220；版本身份以 Plan 48 manifest 为准。

## 背景与产品裁决

Notebook 和 LSP 都受文件类型、项目环境、扩展与外部进程条件影响。本计划只采用精确 Claude Code 2.1.220 bundle locator 与隔离可执行 fixture，不用滚动源码、公开文档或静态字符串替代运行证据。

开工时已拍板：

- Notebook 读取复用现有 `read_file` 的 `.ipynb` internal adapter，不新增独立 model-visible `NotebookRead`；
- 新增 kloop 原生 snake_case `notebook_edit`；精确目标名称仍是 `NotebookEdit`，schema 与行为只实现 exact fixture 已固定的部分；
- LSP 必须同时通过“工具真实注册、正常发现链启动本地 stdio stub、完整 cleanup 可重放”三重证据门才进入产品；门未闭合时保留 `unknown`，不写推测性 client。

所有证据只操作独立临时目录中的合成 notebook。未读取用户 notebook，未安装 Jupyter、插件、编译器或 language server，也未连接远程 kernel/LSP/cloud 服务。

## 最终证据与矩阵

最终基线：

- 210 个 capture，raw/normalized 各 210 份；Plan 57 新增 10 组 deterministic pair；
- 197 条 static evidence；
- 62 行 × 8 维 = 496 cells：125 compatible / 160 intentional-diff / 32 missing / 145 unknown / 24 n/a / 10 same；
- generated executable pair 仍为 4 个；Notebook 的 native report 与静态结构没有被包装成新的 `same`；
- `kloop-plan57-native-report` 由 full 与 corpus-only verifier 真正运行；缺场景、schema 放宽、事件重排、worktree 资格泄漏、preview 缺失、随机 ID normalization 扩大和伪造 LSP cleanup 均有 mutation-negative 门。

对应 matrix 行：

- `notebook-read@clean-cli`：clean profile 没有独立 `NotebookRead`；缺席只证明该 profile，不升级成全局 `missing`；
- `notebook-read-adapter@clean-cli`：CC `Read(.ipynb)` 对应 kloop `read_file(.ipynb)`；
- `notebook-edit@clean-cli`：CC 注册 `NotebookEdit`，kloop 注册 `notebook_edit`；permission 因 kloop 的更强文件资格与提交边界为 `intentional-diff`，其余已执行维度为 `compatible`；
- `lsp@lsp-env-cli`：即使隔离 profile 设置 `ENABLE_LSP_TOOL=1`，两次 capture 的 24 个工具中仍无 LSP；八维继续为 `unknown`。

## Notebook Read 契约

精确 fixture 固定 `.ipynb` 是普通 Read 内的 adapter：

- 顶层 `cells` 必须是对象数组；坏 JSON 使用 notebook 专属错误前缀；
- markdown、code、raw cell 按 cell 顺序呈现 `<cell id="…">`；缺 ID 仅投影为 `cell-{index}`，不回写文件；
- code cell 省略 `<cell_type>code</cell_type>`，markdown/raw 显式带 type；非 Python code 可带 `<language>`；
- stream/execute-result 文本、code output 图片和尾随文本保持块顺序；markdown attachment 不冒充 code output image；
- code-output PNG/JPEG/GIF/WebP 复用 canonical `ToolResultContent` image block；相邻文本块按 exact adapter 规则合并。

kloop 继续从 permission 前绑定的 no-follow regular-file descriptor 读取 bytes。Notebook 输入上限 10 MiB；模型可见文本上限 7,000 字符；最多发 16 张图片、合计解码后不超过 5 MiB。Notebook paging 不呈现伪完整 cell：非 whole-file-compatible `offset`/`limit` 明确拒绝。只有完整、未截断、成功进入最终 `tool_result` 的 cell-aware read 才授予 `notebook_edit` 资格。

## `notebook_edit` 契约（CC：`NotebookEdit`）

注册的 strict schema 为：

- `notebook_path: string` 与 `new_source: string` 必填；
- `cell_id?: string`；
- `cell_type?: "code" | "markdown"`；
- `edit_mode?: "replace" | "insert" | "delete"`，默认 `replace`；
- `additionalProperties: false`。

执行语义：

- `replace` 把 source 写成 JSON string，保留 metadata/未知字段；code cell 同时清 `outputs` 并把 `execution_count` 置 null；
- `insert` 在 `cell_id` 后插入，省略 ID 时插到开头，且必须给 `cell_type`；nbformat 4.5+ 生成 8 位小写十六进制 ID，旧格式不持久化 ID；
- `delete` 只删除目标 cell；fallback `cell-N` 可定位，但不会被补写到未触及 cell；
- serializer 使用一空格缩进、无尾换行，保留顶层与未触及对象的属性顺序和未知字段。

Notebook 模块使用局部 `IndexMap` ordered AST，没有给 workspace 全局启用 `serde_json/preserve_order`。随机 ID normalization 只允许 fixture 明确证明的单个 8-hex ID 及其派生 hash；cell 顺序、metadata、diagnostic、错误文本和 lifecycle 顺序均不可抹平。

## 文件安全、权限与并发

`notebook_edit` 复用 Plan 49/56 的完整文件边界，而不是另写路径写入器：

- 必须使用绝对、精确小写 `.ipynb` 路径；
- 同时要求完整、fresh、notebook-qualified observation；普通 raw Read、`write_file`/`edit_file` 成功替换都不授予或保留该资格；
- validation/parse/target/serialize/commit 失败保持原 bytes 不变，并保守清除旧资格；成功编辑刷新 notebook 资格；
- parent FD、NOFOLLOW、keyed path lock、提交前版本复核、同目录独占 temp、sync、descriptor-relative rename、parent sync 和失败 cleanup 全部复用现有 mutation 内核；
- 同轮两个 kloop `notebook_edit` 串行；exact CC `NotebookEdit` Pre/Post hook fixture 与 kloop native dispatcher report 都覆盖对应偏序；
- `notebook_path` 是 permission 的一等 path key，参与 original/resolved glob、deny/ask、敏感路径、plan mode、AcceptEdits、session remember 与 canonical approval input 重写；
- approval preview 是 cell-aware source diff，并标出 replace/insert/delete 与 cell ID；
- 全链使用 effective cwd/permissions/FileState；进入 worktree 后，主 checkout 的读取资格与 bytes 不会泄漏。

这些边界强于固定目标只按 timestamp/read-state 判断的路径，因此 permission 保留 `intentional-diff`，不为字面一致降低安全性。

## LSP 证据门结果

exact bundle 固定了 `ENABLE_LSP_TOOL`、插件 `.lsp.json` loader 与 manifest `lspServers` locator。隔离的 `lsp-env-cli` profile 显式设置 `ENABLE_LSP_TOOL=1`，但两次确定性运行都只提供 24 个工具，未注册 LSP。证据表明 server 发现依赖 enabled plugin；当前 corpus 没有可权威、hermetic 构造的 enabled-plugin installation/profile。

因此三重门在第一步即未闭合：无法让精确 2.1.220 经正常发现链调用本地 Content-Length stdio stub，也无权取得 request/cancel/shutdown/exit 的可重放 lifecycle。最终裁决：

- 不新增 `core/src/tools/lsp.rs` 或生产 LSP client/manager；
- 不把 env-only 负注册外推为全局 `missing`；
- `lsp@lsp-env-cli` 八维保持带条件向量与失败阶段的 `unknown`；
- verifier 在门未闭合时反向拒绝生产 `lsp.rs`，也拒绝伪造 cleanup 证据。

## Fixture 与测试

Exact deterministic pairs 覆盖：

- rich notebook read、code-output image、raw/markdown/code、missing ID、坏 JSON；
- replace/insert/delete、fallback ID、no-read、missing ID/cell、stale rollback；
- random 8-hex insertion ID 的窄 normalization；
- 同轮 CC `NotebookEdit` seriality；
- `ENABLE_LSP_TOOL=1` 的确定性负注册。

Rust 覆盖：

- ordered parser/serializer、cell/output/image rendering、截断资格、三种编辑与字段保真；
- strict schema/depth registration、全体 kloop-owned tool 名称的 snake_case invariant、完整/fresh/notebook qualification、stale 与失败回滚；
- ordinary mutation 撤销资格、同路径串行；
- permission preview、canonical `notebook_path`、AcceptEdits/Plan/deny/sensitive/outside-cwd；
- worktree FileState 隔离与真实 `dispatch_tools` 事件配对；
- full/corpus verifier 使用固定 selector 执行 native report，并对关键契约做 fail-closed mutation。

## 验证

```bash
python3 -B refs/claude-code-2.1.220/build_matrix.py --check
python3 -B refs/claude-code-2.1.220/verify.py
python3 -B refs/claude-code-2.1.220/verify.py --corpus-only
cd kloop
cargo test -p kloop-core notebook
cargo test -p kloop-core plan57
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p kloop -- --mock
cd ..
git diff --check
```

完成记录：Notebook implementation、exact corpus、LSP 负门、native report、matrix、文档和全量门禁在一次 `feat(plan57)` 提交中闭合。
