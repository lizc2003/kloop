# Plan 157 — 一条还没有人需要的 PDF wire ⛔ 未启动

> **2026-09-17 停,不要实施。**用户一句「目前不需要支持 PDF」。没有写代码,
> `tools/fs.rs` 的拒绝点原样保留,parity corpus 一个字没动。
>
> 本文件只保留**开工前已经查清的事实**——下次重启这条时不必重查,直接从第五节接。
> 停的理由不是"做不到":第二节查下来,plan 49 挂的两个前置条件今天**都已成立**。
> 拦住它的是第三个从来没写进裁决里的条件:**有没有人真的要读 PDF**。

## 一、这条是从哪儿冒出来的

- `docs/capability-report.md:85`(工具面差距表,PDF 读入行):收敛"双家",补齐路径
  "plan 49 明确返回 unsupported;不在 canonical provider wire 未统一时伪兼容",
  触发条件"真实 PDF 需求"。
- `docs/capability-report.md:437` 第四节路线图 T0 行:「`read_file` PDF 原生分页读取仍待实现」。
- `docs/plan/49-file-search-parity.md:185` 的原文是**条件式**的裁决,不是结论式的:
  > PDF 只有在精确 fixture 和 kloop canonical provider wire 都可表达时才对齐;
  > 否则返回明确 unsupported error、增加 paired 回归并标 `intentional-diff`。

  两个条件,逐条验——见第二节。下游那句"canonical provider wire 未统一"是**转述**,
  不是 plan 49 说的话。

## 二、查清的事实(2026-09-17,逐条带出处)

### 2.1 kloop 现状:拒绝点和守门测试

- `rust/crates/core/src/tools/fs.rs:400` —— 判据是魔数 `%PDF-` **或**扩展名 `.pdf`,
  `:406` `bail!("read_file: PDF files are not supported; use a PDF extraction tool or
  convert selected pages to images first")`。
- 守门测试 `rust/crates/core/src/tools/fs.rs:2085`
  `read_empty_pdf_and_character_budget_are_explicit`,`:2107` 断言错误里有
  "PDF files are not supported"。
- **那句错误信息里的"convert selected pages to images first"是 kloop 自己给用户的绕行建议,
  不是任何参考实现的做法**,设计新形状时不要拿它当依据。

### 2.2 cc 的 PDF 有**两条**路,按有没有 `pages` 参数分岔

`refs/claude-code/packages/builtin-tools/src/tools/FileReadTool/FileReadTool.ts:902-1022`:

| 入参 | 走哪条 | 送给模型的是什么 |
|---|---|---|
| 给了 `pages` | `extractPDFPages`(`src/utils/pdf.ts`)→ `pdftoppm -jpeg -r 100` → 每张过 `maybeResizeAndDownsampleImageBuffer` | **一组 JPEG image block**,挂在一条附加 user message 上(`isMeta`) |
| 没给 `pages` | `getPDFPageCount`(`pdfinfo`)守 10 页 → `readPDF` | **整份 PDF 字节**,`{type:"document",source:{type:"base64",media_type:"application/pdf",data}}`,同样挂在附加 user message 上;tool result 本身只有一行 `PDF file read: <path> (<size>)` |

**分页那条是渲染器,不是切页器——cc 从不发一份"只含选中页的小 PDF"。**
(本任务的交接把这两条合并描述成"切页 + 原样发 PDF 字节",那条路在 cc 里不存在。教训 143 的又一例。)

常量核对(`refs/claude-code/src/constants/apiLimits.ts`,行号已复核):

- `:54` `PDF_TARGET_RAW_SIZE = 20 MB`(整份路的硬上限,注释算的是 32 MB 请求上限 ÷ base64 的 4/3)
- `:59` `API_PDF_MAX_PAGES = 100`
- `:65` `PDF_EXTRACT_SIZE_THRESHOLD = 3 MB`
- `:71` `PDF_MAX_EXTRACT_SIZE = 100 MB`
- `:77` `PDF_MAX_PAGES_PER_READ = 20`
- `:83` `PDF_AT_MENTION_INLINE_THRESHOLD = 10`
- `src/utils/pdfUtils.ts:59-61` `isPDFSupported()` 就一句「模型名里不含 `claude-3-haiku`」——
  **它依赖的是模型原生读 PDF,不是自己 OCR**。

一个小反常值得记:`FileReadTool.ts:967-985` 的 `shouldExtractPages`(体积 > 3 MB 或模型不支持)
跑完 `extractPDFPages` 之后**只打了 telemetry,结果没有被用**;模型支持时仍然落到 `readPDF`
整份发。即"3 MB 以上改走渲染"这条注释在这个版本里**对第一方是不成立的**。照抄注释会抄错。

### 2.3 精确 fixture 已经存在,而且判的是**整份**那条

`refs/claude-code-2.1.220/fixtures/normalized/read-special-contract-1.json`(profile `scripted-allow-cli`):

- `toolu_plan49_read_pdf_default` → tool result `"PDF file read: <WORKSPACE>/special/sample.pdf (592 bytes)"`,
  外加一条 `media_type: "application/pdf"` 的 document block。
- `toolu_plan49_read_pdf_page` → 采集机上没装 pdftoppm,**录到的是那条"pdftoppm is not installed"错误**。

**结论:plan 49 的"精确 fixture"条件,整份那条满足,分页那条只有一条环境缺失的报错。**

### 2.4 canonical wire 能表达,而且**不必造附加 user message**

Anthropic 官方文档(Handle tool calls)原话:

> `content` (optional): … These content blocks can use the `text`, `image`, `document`,
> or `search_result` types.

**document block 可以直接放进 `tool_result.content`。**cc 把它挂在外面是 cc 自己的结构选择,
不是 API 约束——也就是说 kloop 现有的 `ToolResultContent::Blocks` 这条通道**够用**,
不需要为工具新造一条"附带 user message"的机制。这是当初写下 plan 49:185 时没有的信息。

三条 rail 的落地形状(都查了官方文档):

| rail | 形状 | 备注 |
|---|---|---|
| Anthropic(`provider/src/anthropic.rs:64` `messages_value`) | `{"type":"document","source":{"type":"base64","media_type":"application/pdf","data":…}}` | 该函数是 serde 原样序列化,给 `ContentBlock` 加一个 `Document` 变体即自动成型 |
| OpenAI chat/completions(`openai.rs:30` `image_url_part` 的邻居) | `{"type":"file","file":{"filename":…,"file_data":"data:application/pdf;base64,…"}}` | 不支持 `detail`;`tool` 角色**不能**带文件,要像图片一样挪到尾随 user message(`IMAGE_RELOCATED_PLACEHOLDER` 那套) |
| OpenAI Responses(`responses.rs:38` `input_image_item` 的邻居) | `{"type":"input_file","filename":…,"file_data":"data:application/pdf;base64,…"}` | |

规范上限:

- Anthropic:请求总体 **32 MB**;**每请求 600 页**(上下文窗口不足 1M 时 **100 页**);
  标准 PDF、不能加密。服务端把**每页转成图,并把该页抽出的文本一起喂**——
  所以整份路拿到的东西严格优于本地渲染。
- OpenAI:**单文件 < 50 MB,一次请求所有文件合计 50 MB**;需要 vision 能力的模型;
  PDF 解析同样是"文本 + 页图"一起进上下文。

## 三、两条路线(停的时候摆在桌上的就是这两条)

**(甲) document wire + 切页。**给 `ContentBlock` 加 `Document` 变体,三条 rail 各译一次;
`pages` 用一个纯 Rust 切页依赖(`lopdf` 一类,开工时确认可行)把选中页切成一份更小的 PDF。
保住文本层,不引渲染器,Windows 面不掉。代价:多一个依赖,且要为 OpenAI-compat 那条
rail(base_url 可任意指,`cli/src/provider_config.rs:43`)定一条裁决——对端不认
`type:"file"` 时是报不支持还是降级。

**(乙) 渲染成图。**复用现有 `ContentBlock::Image` 和 plan 156 的像素降采样
(`core/src/image.rs`),三条 rail 今天就能走,零 wire 改动。代价:丢掉可选中文本、
每页按图计费、要引 PDF 渲染(macOS 上 CoreGraphics `CGPDFDocument` 可以不引第三方,
**但那样 PDF 读就成了 macOS-only**,而 kloop 有 Windows 面,plan 62)。

**当时的推荐是甲**,理由三条:精确 fixture 只覆盖它;它保住文本层;它不引平台绑定的渲染器。

## 四、非目标(重启时也仍然是)

- 不做本地 OCR / 文本抽取。参考实现没有一家这么干,模型端原生读 PDF 是这条能力的前提。
- 不改 `read_file` 的 5 MiB 原始读上限(plan 61 的裁决),PDF 的体积门是**另一个**数。
- 不动 `@` 提及那条路(cc 的 `PDF_AT_MENTION_INLINE_THRESHOLD` 属于它自己的输入面)。

## 五、真要做时,还欠的四件

1. **切页依赖选型**(走甲的话):纯 Rust、能按页子集重写 PDF、许可证可接受。
2. **OpenAI-compat rail 的裁决**:对端不认 `type:"file"` 时的行为,得先问用户。
3. **parity corpus 两行要改**,而且是**生成物**——`refs/claude-code-2.1.220/tool-matrix.json:121`
   和 `:527` 的 notes 由 `build_matrix.py:404` / `:631` 生成,`verify.py` 有
   `verify_matrix_is_generated()`,**手改 JSON 会被拦下**,要改生成器再重跑。
   `:527` 现在的理由原文是 "rather than granting write authority over bytes the model did
   not see exactly or adding a PDF wire"——加了 wire 之后这半句就不成立了。
4. **验收必须把 `verify.py --corpus-only` 跑绿列进去,而且早跑**(它会 `subprocess` 拉 cargo,
   不是秒级):教训 139/140 记着这道门红了将近四周没人发现。另外
   `static-evidence.jsonl` 的 `kloop-read-tests` 按**行号区间**锚在 `tools/fs.rs`,
   `verify_repo_location`(`verify.py:7466`)只校验区间落在文件内,fs.rs 变长不会红——
   但区间指向的内容会悄悄漂移,改完顺手核一眼。

## ⛔ 停(2026-09-17)

用户:「目前不需要支持 PDF」。`capability-report.md` 第 85 行的触发条件写的就是
"真实 PDF 需求",这次的答案是**没有**,所以按原条件停,不是推翻 plan 49——
plan 49 的两个技术前置条件今天反而都成立了(2.3、2.4),这一点记在第 85 行上,
省得下次又从"wire 表达不了"这个已经过期的前提重新推一遍。
