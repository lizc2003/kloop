# Plan 24 — 中途注入:steering + 子 agent 回灌(备忘)

> 备忘,未开工。**本 plan 吸收 plan 17 片 6 的机制部分**(异步派发 + mailbox 回灌)。
> 开工前读 HANDOFF + plan 17 的"回源调研结论"节(两家在此已收敛)。

## 目标

一个"**step 边界注入**"机制,两个消费者:
1. **用户 steering**:agent 干活时用户插一句,下一个 round 边界注入进历史(不硬断
   当前工具,Ctrl+C 才是硬断)。
2. **子 agent 回灌**:子 agent 终态时把摘要投递父 agent 的历史(plan 17 片 6 的
   mailbox 回灌)。

两家收敛(教训 14,已在 plan 17 记录):**只在 step 边界注入,绝不插进在途请求**。
cc task-notification 入队、工具循环 drain 转 attachment;codex mailbox delivery phase
闸门 + `forward_child_completion_to_parent`。形态已定,合成一个机制两个客户最省。

## 关键决定(开工时定)

- **注入点**:round 循环边界(`dispatch_tools` 之后、下一次采样之前)。绝不插进在途
  stream。
- **通道**:一个 per-turn 注入队列挂在 agent 循环可达处。用户 steering:前端输入在
  turn 运行时不打断、进队列;agent 每 round 开头 drain。子 agent 回灌:子 agent 终态
  → 投递同一队列(子 agent 是独立 tokio task,需要终态回调/JoinHandle 完成侦测)。
- **注入形态**:成 user 消息进历史(下一轮采样前)。cc 是 attachment 进本 turn
  toolResults;kloop 倾向简化成 user 消息。子 agent 摘要截断(codex 截 ~900 token +
  "换个任务再派"引导)。
- **打断语义**:steering 软注入,不硬断当前 round 的工具(Ctrl+C 保持硬断那条)。
- **前端接线**:TUI/plain 在 turn 运行时收输入 → 入队而非当作新 turn;server 经
  `turn/steer`(挂账)。

## 现状底座

后台 bash 已有队列/monitor/Ctrl+C 打断;task 子 agent 是独立 tokio task(回灌要它
终态回调);history append-only + 修补语义已能容纳中途插入的 user 消息。

## 不做

硬打断在途工具(Ctrl+C 管);多用户并发 steering;插进在途请求(两家都不做)。

## 测试

round 边界注入(在途请求不受影响)、steering 消息进下一轮历史、子 agent 回灌摘要进
父历史且顺序正确、turn 空闲时注入触发新 turn(若做 autowake)。

## 完成标准

fmt/clippy/test 绿;真 key 一次 steering 或一次子 agent 回灌闭环;README、HANDOFF;
plan 17 片 6 标记为本 plan 承接。
