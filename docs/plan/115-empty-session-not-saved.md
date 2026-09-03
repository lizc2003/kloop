# Plan 115 — 启动了没说话，就不该留下一个会话

> 来源：2026-09-03 dogfood。用户：「如果只是启动了 kloop，没有一个消息，不应该保存 session」。

## 事实

`rollout.rs` 的结构体文档写着「文件在首次 append 时懒创建，所以什么都没记的会话不留痕迹」——
**这句话是假的**：每个 `Rollout::new_*` 构造函数进门就 append 一行前言
（`provider_route_initial`，server 侧还多一行 `session`），文件当场落盘。
`args.rs:442` 的 `SessionChoice::New` 正是这条路径。

实测用户磁盘：79 个会话里 **9 个是 ≤2 行的空壳**（三个项目分区都有）。

危害不止是磁盘垃圾：`resumable_sessions`（`args.rs:390`）只过滤子 agent 会话、
不看有没有内容，而 `sessions_by_recency` 按 mtime 排序——**`--continue` 会挑中
最后一次「启动就退出」的空壳，而不是真正的上一次对话**。

## 为什么不是「真正的懒创建」

直觉方案是把前言也缓存起来、第一条消息到来才建文件。但 `new_session_id`
（`rollout.rs:1596`）分配 id 的方式是**秒级时间戳 + `session_path(...).exists()`
去重**——文件的存在本身就是这个 id 的占位。不建文件，两个同一秒启动的 kloop
会拿到同一个 id，各自等用户输入；谁先发消息谁建文件，另一个之后把行**追加进同一个
文件**，两条会话的 id 链就交错了。现状的竞态窗口只有 id 分配到首次 append 之间的
几微秒，懒创建会把它拉长到「用户思考的几分钟」——这是净退化。

## 改动

**文件照旧在构造时创建（id 占位不变），但只写了前言的写入者在自己被 drop 时把文件删掉。**

- `Rollout` 新增两个字段：`preexisting`（构造时文件已在磁盘上，说明这是 resume/fork
  接手的文件，**永不删除**）与 `wrote_content`（append 过前言以外的行）。
- `RolloutLine::is_preamble()`：`Session` / `ProviderRouteInitial` /
  `ProviderRouteChanged` 三类是前言，其余都算内容。`append_line` 据此维护
  `wrote_content`。
- `impl Drop for Rollout`：`!preexisting && !wrote_content` 时 `remove_file`。
  两个前提缺一不可——只删自己创建的文件，且只在它除前言外一无所有时。
- `recover()` 构造的 rollout（resume/fork 的接手者）显式 `preexisting: true`。
- `resumable_sessions` 跳过「可读且 0 条消息」的会话：新的空壳不再产生，但**存量的
  9 个仍在磁盘上**，不过滤的话 `--continue` 照旧被它们赢走。不可读的文件保留在列表里
  ——`session_line` 会打印原因，隐藏它等于隐藏问题。

`--list-sessions` 不过滤：它是诊断视图，磁盘上有什么就报什么。

**server 侧同受影响，且是想要的**：`thread/start` 也经 `Rollout::new_with_runtime_pending_route`
建文件，thread 在内存里活着期间文件就在（`thread/resume` 找得到）；一个自始至终没有消息的
thread 被销毁时，它的文件跟着走——回放出来本来也是空的，`runtime` 行记的 cwd/model 没有
内容可服务。

## 验证

`cargo fmt` + `cargo clippy --workspace --all-targets -D warnings` + `cargo test --workspace` 全绿。
rollout 新增 3 条测试：只有前言的会话 drop 后自删（且**活着时文件在**，锁住 id 占位这半）、
一条消息就留住文件、resume 后不说话绝不删文件。CLI 新增 1 条：用 `mem::forget` 模拟被硬杀
的写入者留下的空壳，断言它虽是最新的文件也不会赢走 `--continue`。
