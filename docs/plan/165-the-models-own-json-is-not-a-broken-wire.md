# Plan 165 — 模型自己写坏的 JSON,不是坏掉的协议

> 来源:2026-09-18,[plan 163](163-one-usage-report-sent-twice.md)、
> [plan 164](164-chat-reasoning-is-a-real-shape.md) 之后的第三次报错:
> `openai-compat tool bash returned invalid JSON input: expected value at line 1 column 96`。
> 用户拍了两件事:**先兼容**(能救的救回来),救不回来的**可恢复**(回灌给模型重发)。
> 当场做完(✅ 见文末)。

## 现状与定性

`provider/src/lib.rs::parse_tool_input`(三条轨共用)把模型发来的 arguments 字符串解析成
JSON,解析不了就 `ProviderFailure::protocol` —— 整个流失败、整轮作废,连这一轮已经完整流完
的文本和**其它正常的 tool call** 一起丢掉。

第一次报错时手上只有 serde 的"第 1 行第 96 列",原始字符串没人留,定不了性:是模型写坏了,
还是我们的分片累加拼坏了。于是先给错误加上原文(400 字节封顶、按字符边界截断)。**下一次报错
立刻给出了答案**:

```
{"command": "git -C ... show a8e6543d --stat", "description": 查看提交概要与文件列表}
```

`description` 的值**没有引号**,第 96 列正是那个 `查`。同一天在 responses 轨上复现同样的形状
——**与轨无关,是模型的通病**。13 次真机采样全部合法,说明它偶发、但一发就是一整轮。

对照之下,kloop 对"模型叫了一个不存在的工具"的处理是 `unknown tool: xxx` 当 tool_result
回灌、轮次继续。同样是模型的错,两种待遇。

## 裁决

### 一、能读懂的先修,白名单式

`provider/src/tool_input.rs` 只修三种**读法唯一**的失误:

1. value 位置的裸文本(本次这个),补引号;
2. 字符串里的裸控制字符(模型写 heredoc 时的真换行),转义成 `\n`;
3. `}` / `]` 前的多余逗号,删掉。

**修复器只会重写,不会放行**:重写完的文本仍然交给 `serde_json` 判定。这条是硬约束,因为这些
参数会变成 shell 命令——**一个靠猜的修复,就是一条模型没写过的命令**。所以裸值里只要出现
`"` `{` `}` `[` `]` `,` `:` `\` 任意一个就放弃(`{"command": rm -rf {a,b}/tmp}` 不修),
`true`/`false`/`null`/数字仍按字面量解析(`"quiet": true` 不会被当成字符串)。

### 二、修不动的,变成一次"失败的工具调用",不是一次失败的轮次

新增 `AssistantBlock::InvalidToolUse { id, name, raw, error }`,**只活在 provider→core 这段流里**:

- 落历史时规范化成 `ContentBlock::ToolUse { input: {} }` —— 三条轨都要求这里是 object,
  而 `tool_use` 必须有 `tool_result` 配对。**历史里永远只有合法形状**,rollout / native 协议 /
  回放一行都不用改,这是这个形状最值钱的地方。
- 原文和解析错误走 `tool_result`(`is_error: true`)回灌给模型:
  `bash was not run: its arguments were not valid JSON (...). You sent: ... Call bash again with valid JSON arguments.`
  **原文必须回给模型**——只给一个列号,它往往原样再发一遍。
- 这一条**不分发**(不执行工具),同一轮里其它调用照常执行,结果按调用顺序排在一起。

### 三、`{}` 不是"没有参数",所以不能让它去执行

规范化后的 input 是空对象,如果照常分发,`bash` 会自己报"缺 command",**错误信息误导**,更糟
的是别的工具可能对空参数有默认行为。所以"别执行"这个信号必须随流走到 core:`SampleOk` 多一个
`invalid_tool_inputs: Vec<(id, 给模型的话)>`,`dispatch_round` 按 id 跳过它们、把失败结果并回
原位。

### 四、Anthropic 轨同样放宽

那条轨的 `input_json_delta` 也是模型写的字符串。三条轨同一个判定,不留一条"这里仍然会猝死"的
例外——两条既有契约测试(`malformed_tool_input_fails_closed`、`malformed_arguments_fail_closed`、
`malformed_function_arguments_fail_closed`)因此改成断言新形状,名字也跟着改。

### 五、不做的

- **不加"连续坏 JSON"的专门上限**:已有 `max_rounds` 兜底,再加一个计数器是为想象中的模型写的。
- **不修未转义的内层引号**(`git commit -m "x"` 里的引号):那要猜字符串在哪结束,正是第一条
  裁决禁止的猜法。
- **不静默**:修复走了就是走了,但修不动的那条会在 transcript 里留下一次失败的工具调用,
  用户看得见模型犯了什么错。

## 验收

- `tool_input::tests` 七条:真实样本(中文裸值)、裸值后面还有字段、歧义一律拒绝、字面量不动、
  裸控制字符、尾随逗号、修完仍不合法则拒绝。
- `provider::tests::tool_input_requires_complete_json_object`:空参数、正常、可修、非 object、
  原文随错误走、长参数按字节截断且不切碎多字节字符。
- 三条轨各一条契约测试改写成"invalid call + terminal";openai 轨另加
  `repairable_arguments_still_reach_the_tool`。
- `agent::unreadable_tool_arguments_become_a_failed_result_and_the_turn_goes_on`:
  一轮里一好一坏 —— 好的照常执行,坏的拿到错误结果,下一轮模型接着干活,turn 正常完成。

## ✅ 完成

2026-09-18 完成,一次提交 `<sha>`。`cargo fmt --check` + `clippy -D warnings` +
全量 `cargo test`(1588)全绿。
