# Plan 205 — `/clear` 开的是新会话,不是把旧会话抹白

> 来源:2026-09-24 plan 203 收尾后,用户问 `/clear` 的行为,对照三家参考后说「我想开一个 plan,
> 做清屏加开一个全新 thread」。

## 一、为什么

现在的 `/clear`(`core/src/commands/clear.rs`)是**在原会话里抹白**:`history.replace_all(vec![])`
往原 rollout 追加一行 `replacement: []` 的 `compacted`,session id 不变;另外只清了 todo、已解锁的
延迟工具、inbox 里未送达的条目。于是清空之后还挂着一串旧会话的东西:

- **`FileState` 原样留着**:模型没见过的文件照样能 `edit_file`(plan 195 的"读过就放行"失效);
  "在上下文里的行"没清,重读会被误报成"那些行还在上面";plan 197 会提醒模型一个它不记得读过的文件变了。
- **后台工作不停**:还在跑的子 agent、后台 shell、program/workflow,完成后结果注入清空后的新对话。
- **定时任务仍归旧会话**,scheduler 的 owner 还是旧 id。
- **cache key、压缩的 transcript 指针、子 agent 会话文件名**都还是旧 session id。
- 新旧两段对话挤在一个文件里,resume 列表里分不开,要回到清空前只能手动 fork。

**逐项补"清"是在追一个会不断变长的清单**:`Config` 里每加一个会话级字段,`/clear` 就可能漏一个。
换成"开一个新会话",新会话的状态天然是空的——漏清的问题从结构上消失。

### 参考怎么做

- **codex**:`/clear` = 清终端 + 开一个全新 thread。`SlashCommand::Clear` 发 `AppEvent::ClearUi`
  (`codex-rs/tui/src/chatwidget/slash_dispatch.rs:249`),处理处清终端、重置 UI 状态,再
  `start_fresh_session_with_summary_hint(.., Some(ThreadStartSource::Clear), ..)`
  (`codex-rs/tui/src/app/event_dispatch.rs:328`):新建 thread(新 rollout)、关掉旧 thread、
  退订所有被跟踪的 thread(`codex-rs/tui/src/app/session_lifecycle.rs:921`);旧 rollout 留着可 resume。
  `/new` 走同一条路,只是不清屏。有待定的权限根切换时拒绝执行。
- **claude-code**:同一进程里换一个新 session id,旧 transcript 记为新会话的 parent;读文件缓存
  **明确清掉**,连同费用统计、MCP 连接、文件历史快照、plan 文件名、已发现的 skill、会话元数据;
  前台任务杀掉,后台任务保留并把输出重新指向新会话;清空前后各跑一次会话结束/开始 hook。
- **codewhale**:新 session id,清 todo/历史/计数,结束旧会话的子 agent 与写锁;**但读文件追踪器
  每个引擎只建一次、不重置**——和 kloop 现在同一个缺口,是反例。

## 二、目标形状

**`/clear` = 清屏 + 结束当前会话 + 开一个全新会话**:新 session id、新 rollout 文件、全新的会话级状态。
旧会话文件**原样留着、不再追加空 `compacted`**,`--resume` 能回到清空前的样子。
provider 路由沿用当前这一刻的(`/provider` 切过的算数),和 rewind 同一个理由:清空不等于换了一个人在用。

## 三、切法

### 3.1 core:一个"开新会话"的缝

`Config` 里进程级和会话级的字段混在一起,构造又在 CLI(`config_from_settings`,要 approver/questioner/notify)。
不重跑那一整套,而是在 core 加一个从现有 `Config` 派生新会话的函数,例如
`Config::fresh_session(&self, session_id) -> anyhow::Result<Config>`:

- **沿用(进程级)**:provider catalog 与当前路由、system、cwd、project_instructions、max_rounds、
  offload/sessions 目录、context window/budget、questioner、tool_sources、hooks、shell_programs、
  powershell gate、sandbox、agent_types、tool_allowlist、defer_threshold、program_limits、
  request_reduction、skills、surface。
- **新建(会话级)**:`file_state`、`todos`、`unlocked_tools`、`inbox`、`local_agent`(新 root,挂新 inbox)、
  `scheduler`(**必须新建**:`Scheduler::bind_owner` 不允许换 owner,见 `core/src/scheduler.rs`)、
  `background_shells`、`background_executions`、`session_id`(经 `bind_session`)。
- **要问用户的**:`permissions`(模式 + 本会话记住的授权)、`active_worktree`。见第五节。

**穷尽性要由编译器守**:函数体用不带 `..` 的 `let Config { .. }` 解构逐字段分类,以后加字段不分类就编不过。
这是本 plan 最重要的一条——"逐项清"的毛病就是清单会漏,这里让漏掉变成编译错误。

`History` 新建一个,挂 `Rollout::new_with_initial_route(新路径, 当前路由)`;`new_session_id(sessions_dir)` 取 id。

### 3.2 旧会话收尾

- 后台工作怎么处理:见第五节第 1 问。现成的收尾入口是 `Config::shutdown_background_work`
  (本地 agent、scheduler、后台 execution 与 shell 一并停,带 2 秒回收期限)。
- 旧 scheduler 关闭;持久化的定时任务留在旧会话名下,resume 旧会话时回来。
- 旧 rollout 不写任何新行(不写空 `compacted`):它就停在最后一条真实消息上,resume 之后就是那个样子。

### 3.3 前端

- **TUI**:worker 换 `cfg`/`history`/`provider_state`;App 更新 `session_id`、清 cells/tool_cells/agent_cells、
  重画头部。"清屏"具体清到哪一层见第五节第 4 问(kloop 的 TUI 把历史提交进终端原生 scrollback,plan 99)。
  切会话的形状照抄现成的 rewind(`WorkerMsg::Fork` → `AgentEvent::Forked`),它已经是"运行中换一个 rollout"。
- **plain REPL**(`cli/src/main.rs` 的 plain 循环):换 cfg/history,打印新 session id。
- **server**:见第五节第 3 问。
- **headless**:没有 `/clear`,不动。

### 3.4 空会话文件

`Rollout::new_with_initial_route` 立刻写 route 行,连按几次 `/clear` 就会留下几个只有 route 行、
没有消息的会话文件,挤进 resume 列表。开工时定:rollout 懒创建(第一条消息才落盘),
还是 resume 列表过滤掉没有消息的会话。倾向后者——改动面小,且对别的原因产生的空会话同样有效。

## 四、顺带发现:rewind 之后 `cfg.session_id` 没换

TUI 的 rewind(`tui/src/lib.rs` 的 `WorkerMsg::Fork`)把 history 换到 fork 出来的新文件,但只用
`clone_with_provider_route` 重建了 `cfg`,**没有重新 `bind_session`**。于是 rewind 之后:压缩摘要里的
transcript 指针指向**父**会话文件;cache key、子 agent 会话文件名(`{session_id}-agent-N`)、scheduler owner
都还是旧 id。没有测试钉住。

本 plan 的缝(3.1)落地后,rewind 应该改走同一个缝(它就是"开一个从某点起的新会话")。
**开工时问用户**:随本 plan 一起修,还是单独一个提交。

## 五、开工时必须问用户的点(一次问一个)

1. **后台工作**:清空时还有在跑的子 agent / 后台 shell / program / workflow,怎么办?
   - 全部停掉(推荐):最简单,新会话干干净净;代价是正在跑的活没了。停之前在屏幕上说一句停了几个。
   - 跟 claude-code:后台的保留,结果改投到新会话——kloop 的回灌路径绑在旧 inbox 上,改投要动 inbox 的归属,面大。
   - 有在跑的就拒绝 `/clear`,让用户先等或先停。
2. **权限**:permission mode(manual/accept/bypass/plan)与本会话记住的授权,新会话里还算不算?
   倾向 mode 保留(那是用户选的工作方式)、记住的授权清掉、plan mode 退出——但这是用户的决定。
3. **server 上的 `/clear`**:server 本来就有 `thread/start`。选项:`/clear` 在服务端开新 thread 并通知客户端
   新 thread id;或者 server 上拒绝 `/clear`、让客户端自己 `thread/start`(协议更干净)。
4. **"清屏"清到哪**:只清 kloop 自己的视图,还是连终端 scrollback 一起清(codex 是清终端)?
   后者意味着清空前的对话在终端里也看不到了,只能 resume 回去看。
5. **worktree**:在 worktree 里 `/clear`,新会话留在 worktree 里,还是回到启动目录?
6. **要不要同时加 `/new`**(开新会话但不清屏,codex 有)。
7. 第四节的 rewind 修复随本 plan 做还是单独做。

## 六、测试

- `fresh_session`:会话级字段全是新的(`FileState` 空、todo 空、inbox 空、scheduler 未绑旧 owner、
  session id 为新 id),进程级字段指向同一份(`Arc::ptr_eq`)。
- 清空之后:清空前读过的文件 `edit_file` 被拒("must read before modifying");重读不触发重读提示;
  清空前读过、之后被外部改的文件不进 plan 197 的提醒。
- 旧 rollout 最后一行是清空前的最后一条消息,没有空 `compacted`;`resume_session(旧路径)` 得到清空前的历史;
  新 rollout 只有新会话的内容,首行 route 与清空前的当前路由一致。
- 后台工作按第 1 问的决定:停掉的不再往新会话注入任何东西(用一个会在清空后完成的后台 shell 钉住)。
- 定时任务:旧会话建的任务在新会话里不触发;resume 旧会话后仍在。
- TUI:`/clear` 后 App 的 `session_id` 是新 id,cells 只剩清空提示。
- 若做第四节:rewind 之后压缩指针指向 fork 文件,cache key 为 fork 的 id。
- 若 resume 列表过滤空会话:只有 route 行的会话文件不出现在列表里。

## 七、完成时要一起做的

- `rust/DESIGN.md`:先读现在写 `/clear` 的段落(grep `clear`),改写成新语义;讲清"开新会话而不是逐项清"的理由,
  和 `fresh_session` 的字段分类表(以代码为准,DESIGN 里只写分类原则)。
- server 协议若有变化(第 3 问),同步协议文档与 `thread/cleared` 的说明。
- HANDOFF 记教训(若有)。

## 八、完成记录

✅ 2026-09-24,两个提交:`61dcc8a`(`/clear` 开新会话)、`3c76b1c`(rewind 走同一个缝)。

第五节七问的答案(用户逐条定):

1. 后台工作**全部停掉**,屏幕上报停了几个(`stopped N background task(s)`,超时没收回的另报一行)。
2. 权限:**模式保留、会话里记住的授权清掉、plan mode 退回进入前的模式**;全局/项目层与 approver 沿用
   (`Permissions::fresh_session`)。
3. server **拒绝** `/clear`,回一条 `system` 指向 `thread/start`;`thread/cleared` 通知与投影里的处理删掉。
4. 清屏**连终端 scrollback 一起清**(`ESC[2J` + `ESC[3J`,后者绕过 ratatui 直接进帧缓冲)。
5. worktree:新会话**回启动目录**,旧 worktree 按会话结束的规矩保留在盘上并提示路径。
6. **不加** `/new`。
7. rewind 修复**随本 plan、单独一个提交**。

3.4 的空会话文件不用选:`Rollout` 本来就在 drop 时删掉只有 preamble 的文件(见 HANDOFF 教训 193),
测试钉住了"清空后什么都没说就再清,不留空文件"。

实现要点(设计以 `rust/DESIGN.md` 的 `/clear` 条与 Fork 节为准):

- `Config::fresh_session` 用不带 `..` 的解构逐字段分类;PowerShell 闸门归"沿用"(挡住超时没收回的旧进程)。
- `Scheduler::successor` 共享持久化 store,不共享 owner;`commands::start_fresh_session` /
  `replace_session` 先建新 Config(建不出来就什么都不停),再停旧的。
- TUI 的难点不在 worker:**UI loop 和退出收尾各自攥着第一个会话的 inbox / 权限闸 / Config**
  (教训 194)。改成 worker 把当前 Config 发布到一个共享槽,UI loop 在 `Cleared`/`Forked` 后重读并重新订阅 inbox。
  旧会话后台工作留下的待批准弹窗在切换时丢弃(= 拒绝)。
- 顺带删掉失去调用者的 `TodoRegistry::clear`、`Config::reset_deferred_tool_capabilities`、`DeferredToolUnlocks::clear`。

未验证:没在真终端里看 scrollback 清除的效果,也没接真实 provider 跑。
