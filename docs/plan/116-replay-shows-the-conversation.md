# Plan 116 — 重放显示的是对话，不是管道

> 来源：2026-09-03 dogfood。用户 `kloop -r` 恢复 gateway 的审查会话，最终结论
> 「太散并且把子 agent 的结果也显示出来了」。

## 事实

`cells_from_history`（`app.rs:1779`）把**每一条** `Role::User` 的文本都重放成
`Cell::User` —— 而 user 角色的消息里有一大半不是用户说的话：`inbox.rs` 把子 agent
结果、后台程序/workflow/shell 的回报、定时任务、peer 消息、以及用户 steering
统统包成 user 文本喂给模型（十种 framing，各自有前缀）。

于是 `-r` 之后，那条 `A background sub-agent you dispatched has finished…` 加上
子 agent 数千字的 `<analysis>`/`<summary>`，被当成"用户打的字"整段贴在屏幕上，把
真正的结论撑散了。**实时不会有这个问题**（注入不产生 UI 事件），只有重放会。

同一段重放里还有连续四行 `∗ Thought` ——磁盘上没有耗时，每个 thinking block 各占
一行，四行说的是同一件事。

## 改动

1. **`inbox::replayed(text) -> Option<Replayed>`**（core）：把 framing 反过来读。
   `Steer` 是用户在说话——**剥掉包装，显示他打的字**；其余九种是 harness 在回报，
   折叠成一行 `Note`：标签 + 首行细节（`sub-agent result · [Agent agent-1]`），
   多个回灌因此仍能区分。返回 `None` 的就是用户原话，原样重放。
2. **`cells_from_history` 用它分流**：`UserText` → `Cell::User`（去掉 steering
   包装），`Note` → `Cell::Note`（一行、dim、截断）。
3. **连续的无耗时 thinking 折叠成一行**：后续 block 的文本追加到第一格的 `text`
   里（渲染只显示 `∗ Thought`，但内容不丢——将来若做展开还在）。

## 边界

- 只改**重放**。实时路径不经过 `cells_from_history`，行为不变。
- 只认 `inbox` 的十种 framing。compaction 的 `DROPPED_PREFIX` 标记自己就是一行
  自解释文本，不折叠。
- Note 的标签取首行细节而不是丢掉：一个 turn 里回灌多次时，`[Agent agent-1]` 与
  `[Agent agent-2]` 必须能分开。

## 验证

`cargo fmt` + `cargo clippy --workspace --all-targets -D warnings` +
`cargo test --workspace` 全绿。core 新增 1 条（steering 剥壳、sub-agent 与 shell
折叠成标签 + 细节、用户原话返回 `None`），TUI 新增 1 条整对象断言（注入折叠成
Note、steering 还原成 User、两个 thinking 折叠成一格且文本都在）。
