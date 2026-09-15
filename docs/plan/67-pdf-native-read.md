# Plan 67 — `read_file` 纯 Rust PDF 分页读取

> 状态：⏸ 已暂停（2026-08-07；接入前先评估 Hayro 对 release 可执行文件体积的影响）
>
> 依赖：Plan 29（canonical image blocks 与三条 provider rail）、Plan 49 / Plan 61（descriptor-bound Read 与 staged file observation）、Plan 57（多块媒体结果预算）

## 背景

路线图的下一个 T0 是 `read_file` PDF 原生分页读取，完成后再进入 session-scoped Task V2。

当前 `read_file` 已用 no-follow regular-file descriptor 和 `file_io::read_bounded` 得到 5 MiB 内的稳定快照，也已能把普通图片作为 canonical image blocks 送入历史、rollout 和三条 provider rail；PDF 仍明确报 unsupported。

本切片将 PDF 在 kloop 内以纯 Rust 渲染为有序 PNG 页图。已确认不使用 provider 原生 document block，不调用 `pdfinfo`、`pdftoppm`、系统动态库或外部服务。目标是让同一结果跨 provider 可见，同时守住文件绑定、输出预算、失败原子性和 mutation qualification 边界。

## 已确认契约

### `read_file` 参数与结果

- 新增可选字符串 `pages`，仅接受 1-based 的 `"N"` 或 inclusive `"N-M"`；不接受空白、前导零、0、倒序、列表、小数或溢出值。
- 显式范围最多 20 页；任一页越界即整次报错，不裁剪、不返回已有页。
- 未传 `pages`：PDF 总页数 `<= 10` 时按原顺序渲染整份；总页数 `> 10` 时不渲染，并定向提示传 `pages`。
- PDF 只使用 `pages`；只要显式传了 `offset` 或 `limit`（包括 0）就报错。非 PDF 传 `pages` 也报错；文本、图片、notebook 的既有 `offset` / `limit` 语义不变。
- schema 增加 `pages` 和 `additionalProperties: false`；executor 仍在打开文件前做同构 runtime 校验。`offset` / `limit` 继续复用现有 `integer_arg` 兼容规则，不改变字符串整数、0 或错误文案行为。
- 成功固定返回 `ToolResultContent::Blocks([Image, ...])`：每页一个 `image/png`，只含图片，block 顺序就是页序。
- 不插页码文本、不提取文字、不做 OCR，也不新增 PDF / document protocol type。
- 所有页全部成功后才构造结果；任一页失败时不返回半页。

### mutation qualification

PDF 页图是转换后的展示，显式 `pages` 还可能只覆盖源文件的一部分，不能授予 Write / Edit 的完整读取资格。

- 成功结果 staged 一个 `FileStateUpdate::Clear`，只在最终 tool result 通过 post-hook 后提交。
- 失败、取消、permission 拒绝或 post-hook 阻断不建立或刷新 observation。
- 后续若要用页级 coverage 授权 mutation，另做 provenance-aware 设计；本计划不扩 `FileObservation`。

## 实施

### 1. 参数校验与文件分流

修改 `rust/crates/core/src/tools/mod.rs` 与 `rust/crates/core/src/tools/fs.rs`：

1. 在公开 ToolDef 和 `PreparedRead` 中加入 `pages`，保持 schema 与 runtime parser 同构。
2. `prepare_read` 在任何文件打开前拒绝未知字段、错误类型和非法分页语法。
3. 继续以 `%PDF-` magic 或大小写不敏感 `.pdf` 扩展分流；沿用同一 descriptor 的 metadata 预检、`cap + 1` 增长哨兵和读后稳定性复核。
4. 在得到稳定 raw snapshot 后调用 PDF 适配层；PDF 成功 staged `Clear`，其他格式的 observation 语义不变。

### 2. 纯 Rust 渲染适配层

新增 `rust/crates/core/src/pdf.rs` 并在 `rust/crates/core/src/lib.rs` 注册。该模块是唯一接触 Hayro 的窄边界；`fs.rs` 只负责稳定快照、格式分流、参数关系和 staged file-state update。

- 精确依赖 `hayro = "=0.7.1"`，保留默认嵌入字体 / CMap feature，不直接依赖 `hayro-syntax`。
- 使用公开的 `Pdf::new`、页集合、`render_dimensions`、`render` 与 `Pixmap::into_png`；同一文档复用一个 `RenderCache`。
- PDF 解析与逐页渲染整体放进一个 `spawn_blocking` closure。
- 用进程级 semaphore 将并发渲染限制为 2 份 PDF；等待 slot 的 future 可取消，permit 移入 blocking closure，已开始的 Hayro 工作即使调用方取消也占用 slot 到自然退出。
- 页底色固定为不透明白色；目标倍率最多 2x（约 144 DPI），再按原比例缩到长边 `<= 2048 px`、单页 `<= 4,000,000` 像素。
- 尺寸必须 finite、positive；所有尺寸换算、乘法与累计先 checked 后分配。
- 逐页暂存 PNG，全部页通过后才调用既有 `image_block_from_bytes` 做 PNG magic、单图上限和 canonical base64 封装。
- 可用窄 `catch_unwind` 把 renderer panic 归一化为稳定 tool error；不透出依赖内部错误串。

固定资源预算：

| 边界 | 上限 |
|---|---:|
| 原始 PDF | 5 MiB |
| 解析后总页数 | 10,000 |
| 自动读取页数 | 10 |
| 显式选择页数 | 20 |
| 页图长边 | 2,048 px |
| 单页像素 | 4,000,000 |
| 本次累计像素 | 40,000,000 |
| 单页 PNG decoded bytes | 2 MiB |
| 本次 PNG decoded bytes 合计 | 5 MiB |
| 同时渲染的 PDF | 2 |

加密 / 密码保护、损坏或不支持、页范围、无效 MediaBox、总页数、单页 / 累计像素、单页 / 累计 PNG 和 renderer panic 分别使用稳定、可断言的 tool errors。

### 3. 多页图片的预测性上下文估算

修改 `rust/crates/core/src/history.rs`。当前 `estimate_message_tokens` 按完整 JSON 字节数 `/4`，会把结构化图片的 base64 当文本 token；多页 PDF 可能在第一次模型可见前被错误判为超过默认 context。

- 改为按 block 估算：非图片内容继续沿用 serialized-byte heuristic；图片忽略 base64 payload 长度，按命名常量保守计 `6,000` tokens / 张，并计入结构元数据。
- 这只是 provider-neutral overflow 预测上界；真实 provider usage 返回后仍由既有 usage anchor 接管。
- 同时覆盖既有普通图片和 notebook 图片，不改变 wire、history 或 rollout 内容。

### 4. Provider 接线

现有三条 rail 应直接复用，不新增 PDF / provider 分支，只补双页整对象契约测试：

- `anthropic.rs`：同一 tool result 的两个 image blocks 原生保序序列化。
- `openai.rs`：tool role 保留 placeholder；同一 call 的两张图片按页序出现在 trailing user message 的两个 `image_url` parts 中，且仍位于全部 tool messages 之后。
- `responses.rs`：同一 `function_call_output.output` 数组按页序生成两个 `input_image`。
- 若测试不暴露缺陷，不改三条 adapter；`ToolResultContent::as_text()` 仍只供 hook、UI 和 Code Mode 降级，不能进入模型页图路径。

### 5. 依赖、MSRV 与 CI

- 在 workspace 和 core manifest 加 Hayro，并更新 `Cargo.lock`；测试侧复用 workspace 已有 `png` crate，不引入第二套图片库。
- Hayro 0.7.1 的 MSRV 是 Rust 1.92：在 `[workspace.package]` 声明 `rust-version = "1.92"`，所有 workspace member 用 `rust-version.workspace = true` 继承；edition 仍为 2021，不新增 `rust-toolchain`。
- `.github/workflows/ci.yml` 保留三平台 stable 主门，新增 Ubuntu Rust 1.92 的 `cargo check --workspace --all-targets` MSRV job。
- 用独立 `CARGO_TARGET_DIR` 记录实施前后 `cargo build -p kloop --timings` 的冷构建差异，并用 `cargo tree -p kloop-core -e features` 审核最终 Hayro feature / dependency graph；只记录结论，不提交 target 或 timing HTML。

## Fixtures 与测试

在 `rust/crates/core/tests/fixtures/pdf/` 加仓库自制、无第三方字体或图片的最小 fixtures，并附 README 说明生成方式：

- 一页 Base-14 文本 PDF；
- 两页红 / 蓝矢量 PDF；
- 损坏 PDF；
- 有效加密 PDF；
- 巨大 MediaBox PDF。

页数边界由测试 helper 生成最小 N 页 PDF，避免提交大量重复 fixture。

定向覆盖：

1. `pdf.rs`：`pages` grammar 正负例、整数溢出、20 页边界、默认 10 / 11 页、显式越界、单页选择、页序、尺寸与全部资源预算、并发 slot、损坏、加密、巨大 MediaBox、第二页失败不泄漏第一页。
2. PNG：只断言 signature、IHDR、尺寸、不透明白底和中心 / 非背景像素；红蓝页断言保序，文本页断言非空白，不做压缩字节 snapshot。
3. `fs.rs`：magic 与大小写 `.pdf` 分流、5 MiB / `+1` raw cap、默认与显式分页、参数互斥、成功只含有序 PNG blocks、成功清除 mutation qualification、所有失败无新 observation，且文本 / 图片 / notebook 不回归。
4. `history.rs`：扩大同一图片 base64 不应线性放大 token estimate；20 张页图加现有 35k round-growth reserve 仍可进入默认 200k 窗口的第一次采样；普通大文本估算保持原行为。
5. rollout / resume：多图不 offload，持久化后数量与顺序不变。
6. provider：三条 rail 各补两页整对象断言。
7. 冻结的 Claude Code 2.1.220 corpus 不改，不伪造纯 Rust renderer 与 Claude Code / Poppler 成功路径的 exact parity。

## 文档与完成门

同步更新：

- `rust/README.md`
- `docs/capability-report.md`
- `docs/plan/HANDOFF.md`
- `refs/README.md`
- 本 Plan 67 的状态、验证记录和提交说明

文档写清 `pages`、资源预算、mutation qualification、三条 rail 复用、Hayro / MSRV，以及 Claude Code 双路行为与 kloop 纯 Rust 路线的有意分歧；完成后把路线图 T0 前移到 Task V2。

验证：

1. focused core PDF / fs / history / rollout 与 provider tests；
2. `cargo +1.92.0 check --workspace --all-targets`；
3. `cargo fmt --all --check`；
4. `cargo clippy --workspace --all-targets -- -D warnings`；
5. `cargo test --workspace`；
6. `cargo run -p kloop -- --mock`；
7. `python3 -B refs/claude-code-2.1.220/verify.py`；
8. `python3 -B refs/claude-code-2.1.220/verify.py --corpus-only`；
9. `git diff --check`；
10. 从仓库根既有 `.kloop/env.local` 加载本地凭据，对两页 fixture 做真实视觉 dogfood；每条已配置的 Messages / Chat / Responses rail 都验证页序与内容，缺少某条官方凭据时明确记录，凭据和响应原文不提交。

完成后一次 `plan67` commit；Plan 文件补 ✅、日期、验证结果与 SHA（以本文件所在提交为准）。

## 非目标与安全边界

- 不做 PDF 文本抽取、OCR、搜索、表格语义、任意非连续页列表、密码输入、解密、修复、编辑、缓存、用户可调 DPI、provider document blocks、外部转换器或专用 viewer。
- 不改变 protocol、rollout 格式或三条 provider rail 的媒体模型。
- `spawn_blocking`、semaphore、尺寸 / 输出预算和 `catch_unwind` 只限制 kloop 可控的并发与结果规模。
- Hayro 仍在主进程解析不可信压缩流，缺少可取消的 CPU / 内存上限，因此不能宣称形成了恶意 PDF 炸弹的隔离边界；若将来需要该承诺，应另做带 OS CPU / 内存限制的隔离 worker。
