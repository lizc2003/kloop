# Plan 15 ✅ — provider 打磨:prompt caching + thinking/reasoning + 三线协议

> 完成(2026-07-10):片 1 = 56efa17(caching + usage 记账,真 key cache_read=14450 全量命中);片 2 = dcefac7(thinking 块,真 key sonnet-5 adaptive 产出空文本+signature 块、回带无 400、resume 正常,openai 轨回归过);片 3 = 6c3adf8(Responses 适配器,代理端点两轮工具任务端到端过;**挂账:加密 reasoning 往返仅 wiremock 锁定,销账需官方 api.openai.com key**)。挂账后续核查(2026-07-10,端点事实存 memory `reference-codex-proxy-endpoint`):当时"chat 转码"推测**不成立**——该代理是 ChatGPT Codex 后端,复杂任务 + effort:high 会产真实 Fernet 密文的 reasoning item,但**只在流式 `output_item.done` 里带全字段**(非流式与 `completed` 聚合体剥掉 id/encrypted_content,代理 quirk,kloop 收割点恰是 output_item.done 不受影响);且四组回放对照(原样/不带/篡改密文/剥密文)第二轮**全部 200**——密文根本没被送去解密,"回放接受"与"缺配对 400"两条契约在该端点都验证不了,挂账维持。测试 223 → 244。追加 6de37c2(用户提议):tools 末尾第三断点——工具集比含日期/git 快照的 system 稳定,重启换 system 时 tools 前缀仍命中。

> 原 scope 是 caching + thinking;开工时用户扩大:`/v1/messages`、`/chat/completions`、`/v1/responses` 三个线协议都要支持(参考 cc / codex / claw-code)。调研结论(spec + 三库对照)沉淀在本文件;协议细节只认 spec(教训 9/11)。分三片,每片独立 commit + 验收。

## 调研拍板(2026-07-10)

- **caching 断点(cc 形态)**:system 末块 1 个(渲染序 tools→system→messages,连 tools 一起缓存)+ 最后一条 message 末块 1 个(每轮自动后挪;旧断点仍是有效读取点,配合 20 块回看窗口)。总 2 个 ≤ 上限 4。cache_control 不进 protocol 类型,在 anthropic 适配器序列化时注入。
- **usage 记账(cc 公式)**:上下文大小 = `input + cache_creation + cache_read (+ output)`;`input_tokens` 只是未缓存残量。openai 轨 `prompt_tokens_details.cached_tokens` → cache_read 并从 input 扣减(claw-code 形态)。
- **thinking(spec 已变)**:sonnet-5 系 `budget_tokens` 已移除(400),`{type:"adaptive"}` 是唯一开态,且**省略 thinking 字段时默认 adaptive 开**——透传保真是正确性问题不是可选功能。`display` 默认 `"omitted"`(thinking 文本空串但块和 signature 在)。回传:同模型原样带回(含 signature),不许改;cc 全量回传不裁剪;cache_control 永不打在 thinking 块上;唯一合规修理是"assistant 末尾 thinking"类。
- **Responses API(codex 形态)**:`store:false` 无状态 + `include:["reasoning.encrypted_content"]`;reasoning item 必须随 function_call 回放(gpt-5 系缺配对会 400);usage 在 `response.completed`,cached 字段 `input_tokens_details.cached_tokens`;完整 item 随 `response.output_item.done` 整块到达(delta 只是 UI 流)。claw-code 无 Responses 适配器(注释是幌子)。
- **用户拍板**:thinking 文本要显示(TUI 灰字折叠 + plain 灰字;不额外请求 summarized,有文本就显示——openai 轨 reasoning_content 和 Responses summary 有真文本)。

## 切片

### 片 1 — Anthropic prompt caching + usage 记账
- `Usage` 加 `cache_read_input_tokens`/`cache_creation_input_tokens`,`total()` 四项之和;锚点数学语义不变(anchor = 完整上下文)。
- anthropic 适配器:system 改块数组末块打 `cache_control:{type:"ephemeral"}`;messages 序列化后最后一条末块打;解析 message_start 里两个 cache 字段。
- openai 适配器:`prompt_tokens_details.cached_tokens` → cache_read,input 扣减。
- 开关:默认开,`AGENT_CACHE=off` 逃生口(排查 cache miss 用)。
- 测试:wiremock 请求体断点位置整对象断言、开关关时无 cache_control、usage 三字段解析、openai cached_tokens 映射。
- 验收:真 key 连续两轮,第二轮 `cache_read > 0`。

### 片 2 — thinking 块(Anthropic)+ reasoning_content(chat/completions)
- protocol:`ContentBlock` 加 `Thinking{thinking, signature}` + `RedactedThinking{data}`(趁 rollout 存量少,教训 7);`StreamEvent` 加 `ThinkingDelta`。
- anthropic:SSE 聚合 thinking_delta/signature_delta/redacted_thinking;历史回传 serde 自动保真;cache 断点跳过 thinking 末块。
- 开关:默认不发 thinking 字段(新模型默认 adaptive);`AGENT_THINKING=off|adaptive|<budget数>`(数字仅老模型 enabled+budget 用,budget 需 < max_tokens)。
- openai:入站 `reasoning_content`/`reasoning` delta → Thinking(signature 空);出站默认剥离(cc 默认 strip;deepseek 回传记为可能性)。
- compact:thinking 当普通块,不进摘要;压缩替换后旧 thinking 自然消失。
- UI:TUI 灰字折叠行、plain 灰字;子 agent 不外流(教训 3 同款)。
- rollout:随 Message 自然落盘/重放;老文件前向兼容测试。
- growth 公式复查(已定):MAX_OUTPUT_TOKENS 维持 8192 不动(sonnet-5 实测 adaptive 下正常);Budget 模式把 max_tokens 抬高 budget 而非夹紧(夹紧在小上限下退化,教训 4),代价是 predictive growth 在该遗留模式下低估 budget 量——接受并记录。
- 验收:真 key sonnet-5 多步任务,历史合法(无 400)、rollout 可 resume、TUI 能看到(或确认 display omitted 下无文本)。

### 片 3 — /v1/responses 适配器
- `Provider::OpenAiResponses{key, base}`,`AGENT_PROVIDER=openai-responses` 选择。
- 请求:`instructions`=system;input items:user/assistant message(input_text/output_text)、ToolUse→function_call(arguments 字符串)、ToolResult→function_call_output、Thinking→reasoning item(summary_text=thinking,encrypted_content=signature);tools 扁平 function 形态;`store:false`、`include:["reasoning.encrypted_content"]`、`stream:true`;非 store 剥 item id(codex 形态)。
- SSE:`response.output_text.delta`→TextDelta、`response.reasoning_summary_text.delta`→ThinkingDelta、`response.output_item.done`→BlockDone(message/function_call/reasoning 对翻)、`response.completed`→Done(usage:input/`input_tokens_details.cached_tokens`→cache_read/output)、`response.failed`→错误(含溢出检测)。
- 测试:wiremock 请求体整对象断言(含 reasoning 往返)、SSE 事件族契约、usage 映射。
- 验收:真实 Responses 端点双向(key/base 问用户)。

## 完成标准

每片 fmt/clippy/test 全绿 + 单独 commit;README 同步(AGENT_CACHE/AGENT_THINKING/openai-responses);全部完成后 HANDOFF 更新 + 本文件补 ✅ 与提交号。
