# Plan 33 — 非交互 exec 模式(headless 一次性执行)

> 一句话定位:kloop 的入口只有 TUI(默认)/`--plain`(REPL)/`--serve`/`--mock` 四路
> (`cli/src/args.rs` `CliArgs`),没有"给一个 prompt、跑完退出"的 headless 形态——脚
> 本、管道、CI 都用不了。cc `-p/--print` 与 codex 整个 `exec` crate 双家独立收敛
> (教训 14 强信号),补上。

## 回源结论(2026-07-15,两家真读,file:line)

- **claude-code**:`-p/--print`(`main.tsx:1175`),非交互主循环 `cli/print.ts`
  `runHeadless`(:458)。prompt = 位置参数 + stdin 换行拼接(`main.tsx:1030-1061`,
  两来源可并用);`--output-format text|json|stream-json`(`main.tsx:1189`;text=最后
  一条 result、json=单条 result 对象、stream-json=实时 NDJSON 且要求 `--verbose`,
  `print.ts:800-806/930-973`)。**审批不可用即 deny**(`permissions.ts:953-972`),
  放宽要显式 flag(`--dangerously-skip-permissions`/`--permission-mode`);退出码 =
  `lastMessage.is_error ? 1 : 0`(`print.ts:995-997`);护栏 `--max-turns`/
  `--max-budget-usd`(`main.tsx:1246/1254`)。
- **codex**:独立 `exec` crate。prompt = 位置参数,缺省或 `-` 读 stdin;管道 stdin
  与 prompt 并存时 stdin 作 `<stdin>` 块追加(`exec/src/cli.rs:98-102`,
  `lib.rs:2100-2137`)。`--json` = JSONL 事件流打 stdout(`ThreadEvent` tag=`type`:
  thread.started / turn.started / turn.completed / turn.failed / item.started /
  item.updated / item.completed / error,`exec_events.rs:11-37`),human 输出全走
  **stderr**(`event_processor_with_human_output.rs`);`--output-last-message FILE`
  单取最后消息(`cli.rs:89-96`)。**审批默认 `AskForApproval::Never`**、运行中审批请
  求一律拒(`lib.rs:439/1922-1945`),`--yolo` 绕过;退出码:`error_seen`(含
  Interrupted)→ 1(`lib.rs:1150-1152`)。
- **收敛点(照抄)**:① prompt 双来源(位置参数 + stdin 管道);② 双输出契约——人读
  文本与机器读 JSON 行事件流分开;③ **headless 下审批默认拒绝**而非默认放行,放宽走
  显式 flag;④ 最后消息可单独取用(text 输出/`--output-last-message`);⑤ 退出码
  0/1 按错误与中断。
- **分歧(不必抄)**:cc 的 `--max-budget-usd`/`--permission-prompt-tool` 委托、
  codex 的 `--output-schema`/`--goal` 自动续跑。

## kloop 现状与落点

- `cli/src/args.rs`:`CliArgs` 手写解析,无位置参数;`plain_main` 是阻塞读 REPL。
- 落点:headless = plain 前端的非 REPL 变体——组装 Config → 记 user 消息 → 跑一个
  `run_turn`(截断续跑/压缩照常)→ 打印结果 → 退出。**不挂 Approver**:权限门询问层
  无 approver = 拒绝(对齐 server "回复丢失/EOF = deny" 先例;开工核这条路径的现状),
  `--yolo`/`--accept-edits`/`AGENT_ALLOW` 现成放宽口。
- **kloop 红利**:机器读事件流不用新造 schema——server 模式已定义整套通知 wire 形状
  (turn/started、text/delta、tool/started|completed、turn/completed、system…),
  `--json` 直接复用同形(一套 wire 两处用);`--mock` + headless = CI 可跑的
  hermetic 端到端(现在 `--mock` 走 plain 仍要 stdin)。

## 关键决定(开工时定 / 问用户)

1. **flag 形态**:位置参数 `kloop "prompt"`(codex 形,手写解析容易)vs `-p`
   flag(cc 形)。倾向位置参数 + 无参时报错引导;stdin 管道支持一并定。
2. **`--json` 事件契约**:复用 server 通知形状(倾向,零新 schema)vs 新定 exec 事件
   面(codex 形)。复用时 threadId 字段留空还是去掉,开工定。
3. **会话落盘**:headless 跑完的会话进不进 `.kloop/sessions/`(可 `--resume` 续)。
   倾向进(与 plain 一致,append-only 白送)。
4. **`--max-turns` 护栏**:要不要首片带上(cc 有;防脚本里失控)。倾向带,便宜。

## 不做(挂账)

`--permission-prompt-tool` 委托(server 模式已覆盖程序化审批)、`--input-format
stream-json`(同因)、budget/goal 护栏、`--output-schema` 结构化输出、resume/review
子命令(`--resume <id>` 与位置参数组合即可,不另起子命令)。

## 测试

`--mock` headless:跑通出结果、退出码 0;错误路径退出码 1;`--json` 行事件契约(与
server 通知形状对齐断言);stdin 管道 prompt;位置参数 + stdin 并用;无 prompt 报错;
无 approver 时询问类调用被拒(权限门 fail-safe)。真 key 验一次单命令任务闭环。

## 完成标准

fmt/clippy/test 全绿,一次 commit;README 补 headless 用法;本文件补完成记录;HANDOFF
补能力条目与教训。

## 完成记录(2026-07-16,提交 a33864a)✅

**落点**:`cli/src/headless.rs`(新)+ `cli/src/args.rs`(`-p`/`--headless`/`--json`/
`--max-rounds`/位置参数解析)+ `cli/src/main.rs`(headless 分发 + `read_stdin_if_piped`)。

**四个开工点最终定法**:
1. **flag 形态 = B(cc 形模式开关)**——开关(不是值容器)进 headless,prompt 走位置参数
   + stdin;`kloop` 无参仍进 TUI(不引入"TUI 初始 prompt"新特性)。位置参数/`--json`/
   `--max-rounds` 都是开关-only(无开关报错引导),保持模式 flag 内聚。**flag 名不跟 cc:**
   用户复盘"`--print` 不直观"(cc 从"打印结果就退"这个输出行为命名,非模式命名),定名
   **`--headless`**(自解释)+ 短选项 `-p`,**去掉 `--print`**(连兼容别名都不留,免误导)。
   同轮补 cc 的 `-c`(=`--continue`)/`-r`(=`--resume`)短选项对齐 muscle memory。
2. **`--json` = 复用 server 通知 wire**——method + params 形状逐字节照抄 server 的
   `ThreadUi`(text/delta、tool/started|completed、agent/started|completed、
   todo/updated、note),外加 turn/started、turn/completed 生命周期;**threadId 保留
   并填会话 id**(去掉会破坏 server 客户端复用)。一套 wire 两处用,零新 schema。
3. **会话落盘 = 进**——headless 复用 `open_history`(默认 New,attach rollout),跑完
   可 `--resume <id>` 续/`--fork`。与 plain 一致。
4. **`--max-rounds` = 带**——覆盖 `cfg.max_rounds`,仅 headless。

**两个契约(两家收敛)**:① prompt 双来源——位置参数 + stdin,`assemble_prompt` 纯函数
(都有则"位置\n stdin",都无报错);② 输出分离——text 模式 final_text 一次性进 stdout
(`$(kloop -p …)` 干净捕获)、进度 note/tool 进 stderr、流式 delta 抑制不重复;`--json`
全事件进 stdout。③ **审批默认拒**——headless 挂 `DenyApprover`(confirm 恒 Deny,对齐
server "回复丢失=deny"),`--permission-mode accept-edits|bypass`/`KLOOP_ALLOW` 在 approver 之前放宽。
④ 退出码——Completed=0,其余(MaxRounds/Aborted/Error)=1(`main` 返回 `ExitCode`,
Drop 正常跑,不用 `process::exit`)。

**测试**:cli 43→(headless 6 测:assemble_prompt 组合/exit_code 映射/DenyApprover 拒/
JsonUi 逐行对齐 server wire/text 模式 final+exit0/json 模式生命周期括起 turn;args 新增
headless flag 解析 + 校验错误)。真 key(anthropic 轨)闭环验过:text 位置参数出净结果
exit0、json 走 stdin+read_file 工具事件流(turn/started→tool→text/delta→turn/completed)
exit0、`--max-rounds 2` bound 的 bash 任务(sandbox auto_allow 放行,未触 deny)出结果。
`--mock -p`/`--mock -p --json` 无 key hermetic 端到端跑通。

**偏离/取舍**:
- **退出码 MaxRounds=1**(非 0):护栏触发 = 任务未自然完成,脚本应视为失败;cc 达 max-turns
  也产 error result、codex error_seen→1,两家都是 1。
- **text 模式不流式 stdout**:只在结束打 final_text,保证 stdout 契约=最后消息(cc `-p`
  text 同款);进度看 stderr。
- **deny 路径真 key 未单独触**:sandbox auto_allow 把安全 bash 放行到 approver 之前,
  安排一个"能到 ask 层"的命令代价大且烧 token;DenyApprover 恒 Deny 已单测,机制与 server
  "回复丢失=deny"同缝,足够。

**挂账(不做,plan 已列)**:`--permission-prompt-tool` 委托、`--input-format stream-json`、
budget/goal 护栏、`--output-schema`、`--output-last-message FILE`(text 输出即最后消息,
已覆盖)。

**后续 flag 命名复盘(同会话,用户逐点定)**:① headless 开关 `--print`→`--headless`(留
`-p`,去 `--print` 别名);② 补 cc 的 `-c`(=`--continue`)/`-r`(=`--resume`)短选项;
③ **权限 flag 统一**——查最新 cc(claude-code-guide 查官方 docs.claude.com)确认 cc 用单一
`--permission-mode <mode>` 控全部权限模式(default/acceptEdits/plan/auto/dontAsk/
bypassPermissions/manual),`--dangerously-skip-permissions` = `bypassPermissions` 快捷。
kloop 照此**删 `--yolo`/`--accept-edits` 两个独立 flag,合并成 `--permission-mode
default|accept-edits|bypass`**(取 kloop 内部 `Mode` 三态,kebab 取值),`build_permissions`
直接读 `args.permission_mode`;`plan` 档留给 plan 37 加第四取值。README/HANDOFF/permissions.rs
注释同步。④ **`--max-turns`→`--max-rounds`**——kloop 内部一次 headless = 一 turn = 多
round(round = 一次 sample + 工具),这个数覆盖的是 `cfg.max_rounds`,叫 `--max-rounds`
才对齐内部术语(cc 叫 turns 是它的 turn≈kloop 的 round,不跟)。⑤ 补 **`-h`/`--help`**
(`args::help_text()` 手维护 usage,`CliArgs.help`)。⑥ **env 前缀 `AGENT_`→`KLOOP_`**——
产品叫 kloop,自有 env 却用 `AGENT_` 遗留前缀;全部改 `KLOOP_*`(`KLOOP_MODEL`/`PROVIDER`/
`ALLOW`/`DENY`/`ASK`/`CACHE`/`THINKING`/`EFFORT`/`CONTEXT_WINDOW`/`FALLBACK_MODEL`/
`DEFER_THRESHOLD`/`SANDBOX`/`PROGRAM_*`),第三方原生 `ANTHROPIC_*`/`OPENAI_*`/`TAVILY_*`
不动;内部 `AGENT_SEQ` static 非 env 保留。memory `anthropic-429-fallback` 同步。⑦ **`--json`
保留(不改名)**——评估过它语义偏窄(是 NDJSON 事件流,非 cc 的单 result 对象;cc 分
`json`/`stream-json`),但 kloop 只做流式、无歧义对象,保留简洁名 + 文档已注明"是事件流"。
**命名复盘至此收尾**,CLI 定型:`-p`/`--headless`、`-c`/`-r`、`--permission-mode
default|accept-edits|bypass`、`--json`、`--max-rounds`、`-h`/`--help`,env 统一 `KLOOP_*`。
