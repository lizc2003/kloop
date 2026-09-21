# Plan 116 — 重放显示的是对话，不是管道

> 来源：2026-09-03 dogfood。用户 `kloop -r` 恢复那个 Go 仓库的审查会话，最终结论
> 「太散并且把子 agent 的结果也显示出来了」。

## 事实

`cells_from_history`（`app.rs`）把**每一条** `Role::User` 的文本都重放成
`Cell::User` —— 而 user 角色的消息里有一大半不是用户说的话：`inbox.rs` 把子 agent
结果、后台程序/workflow/shell 的回报、定时任务、peer 消息、以及用户 steering
统统包成 user 文本喂给模型（十种 framing），压缩也用同样的方式把折叠掉的前缀
换成一条 summary marker。

于是 `-r` 之后，那条 `A background sub-agent you dispatched has finished…` 加上
子 agent **两万余字**的 `<analysis>`/`<summary>`，被当成"用户打的字"整段贴在屏幕上，
把真正的结论撑散了；紧挨着的还有一条 18160 字的压缩摘要。**实时不会有这个问题**
（注入不产生 UI 事件），只有重放会。

同一段重放里还有连续四行 `∗ Thought` ——磁盘上没有耗时，每个 thinking block 各占
一行，四行说的是同一件事。

## 第一版做法是错的，而且当场被打脸

最初的实现是**按 framing 前缀做字符串匹配**（`inbox::replayed`）。它在自测里全绿，
拿用户给的真实会话一跑却认不出来：`SUBAGENT_PREFIX` 的措辞在同一天被另一个会话
（plan 117）改长了——尾部多了「如果你已经交付了本轮答案，只补这条结果改变或新增
的部分」——而磁盘上那条是改动之前写的。**用当前 prompt 措辞去认历史消息，措辞一
演进就全部失配，而且没有任何报错。**

用户拍板：**记录标识，不要字符串匹配**；不必考虑历史兼容，保持代码干净。

## 改动

1. **`kloop_protocol::Injected` + `Message.injected`**：与 `provider_provenance`
   同型的内部字段（`skip_serializing_if`，provider adapter 逐字段手搭请求，因此
   永远不会进 provider wire）。枚举带上区分同类所需的数据：
   `SubAgent { label }`、`Shell { id }`、`PeerMessage { from }` 等。
   新增 `Message::injected(kind, text)` 构造函数。
2. **生产者盖章**：`InboxItem::kind()` 给出标识，`into_user_message()` 直接产出
   带标识的 `Message`；`agent.rs` 的两处 drain 改用它。压缩的 summary 与
   dropped marker 同样盖 `ContextSummary` / `DroppedPrefix`。
3. **消费者读标识**：`compact::is_existing_summary` 从
   `text.starts_with(SUMMARY_PREFIX)` 变成读字段——marker 文本是给模型读的，不是
   给代码匹配的。`cells_from_history` 先看 `message.injected`：steering 还原成
   用户打的字，其余折叠成一行 `Note`，标签里带枚举中的 label/id，同一 turn 回灌
   多次仍能区分。
4. **仅剩的一处按结构取值**：steering 要显示用户原话，而原话在 framing 之后。
   `inbox::steering_body` 取第一个换行之后的全部——依据是 `into_message` 恒把
   framing 写成**一行**，由 `every_framing_is_a_single_line` 锁死，与措辞无关。
5. **连续的无耗时 thinking 折叠成一行** `∗ Thought`，后续 block 的文本追加进第一
   格（渲染不显示，但内容不丢）。

## 边界

- 只改**重放**。实时路径不经过 `cells_from_history`，行为不变。
- **不做历史兼容**（用户拍板）：改动之前写的会话没有这个字段，重放仍是原文。
- 显示文案（`sub-agent result · agent-1`）留在 TUI，core 只提供标识。

## 验证

`cargo fmt` + `cargo clippy --workspace --all-targets -D warnings` 全绿；
`cargo test` 分包 kloop-core 780 / kloop-tui 201 / kloop-protocol 17 全绿。
新增测试：protocol 的 serde 往返（不带标识时字段不上 wire，带标识时 round-trip），
inbox 的 `every_item_records_its_kind_and_steering_keeps_the_user_words` 与
`every_framing_is_a_single_line`，TUI 的整对象重放断言。
