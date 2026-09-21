# Plan 189 — 唯一的宣告,就是第一条 delta

> 来源:2026-09-21,用户贴来一次真机报错——一个走 Responses 轨的推理模型,开一轮审查
> 立刻死在 `provider protocol error: openai-responses reasoning event referenced an
> unknown part`。当场查完做完(✅ 见文末)。编号跳过 188:plan 187 的续作已经点了那个号。

## 现状与定性

抓了一次该网关的流,reasoning item 的生命周期是这样的:

1. `response.output_item.added` —— `type: "reasoning"`,**没有 status**,`content`/`summary` 都空;
2. 几十条 `response.reasoning_text.delta`,`content_index: 0` —— **中间没有任何
   `response.content_part.added`**;
3. `response.reasoning_text.done`,同一个 `content_index`;
4. `response.output_item.done` —— `content: [{"type": "reasoning_text", "text": …}]`,
   **没有 `content_part.done`,也没有 `encrypted_content`**。

也就是说,**那个 part 第一次被描述,是在 item 关闭时的 content 数组里**;流中间唯一提到它的,
就是第一条 delta。`responses.rs::reasoning_part_mut` 要求 part 必须先被开帧宣布过
(`*_part.added`),于是第一条 delta 就把整条流判死。**这不是偶发**:这条轨 + 这个模型的每一轮
都走这个形状,组合完全不可用——用户的默认 provider 正是它。

对照之下,同一个函数早就为另一台网关放宽过一次:那台**开每一个 summary part,却只关最后一个**,
当时的结论写在代码注释里——「per-part `.done` 本来就只是一道更早的冗余检查,真正校验流的是
item 边界」。这次撞上的是同一句话的另一半:**开帧也是冗余的**。

## 裁决

### 一、reasoning part 由第一条寻址它的帧打开

`reasoning_part_mut` 从 `get_mut(&index).ok_or(…)` 改成 `entry(index).or_default()`,
**两条通道同一个口径**(summary 与 raw content 走的是同一个函数,不分叉反而少一个分支)。
放弃的那道检查换不来任何东西:`verify_reasoning_parts` 在 item 边界要求**索引连续**、
且**每个累加出来的 part 与最终数组同位置的文本逐字相等**——服务端从没交代过的索引,
仍然在一帧之后失败,只是错误换成「indices were not dense」。

### 二、message part 不跟着放宽

它的开帧不是冗余的:**`output_text` 还是 `refusal` 是开帧决定的身份**,拒答还要据此定这一轮的
结局。没有任何实测形状要求放宽它,就不放。

### 三、不做的

- **不为「只有 `content_part.done` 没有 delta」的假想形状放宽**:那条路径要求 `field_done`,
  而 `field_done` 只由 `reasoning_text.done` 置位。没见过的形状不预先兼容。
- **不动 `add_content_part` 的重复插入检查**:先 delta 后 `part.added` 的顺序没在任何一条线上
  出现过,继续让它报重复。

## 顺带核实(都在同一次真机里跑过)

- 这份 reasoning **没有 `encrypted_content`**,于是落成 signature-less thinking。
  provenance 的 `Plain` 形状在 plan 164 已经对所有家族放开,回放时带 `encrypted_content: ""`
  发回去,网关照常接受:真机 headless 两轮 + 一次 bash 工具调用完整跑通。
- 该网关的 SSE 还有两处早就被容忍的非标准写法(`data:` 后不带空格、夹一行 `:` 注释),
  这次确认都不碍事。

## 验收

- `provider/tests/responses.rs` 两条新测试:
  `raw_reasoning_content_parts_may_open_on_their_first_delta`(照抄实测形状:无 item status、
  无开帧、无关帧、无 encrypted_content,断言 thinking 文本与空 signature)、
  `a_reasoning_delta_on_an_unaccounted_index_still_fails_closed`(索引 1 只发一条 delta,
  最终数组只有一个 part,仍在 item 边界失败)。
- 真机:默认 provider 一次 `--headless` 带工具调用的两轮对话,不再报协议错。

## ✅ 完成

2026-09-21 完成,一次提交(SHA 即本条所在提交)。`make check`(fmt + clippy -D warnings +
全量 1614 条测试)全绿;`responses.rs` 缩了 3 行 code,顺手跑了 `make arch-baseline` 收水位。
