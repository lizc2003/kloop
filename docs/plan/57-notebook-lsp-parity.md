# Plan 57 — Notebook 与 LSP 工具对齐

> 状态：未开工
>
> 母计划：Plan 48
>
> 依赖：Plan 48
>
> 固定目标：Claude Code 2.1.220；版本身份以 Plan 48 manifest 为准。

## 背景

Notebook 和 LSP 都受文件类型、项目环境、平台、扩展与外部进程条件影响。Plan 48 的 clean profile 只观察到 NotebookEdit；NotebookRead 与 LSP 未出现，不能据此推断不存在。

exact bundle 中的 Notebook stale-read 字符串只能作为后续静态追踪入口，不能代替工具注册、schema 或执行证据。kloop 当前也没有可直接计作 Notebook/LSP parity 的实现与测试。

## 当前证据与差距

对应 matrix 行：

- `notebook-read@clean-cli`
- `notebook-edit@clean-cli`
- `lsp@clean-cli`

当前结论：

- NotebookRead 在 clean profile 未观察到，八个维度均为 `unknown`；静态 stale-read 字符串不是 registration 证据。
- NotebookEdit 在 CC clean fixture 可见并带 schema；kloop 无对应实现，因此 registration/schema 为 `missing`，其余维度未知。
- LSP 在 clean profile 未观察到，kloop 也无对应实现或测试，八个维度均为 `unknown`。
- 当前 fixtures 没有合成 notebook、受控并发修改、LSP server 请求日志或依赖条件向量。

优先复用：

- Plan 48 的 exact-binary collector、fake provider、manifest 与 verifier
- `kloop/crates/core/src/tools/fs.rs` 的路径和文件读取边界
- `kloop/crates/core/src/tools/mod.rs` 的注册、dispatch 与并发分类
- Plan 49 的路径、stale-read、权限和截断成果可在其完成后复用，但不作为本计划开工前提
- 独立临时目录中的合成 `.ipynb` 与本地 stdio LSP stub

## 目标

1. 固定 NotebookRead、NotebookEdit、LSP 的 surface_kind、注册条件、schema 与平台/依赖 gate。
2. 固定 Notebook cell、metadata、output、cell ID 和错误输入的 parser/output 契约。
3. 固定 NotebookEdit 的 stale-read、并发修改、原子写入、失败回滚和文件保真边界。
4. 固定 LSP 的 server 发现、启动、请求、超时、取消、错误、输出和进程清理。
5. 只在本地可控 profile 下裁决行为；无法运行的平台或依赖条件继续保留 `unknown`。
6. 所有 fixture 使用合成文件和本地 stub，不读取用户 notebook，不启动真实项目 language server。

## 开工证据闸门

- 从 exact bundle 分别追 NotebookRead、NotebookEdit、LSP 的构造点、gate、schema、parser、executor 和 result mapping。
- 先找到 NotebookRead/LSP 的真实可见 profile；没有 profile 证据时不得把 clean profile 的缺席改判为 `missing`。
- collector 为每个 case 新建临时目录与合成 `.ipynb`，保存调用前后完整文件 hash 和结构化内容。
- LSP fixture 只连接本地 stdio stub；保存 initialize、request、cancel、shutdown、exit 的严格顺序与进程状态。
- 随机 request ID、临时路径和 PID 只按声明的结构化规则归一化，不归一化 cell 顺序、metadata、diagnostic 或错误文案。
- 为 kloop 缺失 surface 建立明确 negative locator；只有取得 CC 可执行链后才决定实现、adapter 或有意不纳入。

## 实施切片

### 0. surface 与条件 profile

- 固定 clean、notebook 文件存在、项目依赖可用、扩展启用和平台条件下的工具数组。
- 区分 model-visible tool、内部 notebook adapter、LSP client 与前端展示能力。
- 固定 schema 字段、required、additionalProperties、默认值和坏类型错误。

### 1. NotebookRead

- 采集空 notebook、单/多 cell、markdown/code/raw、outputs、attachments、metadata 和大文件。
- 固定 cell ID、cell 顺序、分页/截断、缺失文件、坏 JSON、坏 nbformat 和非 notebook 输入。
- 判断输出是原始 JSON、格式化文本还是结构化 adapter；未取得 fixture 前不预写答案。

### 2. NotebookEdit

- 固定 replace/insert/delete、目标 cell 缺失、cell type、source 和 metadata 保留行为。
- 覆盖 read-before-edit、未读取、读取后外部修改、并发编辑和重复调用。
- 验证失败时原文件不被部分写坏，成功时未触及字段保持结构与顺序。

### 3. LSP

- 固定 server 选择、root/workspace、initialize capability、文档同步和请求映射。
- 覆盖成功、空结果、server error、坏响应、timeout、cancel、崩溃、重启和关闭。
- 固定并发请求、乱序响应、diagnostic/位置归一化和 session 退出后的 process cleanup。
- 当前平台或语言依赖不可 hermetic 满足时，保存不可运行证据并保持 `unknown`。

### 4. 产品与回归

只实现已裁决差距；Notebook 与 LSP 可分别选择独立工具、内部 adapter 或明确不纳入。同步 matrix、fixture、static evidence、manifest、generator 与 verifier。

## 非目标与有意保留

- 不读取、修改或提交用户 notebook、工作区缓存或 language-server 配置。
- 不安装 Jupyter、编辑器扩展、编译器或第三方 language server。
- 不连接远程 LSP、notebook kernel 或云服务。
- 不从 stale-read 字符串、公开文档或其他 Claude Code 版本推断 2.1.220 行为。
- 不把普通 Read/Edit 自动计作 NotebookRead/NotebookEdit parity。
- 不逐字节复制 notebook 或 diagnostic 的 UI 展示。

## Fixture 与测试

至少覆盖：

- NotebookRead 的可见/不可见 profile、合法 notebook、空文件、坏 JSON、坏 nbformat、大文件和特殊 cell；
- NotebookEdit 的 replace/insert/delete、坏 cell ID、未读取、stale-read、并发修改和失败回滚；
- metadata、outputs、attachments、cell ID 与未知字段的保真；
- LSP 的发现/缺失、initialize、成功/空结果、错误、timeout、cancel、崩溃和 shutdown；
- 并发 LSP 请求与乱序响应不串扰；
- fixture 后无临时文件、socket、子进程或用户目录改动。

验证：

```bash
python3 -B refs/claude-code-2.1.220/verify.py
cd kloop
cargo test -p kloop-core notebook
cargo test -p kloop-core lsp
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p kloop -- --mock
cd ..
git diff --check
```

## 文档同步

完成时同步本 plan、HANDOFF、refs/README、kloop README、capability report 与 parity 产物。明确 NotebookRead/LSP 的真实注册条件、kloop 产品范围和仍不可运行的平台分支。

## 完成标准

- 当前平台可运行的 Notebook/LSP 链有 exact fixture 与对应 kloop golden 或明确产品裁决。
- Notebook stale-read、并发修改、保真和失败回滚有确定性测试。
- LSP timeout、cancel、崩溃与 session exit 后无残留进程。
- 未运行 profile 保持有证据理由的 `unknown`，不通过删 row 或静态字符串收敛。
- 所有门禁全绿，一次提交，提交信息带 `plan57`。

## 开工时定 / 问用户

- NotebookRead 与 NotebookEdit 是否进入 kloop model-visible surface，还是复用文件工具 adapter。
- kloop 是否提供通用 LSP 工具，以及首批必须支持的本地语言环境。
- 平台/依赖不可 hermetic 运行时的产品支持声明与验收边界。
