# Plan 18 — rewind / fork(会话树)✅

> 完成于 2026-07-10,提交 a9a0c5e。

## 完成记录

**入口形态(用户拍板)**:只做 CLI `--fork <id>#<seq>`(`--fork <id>` = 末尾 fork);TUI 按键选点、server `thread/fork` 留到以后,schema 已兼容。**rewind 不做独立机制**:就是对当前会话在更早截点 `--fork`——依据是 codex 上游已把 `thread/rollback` 标 DEPRECATED、正收敛到 fork-by-turn-id,而 cc 的同文件树形 rewind 需要 leaf 计算 + 单亲回走 + 孤儿 tool_result 回收整套读侧,对 kloop 线性重放改动量远超收益。

### ✅ 追加(2026-07-14,server 入口,plan A 会话·提交号 61c40ce)

`thread/fork` 挂账现补上:新 RPC `thread/fork {threadId, cut?}`(缺 `cut` = 末尾
fork)复用现成 `rollout::fork_session`——复制前缀 → `resume_session` → **spawn 成活
thread**(同 `thread/resume`)→ 返 `{threadId, messageCount}`,client 可立即
`turn/start`。源无需是 active thread(直接读文件,可 branch 休眠历史)。非法 cut →
`fork_session` 报错带合法点提示,映射 SERVER_ERROR;`cut` 非整数 → INVALID_PARAMS。
测试 +2(server duplex):fork 建活 thread + 血缘 `fork_origin` 指回源 `#cut` + 源文件
不动 + fork 可独立 turn;非法 cut 列合法点 + 缺源报错。**TUI 按键选点仍挂账**。

### ✅ 追加(2026-07-14,TUI 按键选点收尾——原地 rewind,提交号 PENDING)

最后一处挂账("TUI 按键选点")补上,做成**原地 rewind**(cc `/rewind` 语义)而非
CLI `--fork` 那种"分叉后退出再 `--resume`":TUI 里 **Ctrl+R**(仅空闲)开选点弹层
→ Enter 在选中的截点 fork + **把 live History 换到该分支** + 转录区重建到更早状态 →
下一条消息续在新分支,旧分支照样落盘(可再 fork/resume)。开工前先查 `reload`:确认
TUI **无** reload 机制,但 worker 独占 History 可直接重赋值,所以进程内热切换很便宜——
真正成本在选点 picker,A(fork 后 relaunch)/B(原地)共用,故选价值更高的 B。

- **seq 权威留 core**:`rollout::fork_points(path) -> Vec<ForkPoint{seq,preview}>`,复用
  `legal_cut_seqs` 同款 `opens_user_turn` 判定(提取为模块级 fn 两处共用),排除 tip
  (末尾 rewind 是 no-op)和首轮(其前无截点);预览取该轮开头 user 消息、剥
  `STEERING_PREFIX` 框、collapse 空白。picker 列的点 = `fork_session` 会接受的点,选中
  永不被拒。
- **History::rebase(items, rollout)**:原地换历史 + 换写入文件,复用已持有的 offload_dir
  (分支共享,同 resume),usage_anchor 清空重锚,旧 rollout drop(它写的分支已在自己文件)。
- **接线**:App 加 `ForkPicker{points,cursor}` 模态(confirm 之外第二个夺键者),
  `Command::{RequestForkPoints,Fork(seq)}`;worker 加 `WorkerMsg::{ListForkPoints,Fork{seq}}`
  → `fork_here`(从 `rollout_path().parent()` 推 sessions_dir)→ `AgentEvent::{ForkPoints,
  Forked{session_id,messages}}`;`Forked` 用 `cells_from_history` 重建转录、把末尾"resumed
  session"note 改标"rewound — N kept"、换 session_id。空列表 → System note"nothing to
  rewind to yet"。
- 测试 +10:core(fork_points 列边界带预览/剥 steering 框/单轮空、rebase 换历史+重定向写入
  +原分支不动)、tui app(Ctrl+R 仅空闲请求、picker 导航+选中 Fork(seq)+吞按键、Esc 取消、
  空列表出 System note、Forked 重建转录+换 id)、tui lib(`fork_here` 在截点分叉、推 sessions
  dir、返新 id+截断消息)。全绿 427,fmt/clippy 净。TUI 交互无法管道验,靠单测锁定(同
  server slash 惯例)。

**回源调研结论**(两个 Explore agent,cc + codex 全文见会话记录,要点):
- 两家都是"物理复制前缀进新文件",都**刻意回避跨文件行级 parent 链**:cc 怕悬空引用(`/branch` 整份拷贝 + `forkedFrom` 仅元数据),codex 明说"复制换简单"(`fork_thread` 复制截断前缀 + SessionMeta `forked_from_id` 文件级标签)。
- 截点合法性两家都靠**白名单**而非事后配对校验:cc 只允许真实 user 消息(tool_result 型 user、合成消息全排除),codex 只在 turn 边界切(TurnStarted 三重校验,半截 turn 合成 TurnAborted 收尾)。
- codex rewind(`thread/rollback`)= 追加 `ThreadRolledBack{n}` 标记 + 重放跳过,一行不删;上游已废弃。cc `/rewind` = 同文件树内分叉(内存截断,新消息 parentUuid 挂回保留前缀尾部,旧 leaf 永久留存但 resume 只自动选最新 leaf)。
- codex 坑:fork 复制前缀时源 SessionMeta 残留(新文件两条 meta 靠"取第一条"兜底)——kloop 的 re-envelope 方案天然没有这个问题。

**实现**(`core/src/rollout.rs` + `cli/src/main.rs`):
- `fork_session(src, cut, sessions_dir)`:前缀物理复制 + 逐行 re-envelope(新 stem、seq 从 1、ts 保留),首行 parent = `{src stem}#{cut}` 跨文件指针——**仅血缘元数据,重放永远单文件**,`resume_session` 零改动,fork 可再 fork。`create_new` 防撞毁已有会话。
- 截点白名单 `legal_cut_seqs`:后继行必须开启新 user turn(user 角色且无 tool_result 块),或截在末尾;非法截点报错列出最近 8 个合法点。截在 compacted 标记处保留标记(重放走替换);截在标记**之前**的合法点 fork 出压缩前原文(append-only 的红利)。
- `fork_origin(path)`:只读首行取跨文件 parent;`--list-sessions` 行尾标 `[forked from {src}#{seq}]`。
- offload 跨分支共享零改动(指针是拷贝的文本,计数器扫目录 fetch_max);usage 锚点不落盘,fork 后首采样重锚定,零改动。
- 顺手重构:`parse_session` 拆出 `intact_lines`(逐行解析 + intact_end),fork 与重放共用一套坏尾判定。

**验证**:fmt/clippy/test 全绿(272,新增 6 个 fork 契约:前缀复制与跨文件血缘/ts 保留、两分支独立追加整文件断言、非法截点报错带合法点、compacted 两侧截点重放、fork 的 fork、跨分支 offload 不冲突 + CLI `--fork` 参数解析)。真 key(anthropic 轨,fork 不碰 provider 层):真实会话两轮工具调用 → `--fork <id>#4` 走另一条路(改查 Cargo.toml)→ 两文件各自 `--resume` 凭记忆答题,原会话记得两轮、fork 分支只带截点前记忆,互不影响。mock 冒烟:非法截点报错、末尾 fork、list 标注、跨文件 parent 落盘全过。

---

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
