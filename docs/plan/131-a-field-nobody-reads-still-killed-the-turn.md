# Plan 131 — 没人读的那个字段,照样打死整轮

> 来源:2026-09-10,用户 dogfood:「查 `~/.kloop/config.toml` 里面的
> deepseek-v4-flash-0731 模型,调用失败,查原因」。
>
> 复现(`kloop --headless`):`error: provider protocol error: openai-responses
> missing or invalid response status`。一次真实调用一个字都没吐出来。

## 一、根因:plan 124 只改了一半

直接 curl 那条 direct 网关(`ai-coding-sr-bj-direct`)取原始 SSE,开场帧长这样:

```
event: response.created
data: {"type":"response.created","response":{"created_at":...,"id":"resp_0217...",
       "max_output_tokens":32768,"model":"deepseek-v4-flash-ga-260731",
       "object":"response","service_tier":"default","caching":{"type":"disabled"},
       "store":true,"expire_at":...}}
```

**没有 `status` 键**。OpenAI 官方形状里这里是 `"status":"in_progress"`;这家网关整条流
其余部分完全正常,`response.completed` 也照发 `"status":"completed"` 和完整 usage,模型
本身答完了。挂掉的只有开场那一帧。

`responses.rs` 的 `response_identity()` 把 id 和 status 绑在一起取,而 status 走的是
`required_str` ——字段缺席即协议违约,fail-closed 打死整轮。

**这正是 plan 124 已经裁定过、但只执行了一半的契约。**那次的原话是:

> status 在开场帧里没有任何一处被读(终态事件的 status 走另一条路径),而 Responses 的
> 开场 status 合法取值包含 queued——检查一个不消费的字段,是把上游的用词变成了这里的
> 协议违约。

当时修掉的是"**值**变化算违约",留下了"**键**必须存在"。代码里那个 `let (id, _status)
= response_identity(...)` 的 `_` 前缀和上方注释("descriptive and never read")都在说这个
字段不被消费,校验却还在——注释与行为分了家,分家的那一半等了一个不发这个键的网关来撞。
gw_router 照发 status,所以一直没暴露。

## 二、第二个坑:开场 part 的 text 也没有

status 修完,同一条真实调用立刻换了个错:

```
error: provider protocol error: openai-responses missing or invalid summary part text
```

同一份 SSE 里:

```
event: response.reasoning_summary_part.added
data: {..., "part":{"type":"summary_text"}, ...}     ← 没有 text 键

event: response.reasoning_summary_part.done
data: {..., "part":{"type":"summary_text","text":"We need answer in ..."}, ...}   ← 有
```

这家网关的一贯风格是**只发必填字段**:`status`、summary part 的 `text`、reasoning item
的 `summary`、message item 的 `content` 全都省掉了(后两样 plan 124 的 `parts_array()`
已经挡住,所以没再报)。开场 part 的 text 在参考实现里恒为 `""`,而"每个字符都从 delta
来"意味着**缺席与空串语义等价**——正是 plan 124 (a) 判据的同一形状,换了个字段。

## 三、做了什么

1. `response_identity()` 换成 `response_id_of()`,只取 id。**一个字段只在真正消费它的
   地方检查**:`terminal_outcome()` 要靠 status 分辨 completed / incomplete,那句
   `required_str` 就搬进它自己的函数体;开场帧和终态身份比对都只读 id。
2. 新增 `opening_part_text()`:`*_part.added` 的文本字段缺席按 `""` 处理。三处开场
   seed 都走它——reasoning summary part、reasoning content part、message content part
   (`message_part_field()` 从 `message_part_value()` 里抽出来共用)。**`.done` 那半继续
   `required_str`**:那里的值真的要跟累积的 delta 比对,缺席是一个没法核对的声明。
3. 开场帧分支上方的注释补上这次的实例:不止"值"不该检查,"键"也不该要求。
4. 四条测试,每条钉一半:
   - `opening_frames_without_a_status_key_are_accepted` / `a_terminal_frame_without_a_status_key_still_fails`
   - `opening_parts_without_a_text_key_are_empty` / `a_closing_part_without_a_text_key_still_fails`

   宽的那一半不许顺手放宽窄的那一半。

## 四、验证

`cargo fmt` 干净,`clippy --workspace --all-targets -D warnings` 全绿,`cargo test
--workspace` 全绿。**外加真实端点**:`kloop --headless` 打 gw_cn 的
`deepseek-v4-flash-0731`,修前 `missing or invalid response status` 一个字不出,修后
纯文本轮与带工具轮都跑通。第二个坑正是这样剥出来的——单元测试只验证得了已经想到的
那一层。

## 五、非目标

- **不放宽终态帧**:那里的 status 真的决定分支(EndTurn / Incomplete / 各种 OutputLimit),
  缺席就是没法判断,继续 fail-closed。
- **不做"未知字段一律容忍"的整体转向**:这次只把一个从不被读的字段从必需降为忽略。
  fail-closed 的边界该不该整体挪一次,是另一个题目(`is_out_of_band` 那段注释里也挂着
  同一个悬念)。

## ✅ 已完成(2026-09-10;提交 SHA 以本条所在提交为准)

- `crates/provider/src/responses.rs`:
  - `response_identity()` → `response_id_of()`(只取 id);`terminal_outcome()` 自己
    `required_str(&response["status"], …)`。三个调用点各归其位。
  - 新增 `opening_part_text()`(缺席按 `""`),三处开场 seed 走它:reasoning summary
    part(`response.reasoning_summary_part.added`)、message content part、reasoning
    content part(后两处在 `add_content_part()`)。`message_part_field()` 从
    `message_part_value()` 抽出,让开场路径能只换取值方式、不换字段选择。
  - 开场帧分支的注释补上"要求键存在"与"要求值合法"是同一个错误的两半。
- README:strict-parsing 那段补两句——lifecycle 帧只读 id、`*_part.added` 可以不带
  text/refusal,而两者的 `.done` / terminal 对偶仍然必需。

### 测试

`crates/provider/tests/responses.rs` 新增四条,宽窄成对:

- `opening_frames_without_a_status_key_are_accepted` / `a_terminal_frame_without_a_status_key_still_fails`
- `opening_parts_without_a_text_key_are_empty` / `a_closing_part_without_a_text_key_still_fails`

四条的 fixture 都**照抄真实网关形状**(reasoning item 不带 `summary`、message item
不带 `content`、part 不带 `text`),而不是补齐成理想形状——教训 112(c) 的直接应用。

`cargo fmt` 干净;`clippy --workspace --all-targets -D warnings` 全绿;
`cargo test --workspace` 退出码 0(provider 那个二进制 41 passed)。真实端点:
`kloop --headless` 对 gw_cn `deepseek-v4-flash-0731`,纯文本轮答出 `1+1 等于 2。`,
带工具轮跑通 `glob` + `read_file` 两个工具调用。
