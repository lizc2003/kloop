# Plan 15 — provider 打磨:prompt caching + thinking 块

> 一个会话完成。开工前先读 docs/plan/HANDOFF.md。参考:Anthropic 官方文档(prompt caching、extended thinking)——协议细节只认 spec(教训 9/11);codex/cc 的实现只看接线位置。

## 目标

两件都是 provider 层、都直接影响真实使用:①Anthropic 请求加 `cache_control` 断点,长会话每轮省大头输入费;②thinking 块透传,支持开思考的模型(sonnet-5 系)。OpenAI-compat 的对应形态(reasoning)本 plan 不做,记为可能性。

## 设计要点

- **caching 断点位置**:对照官方最佳实践定(system 尾、tools 尾、历史倒数第二条 user 是常见形态;断点数量有上限)。断点跟着历史长度动态挪 → 注意别把每轮请求变成 cache miss。
- **usage 记账**:`cache_read_input_tokens` / `cache_creation_input_tokens` 进 Usage,锚点数学(`history.note_usage`)要不要把 cache_read 算进"上下文大小"——按语义应该算(它仍占窗口),对照 spec 确认。
- **thinking 的协议面**:protocol `ContentBlock` 加 `Thinking` 变体(serde 前向兼容:老 rollout 读新文件会遇到未知块?——rollout 未知字段忽略只保护信封层,块级要单独想,开工时定方案);thinking 是否落 rollout、compact 时怎么处理(倾向:不进压缩摘要、边界当普通块)。
- **回传要求**:Anthropic 要求后续请求原样带回 thinking 块(含 signature)时才合法——按 spec 处理,历史里必须保真。
- **开关**:thinking 默认关,`AGENT_THINKING`(预算 token 数?)开;caching 默认开(纯省钱无行为变化)还是可关,开工时定。
- **UI**:thinking 流式要不要显示(TUI 折叠行 / plain 灰字),开工时定,最小可以先不显示只透传。
- 双防线压缩的 growth 估算:thinking 输出算进 max_tokens 预算,检查 predictive 公式的参数是否要动(教训 4:小参数退化先想一遍)。

## 测试

wiremock 契约:请求体含 cache_control 断点(位置整对象断言)、usage 三字段解析;thinking SSE 序列进 → StreamEvent/块序列出;历史带 thinking 块的请求翻译(原样回传);rollout 往返带 thinking 块;老文件前向兼容。

## 完成标准

fmt/clippy/test 全绿;真 key 验收:连续两轮请求第二轮 usage 出现 cache_read > 0;开 thinking 跑一个多步任务,历史合法(无 400)、rollout 可 resume;README、HANDOFF 更新。
