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
