# Plan 61 — 文件工具纠偏：有界 I/O、CRLF Edit 与安全易用的 Write

> 状态：✅ 已完成（2026-08-04；提交 SHA 以本条所在提交为准）
>
> 母计划：Plan 49
>
> 依赖：Plan 49
>
> 调研基线：kloop `9515f90`；滚动 Claude Code、codex、claw-code、CodeWhale 只作设计参考，Claude Code 2.1.220 的兼容结论仍只由 Plan 48/49 的固定 bundle 与 fixture 裁决。

## 背景

Plan 49 已完成 Read、Write、Edit、Glob、Grep 的工具簇闭环，并建立了比参考实现更严格的文件身份、完整读取资格和原子提交边界。后续独立复核发现三个不应留到 Plan 59 验收阶段再处理的 corrective gap：

1. `read_file_tool` 在 7k 模型输出预算生效前会 `read_to_end` 整个文件，并把全部行收集进 `Vec<&str>`；`read_regular_target` 与 approval preview 也会先完整读取再判断是否过大。输出有界不等于 I/O 和临时内存有界。
2. Read 向模型展示 CRLF 文件时去掉行尾 `\r`，Edit 却在原始 CRLF 文本上执行 `matches`/`replace`。模型从 Read 输出构造的 LF 多行 `old_string` 因而无法命中；approval preview 还单独复制了一套同样的 raw-match 逻辑。
3. `write_file` 的模型可见 description 声称会创建缺失父目录，实际实现只允许 leaf 缺失。后者强化了 descriptor-bound 提交边界，却让常见的“一次 Write 新建嵌套文件”退化为先调用 Bash/Mkdir 的多步操作；这里既有契约错误，也有明确的易用性缺口。

这三项共同落在 Read → observation → preview → Write/Edit 的同一安全边界，放在一个 Plan 49 corrective 中一次闭环。Write 的纠偏不在“删掉承诺”和“直接复制 `create_dir_all`”之间二选一，而是在保留 descriptor/no-follow 不变量的前提下兑现新建文件的递归父目录创建。Plan 49 保持已完成历史；Plan 59 仍只负责总体验收，不接管实现修复。

## 独立裁决与优先级

实施顺序不沿用问题被提出的顺序：

1. **P1：有界原始 I/O**。当前单次或同轮并发 Read 可对超大/稀疏文件产生无界内存与 CPU 消耗；preview 和 mutation snapshot 也有同类路径。
2. **P2：CRLF Edit 正确性**。它失败闭合，不会误写文件，但会让 Windows/CRLF 仓库的核心多行 Edit 无法使用。
3. **P3：安全递归创建父目录与 Write 契约**。风险低于无界 I/O，但直接影响模型完成常见新文件任务的步数；必须把易用性建立在 descriptor-relative 创建上，而不是 pathname `create_dir_all`。

Plan 60 首推的 provider stream guard 仍是独立候选，不并入本计划；Plan 50–59 的工具 parity 路线也不在此扩展。

## 必须保留的不变量

- existing Write/Edit 继续要求完整且 fresh 的 session observation；unread、partial、stale、delete/recreate 一律 fail closed。
- 只有最终成功并实际进入模型可见 `tool_result` 的 Read/Write/Edit 才提交 observation；preview、错误、拒绝、取消和 post-hook interruption 不授权。
- 保留 canonical/no-follow descriptor binding、多硬链接策略、同路径 keyed lock、existing-parent identity 复核、same-directory exclusive temp、sync、最终 stale check、`renameat`、parent sync 和失败清理。
- existing Write/Edit 仍由已打开的直接 parent capability 提交；新建 Write 允许父目录链缺失，但必须从审批前绑定的最近已存在祖先目录 handle 开始逐层创建并立即打开下一层，不得调用 pathname `create_dir_all`。Unix 后端使用 `mkdirat` + `openat(O_DIRECTORY | O_NOFOLLOW)`；Windows 后端必须使用等价的 handle-relative native API，并拒绝 reparse point。
- Edit 目标仍必须已经存在；自动建目录只服务于新建 Write。symlink、FIFO 或其他非普通 leaf 策略不放宽。
- `write_file` 是模型显式提供的全内容替换，行尾按输入原样落盘；Edit 的 CRLF fallback 不得影响 Write。
- Read 的 offset/limit、尾空行、7k 字符预算、图片结构化 block、PDF/non-UTF-8 明确失败等现有模型可见契约，除本计划明示项外不漂移。

## 范围

### 范围内

- 为模型 `read_file`、需要完整内容的 `edit_file` 和 approval preview 增加原始字节读取边界。
- 把 mutation 的版本验证、bounded content read 与 chunked content verification 分开，消除不必要的整文件副本。
- 建立 executor/preview 共用的 exact-first、CRLF-aware Edit helper。
- 为新建 `write_file` 增加审批后的安全递归父目录创建，并把 permission、preview、descriptor/handle walk、失败清理和 durability 纳入同一提交边界。
- 把 mutation parent capability 明确拆成 Unix 与 Windows 后端；Windows 不得继续使用当前 `#[cfg(not(unix))]` 的 pathname join/open/rename 降级路径，并增加 Windows 编译与运行回归。
- 修正 Read/Write/Edit 的模型可见 description，并增加定义回归。
- 补资源边界、CRLF/mixed-EOL、preview/executor 一致性和 Plan 49 安全不变量回归。
- 实现完成后同步 README、capability report、HANDOFF、Plan 61 完成记录和必要的 kloop evidence locator。

### 范围外

- 任意大文件 streaming、chunked UTF-8 decoder、增量分页索引、增量 total-lines/coverage 或文件内容缓存。
- 支持大于本计划上限的 Edit；Notebook/LSP 编码与修改组合仍归 Plan 57。
- 改变 FileState 生命周期、complete-read authority、stale policy 或跨 session/worktree 状态继承。
- 为 Edit 创建缺失目标、提供通用 `mkdir` 工具，或放宽 pathname/FD、symlink、hardlink、atomic commit 边界。
- 直接调用 `create_dir_all`、审批前产生目录副作用，或把自动建目录扩到 workspace 外未获准目标。
- 在 Windows 不提供稳定 file ID、handle-relative rename 或 reparse-point 检查的文件系统上降级为 pathname mutation；这类目标必须明确 fail closed。
- 修改 Glob/Grep、工具并发分类、provider payload 限制、通用 offload 或权限层序。
- 重采或改写 Claude Code 2.1.220 capture；不能为降低 unknown/diff 数量而改变 matrix 语义。
- provider stream guard、context no-follow、rollout durability、protocol `seq` 等 Plan 60 候选。

## 固定设计

### 1. 原始 I/O 边界

- `read_file` 和需要完整文本的 `edit_file` 使用 **5 MiB raw-byte ceiling**，与现有 `MAX_IMAGE_BYTES` 对齐。这样不缩窄合法图片，并给文本留下远大于 7k 模型输出的读取余量。
- approval preview 保留现有 **1 MiB** whole-file diff 阈值，但必须在完整读取和分配之前检查。
- 每个 bounded read 都执行：
  1. 已打开 descriptor 的 metadata length 预检；
  2. 最多读取 `cap + 1` 字节，防止 metadata 检查后文件增长；
  3. 读取后再做 metadata/identity/version 复核。
- 超限 Read 返回明确、可操作的错误，且不产生 `FileStateUpdate::Observe`。
- 新建 Write 或 replacement content 不因 5 MiB Read/Edit ceiling 被一刀切限制。Write 的输入本来已在内存中；旧目标、临时文件和提交后文件改用流式 fingerprint/compare 验证，避免再分配整文件副本。
- Edit 必须拿到完整 UTF-8 当前内容才能安全替换，因此目标超过 5 MiB 时在创建 temp 或提交前拒绝。

### 2. Read 行窗口

`read_file_tool` 继续在最多 5 MiB 的 UTF-8 buffer 上工作，但不再 `split('\n').collect::<Vec<_>>()`：

- 用迭代扫描统计 total lines，并只渲染请求窗口；
- 维持 `split('\n')` 的尾空行语义；
- 维持 offset=0、缺省/0 limit、past-EOF warning、7k 字符截断和 `observed_end` coverage；
- CRLF 仍只在模型展示层去掉与 `\n` 相邻的 `\r`，孤立 `\r` 不吞掉。

这只是移除额外行索引分配，不把本计划扩成任意大文件流式分页。

### 3. Mutation 版本与内容验证分层

重构 `rust/crates/core/src/tools/fs.rs` 中当前由 `read_regular_target` 混合承担的职责：

- descriptor-bound metadata/identity 检查；
- chunked SHA-256，构造或比较 `FileVersion`；
- 仅 Edit 需要的 5 MiB bounded bytes；
- 已知 expected bytes 的 chunked equality verification。

`rust/crates/core/src/file_state.rs::FileVersion` 增加从预计算 fingerprint + metadata 构造/比较的 crate-private 窄接口，继续使用现有 len/mtime/ctime/readonly/dev/inode/mode/SHA-256 版本定义，不改变 coverage。

具体结果：

- expected observation 缺失或 metadata 已漂移时，在读取完整内容前拒绝；
- Write 的旧目标 freshness 可用 descriptor-bound metadata + chunked fingerprint 验证，不需要旧文件 `Vec<u8>`；
- Edit 在版本通过后才做 bounded UTF-8 content read；
- rename 前 target、temp-name binding 和 rename 后 committed bytes 使用 chunked fingerprint/equality，保留现有故障注入与原子提交语义；
- successful Write 仍可直接从模型已提供的 replacement bytes 建立完整 observation。

### 4. CRLF-aware Edit：raw exact 优先

新增纯 helper `rust/crates/core/src/text_edit.rs`，由 executor 与 `diff.rs` 共用。固定规则如下：

1. **raw exact match 优先**：当前原始文本能直接命中 `old_string` 时，完全维持现有 count、unique、`replace_all` 和 raw `new_string` 语义。
2. 只有 raw match 数为 0 时才进入 newline fallback。fallback 只把 `\r\n` 视为逻辑 `\n`；孤立 `\r` 是普通内容。
3. 在逻辑 LF 视图中重新计算 0/1/多次匹配；不得让 fallback 绕过 duplicate protection。
4. helper 维护逻辑位置到原始 byte range 的映射，只重建命中区域；未修改 raw 区域必须逐字节保留，不能全文件 normalize 后整体写回。
5. fallback replacement 先把 `new_string` 的 CRLF/LF 规范为逻辑换行，再恢复该 occurrence 的局部行尾：
   - 优先使用匹配区域内遇到的原始行尾；
   - 匹配区域没有换行时，使用文件 dominant EOL；CRLF 数严格多于裸 LF 才选 CRLF，平局取 LF；
   - 文件没有行尾时使用 LF。
6. `replace_all` 对每个 occurrence 独立重建，未命中区域保持原样。
7. helper 返回结构化 outcome（更新文本、匹配数、是否使用 fallback），由 executor 生成现有 canonical result/error，preview 则用同一个更新结果生成真实 whole-file diff。

`write_file` 不调用该 helper。对 CRLF existing file 执行模型提供的 LF full content，最终仍落 LF。

### 5. Approval preview 有界且与执行一致

修改 `rust/crates/core/src/diff.rs`：

- existing Write preview 先 metadata-check 1 MiB；超限直接返回现有 overwrite summary，再用 `cap + 1` 防增长竞态。
- Edit preview 同样 bounded read；超限、不可读、非 UTF-8或真实 Edit 会失败时，继续退化为 old/new 两字符串 diff。
- 可读取且可执行的 Edit 必须调用 `text_edit` helper；不再单独实现 `matches`/`replace`。
- preview 读取仍不建立 FileState authority；本计划不把 preview pathname read 升格为提交安全边界。

### 6. 新建 Write 的安全递归父目录创建

自动建目录只作用于 preflight 时目标 leaf 不存在的新建 Write；existing Write 与 Edit 的 direct-parent capability 路径不变。公共流程与平台后端如下：

1. preflight 对请求路径做现有 workspace/path normalization，并向上找到最近已存在祖先。对该祖先执行 canonicalize、regular-directory 检查和 no-follow handle open；由 canonical ancestor + 尚缺失的 lexical suffix 构造 effective target，permission/sensitive 规则继续同时检查 original spelling 与 effective target。
2. permission request 与 new-file preview 明确展示最终目标及计划创建的父目录链。deny、取消、pre-hook failure 和审批前错误不得创建任何目录。
3. 审批等待期间保留 ancestor handle。批准后进入 effective-target keyed lock，先复核已存在祖先的 pathname identity，再把“逐段创建/打开、leaf 检查、temp、rename、cleanup”全部交给同一平台 capability backend；中途不能重新从未绑定的绝对 pathname 开始遍历。
4. **Unix**：每段用 `mkdirat` 创建，再用 `openat(O_DIRECTORY | O_NOFOLLOW)` 打开；`EEXIST` 只在同一 no-follow open 证明它是目录后接受。新目录使用 `0o777` 经进程 umask，identity 使用 dev/inode。最终 leaf 继续用 `openat`、same-directory exclusive temp、`renameat` 与 `unlinkat`。
5. **Windows**：不能复用当前 `#[cfg(not(unix))]` 的 `parent_path.join(...)` fallback。已存在祖先用 `CreateFileW(OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)` 获得 directory handle，并查询 attributes/tag 拒绝任何 reparse point。每个单独 path component 用用户态 `NtCreateFile`，令 `OBJECT_ATTRIBUTES.RootDirectory` 指向 retained parent handle，使用 `FILE_OPEN_IF` + `FILE_DIRECTORY_FILE` + `FILE_OPEN_REPARSE_POINT`，从 `IO_STATUS_BLOCK.Information` 区分本调用新建与已存在，再验证 directory type、reparse tag 和 handle identity。identity 使用 volume serial + file ID；无法取得稳定 ID 时 fail closed。
6. Windows 最终 leaf/temp 同样必须相对 retained parent handle 打开；提交使用 `NtSetInformationFile(FileRenameInformationEx)`，以 `FILE_RENAME_INFO.RootDirectory` 绑定目标 parent，不能调用 pathname `std::fs::rename` fallback。临时文件和目录 handle 以明确 share/delete flags 打开，兼容 Windows sharing violation 并保持 leaf-appeared race fail closed。
7. 记录本调用实际创建的目录及其 parent/child handle identity。竞争者已经创建且安全打开的目录可复用，但不计入 cleanup 集合。Windows 后续失败时按逆序释放 retained child handle，再相对 retained parent 以 DELETE access 重开同名 cleanup candidate；只有 stable identity 仍匹配且目录为空时，才对新打开的匹配 handle 设置 disposition。因此不删除 pre-existing、竞争者创建、已被替换或已有内容的目录。POSIX 没有 portable atomic handle-bound `rmdir`，inode check 后再 `unlinkat(parent,name)` 会留下同 UID name-swap 窗口，因此 Unix 失败路径保守遗留本调用新建的空目录，不执行不安全的自动目录删除。cleanup/retention 不覆盖主错误，mutation authority 始终清除。
8. Unix 新目录 durability 继续以 final parent 和承载新目录项的 parent FD 自底向上 sync。Windows 必须在 rename 前 `FlushFileBuffers` 写入文件 handle；目录/namespace flush 没有可移植的 POSIX 等价保证，后端应执行目标文件系统支持的 best-effort flush，并在 README 明确只承诺 handle-relative atomic visibility，不虚构断电后的目录项 durability。
9. Windows 新目录继承 parent ACL/attributes，不套用 Unix mode/umask。两端到达最终 parent 后都复用 leaf absent check、最终 stale check 和 committed-content verification；leaf 在 preflight 后出现时绝不退化成未读 overwrite。只有最终成功并进入模型可见 `tool_result` 后才提交 Write observation。

这实现与 Claude Code 相同的一次调用易用性，但不复制其 pathname `recursive mkdir`、失败遗留目录和 atomic-write 失败后直接覆盖 fallback。Unix 与 Windows 可以共享上层状态机和测试契约，不能共享一组假装可移植的底层 syscall。

### 7. 模型可见工具定义

修改 `rust/crates/core/src/tools/mod.rs::builtin_defs`：

- Read：明确单文件 raw limit 为 5 MiB，并保留 text/image/PDF 行为说明。
- Write：明确新建 leaf 时会在批准后安全创建缺失父目录；existing file 需要完整 fresh Read 后按模型输入整体覆盖。
- Edit：明确 existing file 需要完整 fresh Read，目标最大 5 MiB，并说明 exact replacement/duplicate 行为；不承诺创建缺失目标。
- 不使用 JSON Schema `maxLength` 伪装 byte limit；字符串字符数不等于原始 UTF-8 字节数，且 Edit 结果取决于源内容。
- 用整对象或精确 description 断言锁定定义。

## 实施顺序

### 切片 0：工作树与回归基线

1. 重新读取最新 HANDOFF、Plan 49、Plan 50 状态和 `git status`。
2. 当前已观察到另一会话正在修改 `refs/claude-code-2.1.220/collect.py`。待其收敛后再开工；不得覆盖、暂存、恢复或吸收该改动。
3. 先运行现有 focused fs/diff/file_state tests，确认 corrective 前基线。

### 切片 1：P1 有界 I/O

1. 增加 bounded descriptor read 与 chunked fingerprint/equality helpers。
2. 扩 `FileVersion` 的预计算 fingerprint 接口。
3. 接入 Read，并移除全量行 `Vec`。
4. 拆分 mutation snapshot/version/content 路径，接入 target/temp/committed verification。
5. 接入 preview 的 pre-read threshold 与 bounded fallback。
6. 先跑资源边界与全部 Plan 49 文件安全回归。

### 切片 2：P2 CRLF Edit

1. 先实现纯 `text_edit` helper 与 exact/CRLF/mixed-EOL 单元测试。
2. executor 改用 helper。
3. approval preview 改用同一 helper。
4. 增加 Read → CRLF Edit 与 preview → approve → raw bytes 端到端测试。

### 切片 3：P3 安全父目录创建、工具契约与文档

1. 先抽出 platform capability interface；实现 nearest-existing-ancestor 与共用状态机，再分别实现 Unix 的 `*at` backend 和 Windows 的 retained-HANDLE/native-relative backend。Windows FFI 集中在窄模块，不把 raw handle/NTSTATUS 泄漏到工具逻辑。
2. 覆盖 descriptor/handle-relative directory walk、Windows created-directory cleanup、Unix conservative retention、sync/flush 与 race/identity 单元测试；Windows identity 或 reparse 能力不可用时明确 fail closed。
3. 只接入新建 Write 的 preflight → permission preview → approval → keyed lock → commit 路径；existing Write/Edit 迁移到同一 capability backend，但保持既有产品语义。
4. 修正三个模型可见 description，并加定义断言。
5. 更新 README、capability report 和 HANDOFF。
6. 检查 Plan 49 static evidence/source locator 与 generated matrix notes；只更新因 kloop 行为或代码移动而真实变化的 kloop evidence/说明，不改 CC capture，也不把仍存在的 stale、symlink、atomicity 或 lifecycle 差异伪装成 `same`。
7. 全量验证后回填本计划完成记录，一次提交。

## 测试矩阵

### Bounded Read

- 5 MiB 边界成功；5 MiB + 1 普通文件失败。
- 稀疏超限文件在完整分配前失败。
- metadata 预检后增长仍由 `cap + 1` 读取拒绝。
- 超限、读取中漂移、取消和 post-hook cancellation 都不建立 observation。
- 空文件、尾空行、offset/limit、EOF、7k 行内/行间截断、PDF、非 UTF-8、合法/超限图片保持既有结果。

### Mutation I/O

- unread 与 metadata drift 在完整 content read 前拒绝。
- existing Write 用流式 fingerprint 验证旧版本，不复制旧文件 bytes。
- Edit 超 5 MiB 在 temp/rename 前失败，文件不变、authority 清除。
- replacement 大于 5 MiB 的 Write 仍能按显式输入提交，并以 chunked 方式验证；不额外复制 committed file。
- temp replacement、before-rename failure、target swap、committed mismatch 的故障注入继续验证原文件、temp cleanup 和 authority。

### CRLF 与 mixed EOL

- 完整 Read 纯 CRLF 文件后，以 LF 多行 `old_string` Edit 成功，磁盘仍保持对应 CRLF。
- 显式 `\r\n` raw exact input 仍走 exact 分支，不被 fallback 改写。
- missing、single、duplicate、`replace_all=false/true` 全覆盖。
- mixed LF/CRLF 文件只改命中区，未修改 raw bytes 不变。
- 匹配区域内行尾优先；无局部行尾时 dominant CRLF/LF 与 tie→LF 分支均覆盖。
- 孤立 `\r` 不被吞；trailing newline 保持。
- Write 覆盖 CRLF 文件时按模型提供的 LF content 落盘，不继承旧行尾。

### Preview/executor 一致性

- CRLF 与 mixed-EOL preview 使用 helper 的同一 updated text。
- 批准后实际 raw bytes 与 preview 对应。
- 1 MiB 边界内 whole-file diff；1 MiB + 1 在完整读取前退化为 summary/two-string diff。
- preview fallback 不建立 read authority。

### 安全与工具定义回归

- 新建 Write 可一次创建一层/多层缺失父目录；审批 preview 明示目录计划，批准前目录不存在，成功后内容、目录 mode/umask 与 observation 正确。
- deny、取消、pre-hook failure、sensitive effective target 均不创建目录，也不调用 mutation commit。
- 已存在祖先在审批等待中 retarget 时拒绝；缺失段被创建为 symlink、普通文件或 FIFO 时拒绝且不跟随；安全竞争者目录可以 descriptor-bound 方式复用。
- leaf 在审批等待或目录创建期间出现时按 stale/race 拒绝，不覆盖未读文件。
- temp/write/rename/committed verification 故障后，Windows 只逆序删除本调用创建且 identity 匹配的空目录，pre-existing、竞争者目录、非空目录和被替换目录不删除；Unix 明确保守保留本调用创建的空目录，回归证明不进入 check→pathname-rmdir 竞态。成功与 Windows cleanup 的 parent flush/sync 均有覆盖。
- existing Write/Edit 的 stale、delete/recreate、symlink leaf、hardlink、parent retarget、FIFO、alias binding、same-path serialization、atomic cleanup、post-hook cancellation 全部继续通过。
- Unix 覆盖 symlink/FIFO/dev+inode、`mkdirat/openat/renameat/unlinkat` 与 umask；Windows 覆盖 symlink/junction/其他 reparse point、volume+file ID、ACL inheritance、share violation、case-insensitive alias、handle-relative rename 和 leaf replacement。
- Windows 回归必须在 `windows-latest` 原生运行，证明 mutation 路径不再进入 `parent_path.join(...)`/`std::fs::rename` fallback；macOS/Linux focused tests 不能代替该门。
- Read/Write/Edit definitions 精确断言 5 MiB、完整 fresh Read、Write 批准后创建 missing parent directories，以及 Edit 不创建缺失目标。

## 文档与 evidence 闭环

实现完成后更新：

- `rust/DESIGN.md`：raw Read/Edit 上限、Read 的 LF 逻辑展示、Edit exact-first newline fallback、Write 原样整体替换、批准后安全创建缺失父目录，以及 Unix/Windows 在 ACL/mode 与 crash-durability 上的真实边界。
- `docs/capability-report.md`：把 Plan 61 记为 Plan 49 后续 correctness/resource corrective，不改写历史 parity 结论。
- `docs/plan/HANDOFF.md`：记录“模型输出预算不等于 I/O 预算”、CRLF logical-match/raw-preservation 规则与完成状态。
- 本文件：补实际裁决、验证结果、日期、提交号和 `✅`。
- Plan 49 verifier/evidence：若 source locator 或 kloop 行为说明因本计划失效，更新 generator、kloop locator 和对应 matrix note/status；父目录创建趋同本身不能抹掉 stale-read、symlink、atomicity、output/lifecycle 等仍存在的差异，未经 executable pair 不标 `same`。不运行 `collect.py collect --all`，不修改 immutable CC raw/normalized capture。

Plan 49 的完成记录与 Plan 59 均不改写。

## 完成记录（2026-08-04）

- 上限最终裁决：普通文本/图片 `read_file` 与普通 `edit_file` 为 5 MiB；lowercase `.ipynb` Read/Notebook mutation 保留 Plan 57 的 10 MiB；approval whole-file preview 为 1 MiB。模型输出预算与 raw I/O/分配预算在代码和文档中分别表述。
- 新增 `file_io.rs`，把 metadata 预检、`cap+1` bounded read、读后 stable-identity/version 复核、chunked SHA-256 与 chunked equality 收成同一 descriptor/handle seam；Write 显式 replacement 不受 5 MiB 限制。
- 新增 `text_edit.rs`，executor 与 preview 共用 raw-exact-first helper；仅 raw 零命中才折叠严格 CRLF 为 logical LF，duplicate protection 不绕过，未命中 raw bytes/孤立 `\r` 保真，replacement 使用局部或 dominant EOL。
- 新建 Write 的 preflight 只绑定最近既有 ancestor capability 与计划目录；批准后才逐段 materialize。Unix 使用 `mkdirat/openat/renameat`，Windows 使用 retained HANDLE、volume + 128-bit file ID、全 reparse 拒绝、`NtCreateFile(RootDirectory=...)` 与 `NtSetInformationFile(FileRenameInformationEx)`，其他平台明确 unsupported。Windows 失败时释放 retained child、相对 retained parent 重开 cleanup candidate 并复核 identity，只逆序删除本调用创建、identity 仍匹配且为空的目录；Unix 因缺少 portable atomic handle-bound `rmdir` 而保守遗留本次新建空目录，避免误删 name-swap 替换对象。
- `MutationPreviewContext` 只携 planned directories，保持为 Plan 63 可迁移的窄 seam；权限仍同时检查 original spelling 与 frozen effective target，deny/cancel/pre-hook/无 approver 在目录创建前结束。
- README、capability report、HANDOFF、Read/Write/Edit definitions、Plan 49 kloop static locators 与 matrix notes 已同步；Claude Code 2.1.220 raw/normalized immutable capture 未重采、未改写，matrix 状态与 7 个 executable pair 数量不变。
- Darwin 本机验证：五组 focused Plan 61 tests、workspace fmt/clippy/test、mock、matrix check、corpus-only/full verifier 与 `git diff --check` 全绿。
- Windows 11/NTFS 原生验证：workspace all-targets clippy、六组 Plan 61 focused tests、Windows backend 7 项回归与 scheduler 12 项回归全绿；覆盖 relative junction/reparse 拒绝、volume/file ID、case-insensitive alias、临时文件拒绝第二 writer、单字符 `FileRenameInformationEx`、leaf 已存在时 no-replace、retarget binding 与 identity-aware cleanup。
- Windows 全 workspace baseline 为 420 pass / 77 fail；失败集中在当时尚未实现的 Plan 62 Windows shell/hook、既有 search/permission path 展示和 Git worktree 对 verbatim path 的兼容，不属于本计划文件 backend。CI 因此在 Plan 61 完成提交中保持 Windows workspace clippy + focused native gate，workspace 全量测试继续由 macOS/Linux 执行；不得把该 focused 结论外推成 Windows 全产品验收。真实 API 不属于本计划验收。
- 2026-08-05 交叉记录：Plan 62 已实现并把 Windows 全 workspace/mock/corpus 与 shell focused gates 接回 workflow，但尚无原生 Windows runner 实跑；在真实结果关闭上述 77 项基线前，本条历史证据不改写，也不宣称 Plan 61/62 合并后的 Windows 全产品门已绿。

## 验证

```bash
cd kloop
cargo test -p kloop-core file_io::tests
cargo test -p kloop-core text_edit::tests
cargo test -p kloop-core tools::fs::tests
cargo test -p kloop-core diff::tests
cargo test -p kloop-core file_state::tests
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p kloop -- --mock

cd ..
python3 -B refs/claude-code-2.1.220/build_matrix.py --check
python3 -B refs/claude-code-2.1.220/verify.py --corpus-only
python3 -B refs/claude-code-2.1.220/verify.py
git diff --check
```

若新增 helper 的测试模块名与上述 selector 不同，以最终模块路径替换；不能通过删测试或跳过 verifier 缩小覆盖。真实 API 不属于本计划的必要验收；若实现时发现模型可见行为超出 mock/fixture 能确认的范围，再按 `.kloop/env.local` 约定补真 key 验证并如实记录。

Windows 不是 cross-compile 即算通过：扩 `.github/workflows/ci.yml` 到 `windows-latest`，至少原生运行 `cargo test -p kloop-core tools::fs::tests`、`cargo test -p kloop-core file_state::tests`、workspace `cargo clippy --all-targets -- -D warnings` 与 `cargo test --workspace`。Windows 门必须覆盖 NTFS 临时目录中的 handle/reparse/rename/cleanup 行为；若 workspace 其他既有平台缺口阻塞全量门，先单独修复或如实拆出 blocker，不能把 Windows backend 标成完成。

## 完成标准

- Read/Edit/preview 的完整内容读取都在分配前受正确字节边界约束，并有 metadata-after-growth 防线。
- Write/target/temp/committed 验证不再依赖无界整文件 `Vec<u8>`，也不因 Read/Edit ceiling 错误限制显式 Write content。
- executor 与 approval preview 共用唯一 Edit helper；CRLF fallback 不全局转换未修改区域。
- Write 不继承旧文件行尾；新建 Write 在批准后可从绑定的最近已存在祖先 FD/HANDLE 安全创建缺失父目录。Windows 失败时只清理由本调用创建且仍安全可删的目录；Unix 失败时保守遗留本次新建空目录，不执行无法原子绑定 identity 的 pathname `rmdir`。
- Plan 49 的 observation、descriptor/handle、stale、atomicity、permission 与 cancellation 不变量全部回归通过；Windows native CI 证明没有退回 pathname fallback。
- focused/full Rust、fmt、clippy、mock、matrix/verifier 与 diff check 全绿。
- 本 plan、README、capability report、HANDOFF 完成闭环。
- 一次提交，提交信息带 `plan61`；本文件记录实际提交号。
