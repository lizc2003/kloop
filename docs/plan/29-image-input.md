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
