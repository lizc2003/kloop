# Plan 29 — 图片输入 / 多模态

> 一句话定位:目标模型 sonnet-5(及 OpenAI 视觉模型)本就支持视觉,kloop 却只能纯文
> 本——这是"目标模型有、kloop 没有"的核心能力缺口。活儿 = `ContentBlock` 加 `Image`
> 变体 + **三条 provider 轨**翻译 + rollout 持久化 + 图片入口。和 plan 15 的 thinking 是
> 同类协议扩展(protocol 加块 + 双/三轨翻译 + 前向兼容),按它的模式做。

## 回源结论(三家真读,2026-07-14,file:line 级,教训 11)

**2/3 有**(cc 全量 + codex 全量,形态互补);**claw 完全没有**(68 个 .py 对
image/base64/multimodal 零命中,唯一 `is_image_path` 只是 @-引用的扩展名布尔标记,不进
请求)。

### A. 线协议三形(kloop 三轨各对一形)
- **Anthropic**(cc 主轨,`attachments.ts:1170-1176` `buildImageContentBlocks`):
  `{type:"image", source:{type:"base64", media_type, data}}`——**data 与 media_type 分
  离**;`media_type ∈ 'image/png'|'image/jpeg'|'image/gif'|'image/webp'`
  (`imageResizer.ts:22`)。也支持 `source.type:"url"` 透传。
- **OpenAI Chat**(cc `@ant/openaiConvertMessages.ts:247-255`;codex `chat 轨实现:834-854`):
  `{type:"image_url", image_url:{url:"data:${mime};base64,${data}", detail}}`——**data
  URL 整串**,不拆。
- **OpenAI Responses**(codex `models.rs:842-857` `InputImage`;cc `responsesAdapter.ts:51-55`):
  `{type:"input_image", image_url:"data:...", detail}`。
- **detail 分歧**:Anthropic **无** detail;OpenAI 有 `auto/low/high`(codex 默认
  `High`,把 `Original/low` 归一到 `high`,`chat 轨实现:856-862`)。→ kloop 内部块带
  **可选 detail**,anthropic 轨忽略、openai/responses 轨缺省填 `auto`(或 high)。

### B. 图片来源(两类)
- **user 顶层图**(粘贴/拖拽/CLI 路径):cc `imagePaste.ts`(xclip/wl-paste);codex
  `UserInput::Image`/`LocalImage{path}`→`ContentItem::InputImage`,并用
  `<image name=[Image #N] path="…">…</image>` 文本标签包裹(`models.rs:1567-1589`)。
- **工具读图**:cc 的 MCP 结果可带 image(`mcp/client.ts:451-454`);codex 专门
  `view_image` 工具读文件字节→data URL→`FunctionCallOutput{output:ContentItems([InputImage])}`
  (`view_image.rs:221-236`),模型不支持图时直接拒(`:49-98`)。

### C. 持久化:base64 内联进 rollout(两家一致)
- cc **对 image block 显式跳过 offload**——`toolResultStorage.ts:301-304` 注释
  `"Skip persistence for image content blocks - they need to be sent as-is to Claude"`;
  另把粘贴图落 per-session `image-cache/{id}.{ext}`(`0o600`,base64 编码)做指针
  (`imageStore.ts:33-66`,`MAX_STORED_IMAGE_PATHS=200`)。
- codex rollout JSONL 直接内联 `InputImage{image_url=完整 data URL}`
  (`tests/suite/image_rollout.rs:46,58`);两家都有**"看完即弃/不支持则剥离"**省 token
  机制(codex `replace_last_turn_images`/`strip_images_when_unsupported`
  `history.rs:260-277,410-419`;token 按 base64 长度估算)。

### D. 格式与限额基线
png/jpeg/gif(**仅非动画**,codex `lib.rs:311`)/webp;单图 cc **5MB 按 base64 串长**
(`apiLimits.ts:22`,`imageValidation.ts:65-104`);尺寸 2000–2048px;单请求媒体 **≤100**
(cc `stripExcessMediaItems` 从旧到新裁);**远程 http(s) URL codex 直接拒**、只收 base64
data URL(`image_preparation.rs:104-121`)——SSRF 面天然收窄,kloop 宜同样只收本地读盘+
base64。

### E. tool_result 内嵌 image 的降级(最大分歧,决定片 2 走法)
- Anthropic 轨:**原生**塞进 `tool_result.content`(cc 主轨,`claude.ts:986-990`)。
- OpenAI **Responses** 轨:塞进 `function_call_output.output` 的 `content_items` 数组
  (codex,Responses 独有能力,Chat 没有)。
- OpenAI **Chat** 轨(tool 角色消息**不能带图**)两条先例:
  - **搬运**(codex `chat 轨实现:869-940`,**推荐**):tool 消息只留文本 +
    `"[tool output contains image data attached in the following message]"` 占位,图另建
    一条**紧随的 `role:"user"`** 合成消息(`"Tool output for call_id …:"` + image_url),
    信息不丢。
  - **丢弃**(cc `@ant/openaiConvertMessages.ts:144-151`):tool 消息只取文本、image
    block 映射成空串扔掉。

## 目标

用户能给 kloop 图片(片 1),模型能读代码库里的图片文件(片 2),三条 provider 轨
(anthropic / openai-chat / responses)都正确翻译、rollout 前向兼容。

## kloop 现状与改造点(已读准)

- `ContentBlock`(`protocol/src/lib.rs:20`,Anthropic wire shape)有
  Text/Thinking/RedactedThinking/ToolUse/ToolResult,**无 Image**;canonical 即 Anthropic
  形,加 `Image { source }` 最自然。
- **`ToolResult.content` 是 `String`**(`lib.rs:46`);`openai.rs` 里 user content 是拼
  接字符串(`:88`)、tool 消息 content 也是字符串(`:76-79`)。→ **片 1(user 图)只需
  user content 变数组,不动 ToolResult**;**片 2(工具读图)才需改 `ToolResult.content`
  装 blocks + Chat 轨降级**。
- `read_file_tool` 返回 `Result<String>` + `read_to_string`(`fs.rs:9-13`)——纯文本,
  读图会崩。片 2 决定改它判 MIME 还是单开 `view_image`。
- 三轨翻译点:`anthropic.rs`(序列化 `Block` enum,加 `Image` 分支)、`openai.rs`
  (user content 数组化 + 片 2 的 tool 降级)、`responses.rs`(`input_image`)。
- 持久化:`History` offload cap 8000 针对 `ToolResult.content` String——**image block
  不走 offload**(照 cc),base64 内联 rollout。

## 建议切片

- **片 1(user 顶层图,最小闭环、无降级)**:`ContentBlock::Image{source:{media_type,
  data, detail?}}` + 三轨翻译(anthropic image / openai image_url / responses
  input_image)+ **图片入口**(决定见下)+ 格式&大小校验(png/jpeg/gif/webp、只收本地读
  盘 base64、单图上限、远程 URL 拒)+ rollout 内联持久化(image 跳过 offload)。user 顶
  层图三轨都原生保留,**不动 `ToolResult`**。交付"用户给图、模型看图"。
- **片 2(工具读图,碰 `ToolResult` + Chat 降级)**:`read_file` 判 MIME 读图 或 新
  `view_image` 工具 → tool_result 带 image;`ToolResult.content` `String` → 装 blocks;
  三轨:anthropic 原生内嵌、responses 塞 content_items、**openai-chat 走"搬运"降级**
  (codex 式,不丢)。模型不支持图能力位检测(可选)。

## 关键决定(开工时定 / 问用户)

1. **图片入口**(片 1,最小选一个):CLI `--image <path>`(可重复,像 plan 18 `--fork`
   起步最省)vs TUI 粘贴(需 crossterm 剪贴板,plan 25 未开 mouse capture,较重)vs 消息
   里 `@path` 引用。倾向 CLI `--image` 起步,TUI 粘贴挂账。
2. **`ToolResult.content` 改造形态**(片 2):`Vec<ContentBlock>`(全面,所有工具返
   String 包 Text)vs `enum {Text(String), Blocks(Vec)}`(渐进,多数工具仍 Text)。倾向
   后者(改动面小、序列化可保持旧 `content:String` 前向兼容)。
3. **read_file 判 MIME vs 新 view_image 工具**(片 2):codex 单开 view_image 更干净
   (read_file 保持返 String)。倾向单开。
4. **Chat 轨 tool_result 降级**:搬运 vs 丢弃。**倾向搬运**(codex,信息不丢);但
   kloop 若片 2 只用 view_image(模型主动读图),丢弃也可接受——开工定。
5. **detail 字段**:片 1 是否暴露。倾向内部块带 `Option<detail>`、CLI 不暴露,openai/
   responses 轨缺省 `auto`,anthropic 轨忽略。

## 不做(挂账,记为后续可能性)

client 端 resize/降采样(cc/codex 都做,kloop 首版直传、超限报错让用户自理);
image-cache 落盘指针(cc 有,首版内联 rollout);"看完即弃/不支持则剥离"省 token 机制
(两家都有,等 rollout 体积有痛感再补);PDF/document 块;远程 URL 图片(只收本地 base64,
SSRF 面天然收窄);粘贴/拖拽入口(片 1 若选 CLI);单请求媒体数上限裁剪(≤100,首版靠
用户自律)。

## 测试

协议往返(`Image` 块 serde);三轨翻译契约(anthropic `source.base64`、openai-chat
`image_url.url=data:` 拼串、responses `input_image`——各断言请求形态,复用 plan 13 的
`mock_recording`);格式校验(非 png/jpeg/gif/webp 报错、超大报错、远程 URL 拒);rollout
前向兼容(image 内联往返、旧无 image 的 rollout 照读);片 2:`ToolResult` 装 blocks 的
序列化旧 `content:String` 前向兼容、Chat 轨"搬运"降级(tool 留占位 + 紧随 user 消息带
图)、view_image 读真图返 base64。

## 完成标准

fmt/clippy/test 全绿,一次 commit;真 key 双轨验收(至少 anthropic 轨:给 sonnet-5 一张
截图让它描述/据图改代码;openai 轨给视觉模型同任务);README 同步图片用法与三轨/限额;
本文件补完成记录(提交号 + 挂账);HANDOFF 补教训(尤其 `ToolResult` String→blocks 改造
的前向兼容、三轨 detail 归一、Chat 轨 tool_result 降级选型)。

## 完成记录(片 1,提交 `ed088d0`)

**范围**:只做片 1(用户顶层图,不碰 `ToolResult`)。片 2(工具读图 + Chat 轨降级 +
`ToolResult` String→blocks)整体挂账留下一会话。

**决定落地**:
1. 入口:CLI `--image <path>`(可重复),读盘 + 校验后附到**首个 user 消息**(`plain`
   与 TUI worker 各持 `pending_images`,首个真 user turn `std::mem::take` 一次;`--mock`/
   `--serve` 忽略并告警——server 由 client 经 RPC 给图)。远程 URL(http/https 前缀)入口
   直接拒(只收本地读盘 base64,SSRF 面天然窄,随 codex)。
2. `ToolResult` 未动(片 1 无降级需求)。
3. read_file 未动(片 2 再定 view_image vs 判 MIME)。
4. Chat 轨降级:片 1 无(user 顶层图三轨都原生保留)。
5. **detail 字段:片 1 不进 protocol**,openai/responses 翻译时发常量 `detail:"auto"`
   (对齐 OpenAI 默认;anthropic 无此字段)。理由:片 1 CLI 不暴露 detail,`Option<detail>`
   会是恒 None 死字段;发 `"auto"` 常量已满足三轨契约。**偏离 plan 倾向("内部块带
   Option<detail>")**——未来真有来源(view_image / CLI 暴露)时给 `Image` 块加字段(内容
   块字段前向兼容,serde default),openai/responses 只替换那个常量。记挂账。

**改造点**(与 plan 15 thinking 同模式:protocol 加块 + 双/三轨翻译 + 前向兼容):
- `protocol`:`ContentBlock::Image { source: ImageSource }` + `ImageSource::Base64
  { media_type, data }`(tag 形,留 `url` 变体余地);`Message::user_with_blocks(text,
  blocks)`(text 前、附件后;空 text = 纯图消息)。canonical = anthropic wire,序列化即
  `{type:"image", source:{type:"base64", media_type, data}}`。
- `core/src/image.rs`(新,纯函数,片 2 view_image 可复用):`detect_media_type`(magic
  bytes 嗅探,**不信扩展名**:png/jpeg/gif/webp;WEBP=RIFF+偏移 8 WEBP;短 buffer 不
  panic)、`MAX_IMAGE_BYTES=5MiB`、`image_block_from_bytes`(校验格式+大小→base64→块)。
  依赖:新引 `base64`(第 3 个功能依赖——标准编码,自写不如引 crate,已在依赖树)。
- 三轨:**anthropic 零改动**(`serde_json::to_value(messages)` 生 serialize,Image 天然对
  齐;加 unit test 锁契约,cache 断点可落在 image 块——非 thinking);**openai-chat** user
  content 有图时变 parts 数组(text part + `image_url` data URL + `detail:"auto"`),无图仍
  纯 string(旧 wire 逐字节不变);**responses** user content 数组加 `input_image`
  (`image_url` data URL + `detail:"auto"`)。两 adapter 的 Assistant 分支加 Image 忽略分支
  (assistant 不产顶层图)。
- 持久化:**image 块天然不走 offload**(`History::record` 的 offload 只判 `ToolResult`),
  base64 内联进 rollout;token 记账走既有 `estimate_message_tokens`(按 serialized base64
  长度,plan D 同法)。
- TUI:`cells_from_history` 给 resume 的 user 图加 `[image: {media_type}]` 占位行(base64
  不打印);**实时 `--image` 首个 turn 的转录占位挂账**(UI loop 的 Cell::User 不感知
  pending_images;模型回应已确认收图)。

**真 key 验收(双轨过)**:测试图 PNG 画秘密词 `KLOOP-VISION-7F3Q`(模型不可能猜)。
- **anthropic 轨**(claude-sonnet-5):`--plain --image` → 模型逐字读出秘密词 + 第二行。
- **openai-chat 轨**(自建代理 + gpt-5.4-mini):同任务、同样读出秘密词。
- **rollout 内联**:两轨 session 文件均含 image 块(base64 3348 字符、`offloaded=false`)。
- **resume 重放**:`--plain --resume <id>` 后追问"图里第二行是什么",模型答 `sonnet sees
  this`——证明 rollout 内联的 image 块 resume 时正确重送。
- **responses 轨真 key 挂账**:env.local 的 OPENAI_BASE_URL 是 openai-compat 代理(chat/
  completions),非 Responses 端点;responses 轨的 `input_image` 有单测契约覆盖,真 key 待
  官方 Responses 端点(与 plan 15 responses 轨挂账同因)。

**挂账(片 1 之外)**:片 2(工具读图:view_image / MCP 图结果 + `ToolResult` String→
blocks + Chat 轨"搬运"降级 + 模型视觉能力位检测);client 端 resize/降采样;image-cache 落
盘指针;"看完即弃/不支持则剥离"省 token;PDF/document 块;远程 URL 图;单请求媒体数上限裁
剪(≤100);TUI 粘贴/拖拽入口 + 实时 turn 转录图占位;暴露 `detail`。

## 完成记录(片 2,提交 `0464725`)

**范围**:工具读图 + `ToolResult` String→blocks + 三轨(anthropic 原生 / responses 原生 /
openai-chat 搬运降级)。至此 plan 29 主体完成。

**决定落地(开工时逐点与用户敲定,连续三点"参考 cc")**:
1. **入口 = `read_file` 判 MIME 一把梭,不单开工具**(参考 cc `FileReadTool`,回源真读
   `packages/builtin-tools/.../FileReadTool.ts:650-666`:一个 Read 工具读文本/图/PDF/
   notebook,读图返 `tool_result.content:[{type:image,source:base64}]`)。**纠正 plan 备
   忘**:回源结论原只记了"cc 的 MCP 结果可带 image",漏了 cc 的 FileReadTool 本身判 MIME
   读图——两家在"单个 Read 判 MIME(cc)vs 单开 view_image(codex)"上就是分歧,用户选
   cc。工具名 `read_image` 讨论作废(不单开)。cc 的 resize/降采样不抄(plan「不做」),超
   5 MiB 报错;非图非 UTF-8 二进制干净报错。
2. **`ToolResult.content`:`String` → `enum ToolResultContent { Text(String),
   Blocks(Vec<ContentBlock>) }`,`#[serde(untagged)]`**(cc 坐实:cc 的
   `tool_result.content` 就是 `string | array<block>` 二态,读文本用 string、读图用 array;
   见 `@ant/.../openaiConvertMessages.ts:convertToolResult`)。untagged 序列化:Text→裸字
   符串(旧 rollout `content:"…"` **前向兼容白送**)、Blocks→块数组(对齐 Anthropic
   `tool_result.content` 的 string|array)。便利 API:`From<String>`/`From<&str>`(构造点
   `.into()` 零成本兼容)、`as_text()->Cow`(Text 原样;Blocks join 文本 + `[image:
   <mt>]` 标签,给 offload/hook/codemode/降级占位等纯文本面)。
3. **openai-chat 轨降级 = 搬运**(codex 式,非 cc 的丢弃)。**破例不随 cc**:cc 的 Chat
   是边缘轨(cc 主轨 Anthropic),丢弃可接受;但 openai-chat 对 kloop 是正经副轨,丢弃 =
   该轨读图直接废。搬运保信息(回源真读 codex `chat 轨实现:869-940` +
   `pending_multimodal_tool_outputs` 排序 `:401,537`):tool 消息留占位
   `[tool output contains image data attached in the following message]`,图另建**紧随的**
   `user` 消息(`Tool output for call_id X:` + `image_url` data URL)。用户追问"大模型支持
   吗"——答:OpenAI Chat 允许 tool 消息后接 user 消息、user 的 image_url 是标准视觉输入,
   唯一硬约束是本轮所有 tool 消息须紧跟 assistant tool_calls(图 user 消息延后)。**kloop
   利好**:一轮 tool_result 本就聚在一条 `Message::tool_results`,`to_openai_messages` 循环
   内 tool 消息先 push、图攒到末尾随 user 消息 push,天然满足排序,不用 codex 那样跨
   Message 缓冲。
4. **detail = 常量 `"auto"`**(复用片 1,read_image 的 image_url 与片 1 user 图同款 data
   URL)。

**改造点(自底向上,与片 1 同模式:protocol 加/改块 + 三轨翻译 + 前向兼容)**:
- `protocol`:`ToolResultContent` enum(untagged)+ `ContentBlock::ToolResult.content` 换
  类型 + 便利方法;测试锁 string|array 双态往返 + 旧 `content:"…"`→Text 前向兼容 +
  `as_text` 图标签。
- `core/tools/fs.rs`:`read_file_tool` 返 `Result<ToolResultContent>`——读**字节**(非
  read_to_string)→ `detect_media_type` 嗅 magic(复用片 1 `image.rs`)→ 图走
  `image_block_from_bytes`(校验格式 + 5 MiB)返 `Blocks`;非图 `from_utf8` 失败即报错,成
  功走行号 offset/limit 返 `Text`。工具描述加"可读图"。
- `core/tools/mod.rs`:`execute_tool` 返 `Result<ToolResultContent>`——`read_file` 单独早返
  (唯一能返非文本的工具),其余工具仍返 `String` 统一 `.map(Text)` 包装(改动面最小);
  `run_one` 的 post_tool hook 与 tool_result 构造用 `as_text()`/`.into()`;`interrupted`/
  测试 `run_tool` 走 `as_text` 扁平化(既有调用零改)。
- `core/history.rs`:offload 只 spill `Text` 变体(**图块天然不 offload**,base64 内联
  rollout,照 cc `toolResultStorage.ts` "skip persistence for image content");
  `estimate_message_tokens` **零改动**(serde 序列化数字节,base64 长度自动计入,plan D 白
  送)。
- `core/tools/codemode.rs`:program 拿工具结果用 `as_text()`(program 编排不能"看"图,图降
  级成 `[image: …]` 文本;built-in `read_file` 的 TS 声明仍 `Promise<string>` 准确)。
- 三轨:**anthropic 零改动**(serde 自动,`tool_result.content:[{type:image,…}]`,加契约单
  测);**responses** `function_call_output.output` = string|array,Blocks 塞
  `input_text`/`input_image` content_items(codex `FunctionCallOutputBody` 同为 untagged
  Text/ContentItems,回源坐实);**openai-chat** 搬运(占位 + 紧随 user 消息 + 排序保证)。
- **TUI/server 零改动**:cells_from_history 本就跳过 tool_result 内容(history-internal,注
  释在案),read_file 读图的 tool 行照常 ✓;实时/resume 都不特殊处理图 tool_result。

**真 key 验收(双轨过)**:测试 PNG 画秘密词 `MULBERRY-Q92`(900×240,避免片 1 首图右缘裁
切的坑)。
- **anthropic 轨**(sonnet-5):`read_file` 读图(仅 read_file、无 bash/python)→ 逐字读出
  `MULBERRY-Q92`;二行小字追问答 `kloop slice 2 vision check`(resume 未重读文件、直接从内
  联 image 块答——证明 rollout 内联 + resume 重放正确)。
- **openai-chat 轨**(gpt-5.4-mini):`read_file` 读图 → 经**搬运降级**看到图 → 转写
  `SECRET: MULBERRY-Q92`。(首次用"secret token"措辞触发模型误拒,与 kloop 无关,换中立措
  辞即读出——记:验收 prompt 别用易触发安全拒答的词。)
- **rollout 内联**:session 文件 tool_result 内 `image` 块 base64 20104 字符、`offloaded=
  false`(> 8000 cap 但因是 Blocks 变体不 spill)。
- **responses 轨挂账**:同片 1/plan 15,env.local 的 OPENAI_BASE_URL 是 chat/completions
  代理非 Responses 端点;responses 轨 `function_call_output` 带 `input_image` 有单测契约,真
  key 待官方 Responses 端点。

**挂账(片 2 之外)**:模型视觉能力位检测(codex
有,kloop 目标模型都支持图,不做);client 端 resize/降采样;image-cache 落盘指针;"看完即
弃/不支持则剥离"省 token;PDF/document 块;远程 URL 图;单请求媒体数上限(≤100);TUI 粘贴/
拖拽 + 实时 turn 转录图占位;暴露 `detail`。

## 完成记录(片 2 续:MCP 工具图结果,提交 `5efcff9`)

**范围**:承接片 2 挂账里的"MCP 工具图结果"——MCP 工具返回的 image 内容块现在被抬成模型能
看的图,不再降级成 `[image: …]` 文本标签。地基(`ToolResultContent::Blocks` + 三轨翻译)片
2 已铺,本次只补 MCP 结果的 image 提取 + 接线。

**回源(cc 真读)**:cc 的 `MCPTool.mapToolResultToToolResultBlockParam`(`MCPTool.ts:70`)
把 MCP `content` 数组**原样**塞进 `tool_result.content`(`string|array` 二态);MCP 图内容形
是 spec 的 `{type:"image", data:<base64>, mimeType}`(kloop 的 `render_content` 早已按
`mimeType` 读)。cc 不降级,image 块透传给模型。

**改造点**:
- `kloop-mcp`:`render_content` 拆出 per-item `render_item`(文本降级复用);新
  `content_blocks(content)->Option<Vec<ContentBlock>>`——**只有含可用图时**才走 Blocks 路
  (text-only 返 None,文本路径字节不变),把 MCP `{type:image,data,mimeType}` 抬成 canonical
  `ContentBlock::Image{Base64{media_type:mimeType,data}}`,图周围的非图项折成 `Text` 块保序;
  **unsupported mimeType(非 png/jpeg/gif/webp)不抬**(坏块会整请求失败,降级成 `[image:…]`
  文本更稳)。`isError:true` 在 `call_tool_structured` 已 Err 化,图只走 success 路。
- `SourceOutput` 加 `blocks: Option<Vec<ContentBlock>>` + `into_content()`(有 blocks→Blocks,
  否则 Text);`execute_tool` 把 source 分支像 read_file 一样**上提早返**(能返 Text 或 Blocks),
  原 `other=>find_source` 收敛成"未知工具"。program 面照旧拿 `structured`(CallToolResult),
  模型面拿 Blocks。cli `McpToolSource::call` 填 `content_blocks(&structured["content"])`。
- **三轨全复用片 2**:anthropic 原生内嵌 / responses 原生 content_items / openai-chat 搬运——
  MCP 图与 read_file 图走完全相同的 `ToolResultContent::Blocks` 下游,零新增翻译。

**真 key 验收**:stub MCP server(Python,stdio JSON-RPC)`badge__get_badge` 返回
`[{text},{image:png}]`,图画秘密词 `TANGERINE-K7`(只在 MCP 图里)。
- **openai-chat 轨**(gpt-5.4-mini):调 `badge__get_badge` → MCP 图经 `content_blocks` 抬成
  Blocks → **搬运降级**看到图 → 读出 `BADGE: TANGERINE-K7`。
- **rollout 内联**:session 文件 tool_result = canonical `[Text("Here is the badge image."),
  Image(png,base64 10348)]`(`content_blocks` 的输出原样),未 offload。
- **anthropic 轨挂账(仅本次)**:代理端 `claude-sonnet-5` 持续 429("No available channel",
  代理容量问题非 kloop);anthropic **原生内嵌**路径 = `content_blocks`(单测)→
  `ToolResultContent::Blocks` → serde 自动,与片 2 今日已实机验过的 read_file anthropic 内嵌
  **同一下游** + 新增 serde 契约单测覆盖,代码路径已验,仅缺一次 MCP-on-anthropic 的实机跑通。

**测试**:`content_blocks`(text-only→None、unsupported mime→None、图抬成 `[Text,Image]` 保序);
core stub source 返图 → dispatch 出 `Blocks` tool_result;`SourceOutput::into_content`;既有
defer/collision/counts 测试随 stub 加一个 image 工具同步。

**挂账**:模型视觉能力位检测;client resize;PDF 块;远程 URL 图;媒体数上限;TUI 粘贴入口;
暴露 detail(同片 2)。MCP 图结果本身**无挂账**(除 anthropic 实机跑一次待代理恢复)。
