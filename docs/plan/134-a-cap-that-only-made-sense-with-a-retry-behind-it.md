# Plan 134 — 那个上限,只有配着重试才讲得通

> 来源:Plan 133 收尾时的追问。修好「截断被读成协议损坏」之后,真正的问题露了出来:
> **为什么每轮都在撞上限?**用户:「max_output_tokens, 当前是怎么处理的」→
> 「看参考项目是怎么处理的」→「按现在的参考项目,再看看」。

## 一、现状:一个硬编码常量,三条 rail 共用,还兼任压缩估算

`MAX_OUTPUT_TOKENS: u64 = 8192`(`protocol/src/lib.rs:812`),没有配置项也没有环境变量:

| rail | 发什么 | 推理 token 算在哪 |
| --- | --- | --- |
| Anthropic Messages(`provider/src/lib.rs:509`) | `max_tokens: 8192` | `/thinking budget N` 时发 `8192 + N`(`:535`)——**thinking 预算另加,不挤占正文** |
| OpenAI Responses(`:556`) | `max_output_tokens: 8192` | **算在这 8192 里,没有任何另加** |
| Chat Completions(`:585`) | `max_tokens: 8192` | 同上 |

第四个用途在 `agent.rs:399`:`compact::max_turn_growth(MAX_OUTPUT_TOKENS)`,喂给预测式
压缩的每轮增长估计。**所以这个数字不只是请求参数。**

后果就是 Plan 133 那两轮:gw_cn 的 deepseek 在 xhigh 下,**推理 token 单独就吃满 8192**
(实测 `reasoning_tokens: 8192`,正文一个字没轮上),每轮都要靠续写恢复兜底,而恢复后
那一轮可能再次烧满。顺带,不发这个字段时 gw_cn 服务端默认是 32768——**我们主动把上限
压到了它的四分之一。**

## 二、参考项目:8192 这个量级从不单独出现

三家都在 `refs/` 下重查过一遍(2026-09-11):

- **`refs/codex`(上游 openai/codex)**:`WireApi` 只剩 `Responses`(`model-provider-info/src/lib.rs:68`),
  chat completions 已从上游移除。`ResponsesApiRequest`(`codex-api/src/common.rs:256`)
  **没有 `max_output_tokens` 字段**——不发,用服务端默认;模型元数据
  (`protocol/src/openai_models.rs:446`)只有 context window / auto-compact 阈值,
  **没有任何 per-model 输出上限**;`max_output_tokens` 这个名字在上游只用于**工具输出
  预算**。截断时 `response.incomplete` → 直接 `ApiError`,没有恢复。
- **`refs/claude-code`**:Anthropic 路径 `CAPPED_DEFAULT_MAX_TOKENS = 8_000`
  (`src/utils/context.ts:29`),**配对**撞上限自动升到 `ESCALATED_MAX_TOKENS = 64_000`
  重发一次(`src/query.ts:1474`),再不行才多轮恢复(3 次)。而 **OpenAI 兼容路径直接用
  upperLimit(默认 64000)**,`src/services/api/openai/index.ts:331` 的注释写明了为什么:
  > when thinking is enabled the thinking phase consumes the entire budget leaving no
  > tokens for the final response …… the Anthropic path's slot-reservation cap (8k)
  > **is paired with an auto-retry at 64k**. The OpenAI path has no such retry, so
  > **using the capped 8k default would silently truncate responses**.

  per-model 表里 GPT-5.6 家族 default 是 **32_000**/upper 128k。
- **`refs/codewhale`**:`TURN_MAX_OUTPUT_TOKENS = 262_144`(内部预算,**故意大于 API 请求
  上限,就是为了交错思考吃不光整轮**)、`API_MAX_OUTPUT_TOKENS = 65_536`、
  `UNCATALOGUED_COMPAT_MAX_OUTPUT_TOKENS = 8_192`——**8192 只发给目录里不认识的模型**。

**8192 这个量级在三家里只出现两次,两次都带配套**:claude-code 的 8k 配了 64k 自动重试
(注释明说两者成对),codewhale 的 8192 只是"不认识这个模型"的保守地板。kloop 两个配套
都没有,还把它当成三条 rail 通用的硬编码。

## 三、做什么

照 claude-code 那条规则落地:**没有升级重试的 rail,就不许用省槽位的上限。**

1. 常量分两档(`protocol`):`ANTHROPIC_MAX_OUTPUT_TOKENS = 8_192` 保持不变(它的 thinking
   预算本来就是另加的);新增 `OPENAI_MAX_OUTPUT_TOKENS = 32_768`,给 Responses 与 Chat。
   32768 的依据:gw_cn 服务端默认就是这个数,claude-code 给 GPT 家族的 default 是
   32_000,codewhale 的 API 上限 65_536——取三者里最保守的一个。
2. `ProviderApiFamily::max_output_tokens()` 做唯一映射,provider 的三处请求体和
   `agent.rs` 的增长估计都从它取,**不再各写各的数**。
3. `agent.rs:399` 改成按**当前 rail** 估增长:`max_turn_growth` 收到的不再是全局常量,
   而是这一轮真正会用的上限(`OUTPUT_GROWTH_CAP = 20_000` 仍然封顶)。

## 四、非目标

- **不做「先小后大重发一次」的升级重试**。它是给"为省 slot 才压到 8k"的场景配的,
  kloop 没有那个约束;而且重发要把已经烧掉的推理整轮扔掉,代价比一开始就给够高。
  已有的续写恢复(Plan 133 修好的那条)继续做兜底。
- **不动 Anthropic rail 的 8192**:那条 rail 的 thinking 预算是另加的,不存在"推理吃光
  正文"的问题。
- **不引入 per-model 输出上限目录或环境变量覆盖**:上游 codex 根本没有这个概念,
  claude-code 的表是为它自家模型维护的;kloop 现在只有一个 provider 在用,等真有第二个
  模型撞到再说。

## ✅ 已完成(2026-09-11;提交 SHA 以本条所在提交为准)

### 改了什么

`protocol`:`MAX_OUTPUT_TOKENS` 拆成 `ANTHROPIC_MAX_OUTPUT_TOKENS = 8_192` 与
`OPENAI_MAX_OUTPUT_TOKENS = 32_768`,并新增 `ProviderApiFamily::max_output_tokens()`
作为唯一映射(Mock 走 Anthropic 那档,脚本化 turn 的增长预测因此一分不变)。
`provider` 三处请求体、`agent.rs` 的增长估计都从它取值。

### 测试

- `output_caps_split_by_rail`(protocol):四条 rail 的上限一次断言成一个数组。
- `growth_follows_the_rail_cap`(core/compact):Anthropic 档 = 8_192 + 工具尖峰;
  Responses 档被 `OUTPUT_GROWTH_CAP = 20_000` 封顶——**上限变大不等于预留无限变大**。
- `chat_requests_carry_the_openai_output_cap`(provider/openai):chat rail 的请求体
  断言 32_768;Responses 的整对象请求体断言同步改成 32_768;Anthropic 的 8_192 与
  `8192 + 2048`(thinking budget)两条既有断言原样通过,证明那条 rail 没被动。
- `cargo fmt --all`、`cargo clippy --all-targets --all-features`(0 warning)、
  `cargo test` 全绿(workspace 0 failed)。

### 真实线路验收(gw_cn / `deepseek-v4-flash-0731` / xhigh)

同一个 prompt(`Output every integer from 1 to 5000 …`),同一条 rail,前后对照:

| | 轮数 | 第一轮 output_tokens | 截断提示 | 产出 |
| --- | --- | --- | --- | --- |
| Plan 133 之后(8192) | 2 轮(续写恢复 1 次) | **8192(撞满)** | `response truncated by output limit; asking the model to continue (1/3)` | 18,973 字节 |
| 本次(32768) | **1 轮** | **14,248** | 无 | 23,892 字符正文,`turn_terminal: completed`,exit 0 |

转录:`~/.kloop/projects/v1/p1_38ef03.../sessions/20260911-061714.jsonl`。
14,248 > 8,192 直接说明旧上限就是那条天花板本身。
