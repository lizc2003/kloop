# Plan 67 — `read_file` 纯 Rust PDF 分页读取

> 状态：**⛔ 已停（2026-09-17）,不要实施。**用户判「目前不需要支持 PDF」。
> 理由与 2026-09-17 重新调研出的事实见文末「⛔ 停」一节——**那一节改变了本计划的技术前提**,
> 重启时先读它再读上面的设计。
>
> 此前状态：⏸ 已暂停（2026-08-07；接入前先评估 Hayro 对 release 可执行文件体积的影响）
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

- `rust/DESIGN.md`
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

## ⛔ 停(2026-09-17)

用户判「目前不需要支持 PDF」,本计划到此停,一行代码没写,`tools/fs.rs` 的拒绝点原样保留。
`docs/capability-report.md` 第 85 行的触发条件写的就是"真实 PDF 需求",这次的答案是**没有**。

停之前照 `docs/plan/49-file-search-parity.md:185` 重新查了一轮(那条裁决是**条件式**的:
"PDF 只有在精确 fixture 和 kloop canonical provider wire 都可表达时才对齐")。
结论是**两个条件今天都已成立**,而本计划第二节和「非目标」是在它们都不成立的前提下写的。
下面是查清的事实,重启时不必重查。

### 一、本计划选的路线(乙)不再是唯一可行的那条

第二节选纯 Rust 渲染成 PNG、「非目标」里明写"不做 provider document blocks",
那是 2026-08-07 的信息。2026-09-17 复核:

- **Anthropic 官方文档(Handle tool calls)明写** `tool_result` 的 `content` 可以用
  `text` / `image` / **`document`** / `search_result` 四种。也就是说**整份 PDF 字节可以直接放进
  kloop 现有的 `ToolResultContent::Blocks`**,既不需要新造"附带一条 user message"的机制,
  也不需要渲染器。
- 三条 rail 的落地形状都查到了:

  | rail | 形状 |
  |---|---|
  | Anthropic(`provider/src/anthropic.rs:64` `messages_value`,serde 原样序列化) | `{"type":"document","source":{"type":"base64","media_type":"application/pdf","data":…}}` |
  | OpenAI chat/completions(`openai.rs:30` `image_url_part` 的邻居) | `{"type":"file","file":{"filename":…,"file_data":"data:application/pdf;base64,…"}}`,不支持 `detail`;`tool` 角色不能带文件,要像图片一样挪到尾随 user message |
  | OpenAI Responses(`responses.rs:38` `input_image_item` 的邻居) | `{"type":"input_file","filename":…,"file_data":"data:application/pdf;base64,…"}` |

- 规范上限:Anthropic 请求总体 **32 MB**、**每请求 600 页**(上下文窗口不足 1M 时 **100 页**),
  标准 PDF 不能加密;OpenAI **单文件 < 50 MB、一次请求所有文件合计 50 MB**,需 vision 模型。
  两家都是服务端把**每页转成图并把该页抽出的文本一并喂给模型**——
  所以走 document 这条,模型拿到的东西**严格优于**本地渲染出来的纯像素。

于是路线不再是一条而是两条,**重启时要先重选**:

- **(甲) document wire + 切页**:给 `ContentBlock` 加 `Document` 变体,三条 rail 各译一次;
  `pages` 用一个纯 Rust 切页依赖(`lopdf` 一类,开工时确认)把选中页切成一份更小的 PDF。
  保住文本层,不引渲染器,Windows 面不掉。代价:多一个依赖;且要为 OpenAI-compat 那条
  rail(base_url 可任意指,`cli/src/provider_config.rs:43`)定一条裁决——对端不认
  `type:"file"` 时报不支持还是降级。
- **(乙) 本计划原方案**:纯 Rust 渲染 PNG。零 wire 改动,三条 rail 今天就能走,
  但丢掉文本层、每页按图计费,并且**暂停原因(Hayro 对 release 体积的影响)依然没评估**。

2026-09-17 的推荐是**甲**:精确 fixture 只覆盖它(见下);它保住文本层;它不引新的渲染依赖。

### 二、精确 fixture 一直都在,而且判的是整份那条

`refs/claude-code-2.1.220/fixtures/normalized/read-special-contract-1.json`(profile `scripted-allow-cli`):

- `toolu_plan49_read_pdf_default` → tool result 只有一行
  `"PDF file read: <WORKSPACE>/special/sample.pdf (592 bytes)"`,外加一条
  `media_type: "application/pdf"` 的 document block。
- `toolu_plan49_read_pdf_page` → 采集机上没装 pdftoppm,**录到的是那条 "pdftoppm is not installed" 错误**。

即:plan 49 的"精确 fixture"条件,**整份那条满足,分页那条只有一条环境缺失的报错**。

### 三、cc 的 PDF 有两条路,按 `pages` 分岔(别按单条描述设计)

`refs/claude-code/packages/builtin-tools/src/tools/FileReadTool/FileReadTool.ts:902-1022`:

| 入参 | 走哪条 | 送给模型的是什么 |
|---|---|---|
| 给了 `pages` | `extractPDFPages`(`src/utils/pdf.ts`)→ `pdftoppm -jpeg -r 100` → 每张过 `maybeResizeAndDownsampleImageBuffer` | 一组 JPEG image block,挂在一条附加 user message 上(`isMeta`) |
| 没给 `pages` | `getPDFPageCount`(`pdfinfo`)守 10 页 → `readPDF` | 整份 PDF 字节,`{type:"document",…,"media_type":"application/pdf"}`,tool result 本身只有一行 `PDF file read: <path> (<size>)` |

**分页那条是渲染器,不是切页器——cc 从不发一份"只含选中页的小 PDF"。**

常量(`refs/claude-code/src/constants/apiLimits.ts`,行号已复核):`:54` `PDF_TARGET_RAW_SIZE = 20 MB`、
`:59` `API_PDF_MAX_PAGES = 100`、`:65` `PDF_EXTRACT_SIZE_THRESHOLD = 3 MB`、
`:71` `PDF_MAX_EXTRACT_SIZE = 100 MB`、`:77` `PDF_MAX_PAGES_PER_READ = 20`、
`:83` `PDF_AT_MENTION_INLINE_THRESHOLD = 10`;`src/utils/pdfUtils.ts:59-61` 的 `isPDFSupported()`
就一句「模型名里不含 `claude-3-haiku`」——**它依赖的是模型原生读 PDF,不是自己 OCR**。

一个反常值得记:`FileReadTool.ts:967-985` 的 `shouldExtractPages`(体积 > 3 MB 或模型不支持)
跑完 `extractPDFPages` 之后**只打了 telemetry,结果没被用**;模型支持时仍落到 `readPDF` 整份发。
即"3 MB 以上改走渲染"那句注释在这个版本里对第一方**不成立**。照注释抄会抄错。

### 四、kloop 现状与重启时还欠的四件

现状:`rust/crates/core/src/tools/fs.rs:400` 按 `%PDF-` 魔数**或** `.pdf` 扩展名拒绝,
`:406` 的错误信息是 "use a PDF extraction tool or convert selected pages to images first";
守门测试是 `fs.rs:2085` `read_empty_pdf_and_character_budget_are_explicit`(`:2107` 断言那句话)。
**那句"convert selected pages to images first"是 kloop 自己给用户的绕行建议,不是任何参考实现的做法**,
设计新形状时不要拿它当依据。

重启时还欠:

1. **先重选路线**(甲/乙),别默认沿用本计划第二节。
2. 走甲的话:**切页依赖选型**(纯 Rust、能按页子集重写 PDF、许可证可接受);
   走乙的话:**Hayro 对 release 体积的影响仍然没评估**,那是 2026-08-07 暂停的原因。
3. **OpenAI-compat rail 的裁决**:对端不认 `type:"file"` 时的行为,得先问用户。
4. **parity corpus 两行要改,而且是生成物**:`refs/claude-code-2.1.220/tool-matrix.json:121` 和 `:527`
   的 notes 由 `build_matrix.py:404` / `:631` 生成,`verify.py` 有 `verify_matrix_is_generated()`,
   **手改 JSON 会被拦下**,要改生成器再重跑。`:527` 现在的理由原文是 "rather than granting write
   authority over bytes the model did not see exactly or adding a PDF wire"——加了 wire 之后
   这半句就不成立了。验收要把 **`verify.py --corpus-only` 跑绿列进去,而且早跑**(它会
   `subprocess` 拉 cargo,不是秒级);教训 139/140 记着这道门红了将近四周没人发现。
   另外 `static-evidence.jsonl` 的 `kloop-read-tests` 按**行号区间**锚在 `tools/fs.rs`,
   `verify_repo_location`(`verify.py:7466`)只校验区间落在文件内,fs.rs 变长不会红,
   但区间指向的内容会悄悄漂移,改完顺手核一眼。
