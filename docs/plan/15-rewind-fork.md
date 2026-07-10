# Plan 15 — rewind / fork(会话树)

> 一个会话完成。开工前先读 docs/plan/HANDOFF.md。参考:plan 7b 的 rollout 信封(id/parent/ts 就是为这一天埋的地基);codex rollout 的 fork/rewind 机制回源核对(教训 11);cc 的 /rewind 语义作对照。

## 目标

从已有会话的某个历史点开出新分支:新 session 文件,首行 parent 指回原文件的 `{stem}#{seq}`,从截点继续对话。原文件不动(append-only 不破)。探索性任务走错路时不用从头再来。

## 设计要点

- **入口形态**(开工时问用户,最小闭环选一个):`--fork <session-id>#<seq>` CLI 起步最省;TUI 按键选历史点、server `thread/fork` 记为可能性,schema 先留兼容。
- **截点合法性**:截点必须落在消息边界且不切开 tool_use/tool_result 对(compact 已有同类边界逻辑,看能否复用);非法截点直接报错列出附近合法点。
- **重放语义**:fork = 读原文件到截点(经过 compacted 标记时按现有替换语义)+ 新文件首行记 parent 跨文件指针。`resume_session` 的修补逻辑(孤儿/坏尾)对截断视图同样适用,验证别双算。
- **offload 共享**:off-xxxx 指针跨分支指向共享目录,fork 不复制;计数器 fetch_max 已经防覆盖,加测试锁死跨分支不冲突。
- **usage 锚点**:不落盘,fork 后首采样重锚定,应当零改动——验证即可。
- **展示**:`--list-sessions` 要不要显示树形(parent 关系),开工时定;最小可以只在行里标 `forked from …`。

## 测试

fork 后两分支独立追加互不影响(整文件断言);跨文件 parent 链正确;非法截点(对中间、compacted 内部)拒绝;经过 compacted 标记的截点重放;offload 计数跨分支;fork 出的会话可再 fork。

## 完成标准

fmt/clippy/test 全绿;手工验收:真 key 跑一段带工具调用的会话,fork 到中间点走另一条路,两个文件各自 `--resume` 都能继续;README、HANDOFF 更新。
