# Plan 251 — 会话持久化与恢复

> 2026-10-28 立。来源：全仓只读审查(HANDOFF「〇⁵」节)。
> 状态：待实现。行号是 2026-10-28 的读数，开工先自己量;标「待复核」的条目开工第一步
> 先读代码确认，不成立就在本文件划掉并写明原因。

## 问题

1. **server 新建、还没跑过一轮的会话文件既不清理也读不出来**(已复核前半)。
   `server/src/methods/threads.rs:79-84` 先 `claim_new_session`,再
   `Rollout::new_with_runtime_pending_route`(`core/src/rollout.rs:124-131`),落到
   `with_origin`(`:185`)把 `preexisting` 设成 `path.exists()`——claim 已建好文件，于是为 true,
   Drop 不删。对照 `claimed_with_initial_route` 显式设 `preexisting = false`。
   - 文件里只有一行 runtime;首个 route 要等第一轮 `ensure_initial_provider_route` 才写。
   - 读它报 "provider route timeline is invalid"(`rollout/validate.rs:176` 对空 timeline 返回
     Err,待复核);`thread/list` 静默跳过，且在 `skip/take` 之后才跳(`threads.rs:265`),
     分页会变短;`thread/resume` 报错;这种文件越积越多。
2. **rewind 换路由失败时回退到错误的 route**(疑似)。`core/src/rewind.rs:110-116`:
   `adopt_provider_route` 失败时回退用的是旧会话当前 revision 的 route,分支 timeline 却停在
   cut 点;之后 assistant 消息的 provenance 指向分支里不存在或身份不符的 revision,下次
   resume `validate_provenance` 报错，整个分支读不出来。
3. **`session_digest` 遇非法 UTF-8 整个报错**(待复核)。`core/src/rollout/sessions.rs:488-502`:
   `read_line` 读进 String 返回 InvalidData,`?` 让 digest 失败;CLI picker(`cli/src/args.rs:431-450`)
   给它 `origin: None`、`modified: EPOCH`,sub-agent 会话因此不再被过滤、出现在默认选择器里。
   注释说的是"跳过坏行"。
4. **append 部分失败后继续写会把新行粘到残行上**(观察，待复核)。`Rollout::append_line` 的
   `writeln!` 是两次 write;`intact_lines` 遇到有换行但不是 JSON 的行就停止读取、resume 时截掉
   之后全部内容。现在安全只因所有写路径先 recover、History 在第一次写失败后就丢弃 rollout;
   `append_runtime` 这类把错误交回调用方而 rollout 仍在的路径没有防护。
5. **resume 整文件读两遍、历史复制多份**(优化)。`sessions.rs:62-70`、`164-166`、`641`:
   `resume_session` = `inspect_session` + `recover_for_writing`,后者拿到 lease 后又完整读、
   解析、校验、修复一遍(server `thread_resume` 与 `rewind::fork_here` 同样);`repair_pairing`
   逐条 clone,`inspect_session` 把 `repair.messages.clone()` 放进 snapshot,`ResumedSession`
   同时持有 `messages` 与 `snapshot.messages`。
6. **`--list-sessions` 全量回放;`--continue` 算全部 digest 只取第一个**(优化)。
   `cli/src/args.rs:305-367`、`566-571`:`session_line` 里 `load_session(path)`;
   `resumable_sessions(...).into_iter().next()` 前先 collect 了全部。`resumable_sessions` 的
   注释(`:424-427`)明说过全量回放正是要避免的成本。
7. **`StateLock` 从不删自己的 `.lock`**(观察)。每个会话(含被 Drop 删掉 `.jsonl` 的空会话)
   都在 sessions 目录留下 `{id}.lock`,按扩展名过滤所以无害，但无限累积。

## 开工先问用户

- 第 7 条要不要处理?删 lock 文件有竞态(另一个进程正要 flock 同一路径)。推荐只在 Rollout
  Drop 删掉自己的 `.jsonl` 时顺带删 `.lock`(此时会话从未被他人打开过),其余不动。

## 做法

1. `new_with_runtime_pending_route` 改为接收 `ClaimedSession` 并设 `preexisting = false`;
   解析把"只有 preamble、没有 route"的文件当空会话(或可续写的待定会话)处理，不报错;
   `thread/list` 的跳过发生在分页之前。
2. rewind 换路由失败时回退到分支自己最后一条 receipt 的 route,或直接中止 rewind 并清理
   分支文件(与 plan 249 的守卫一致)。
3. `session_digest` 改用 `read_until(b'\n')` 读 `Vec<u8>`,`from_utf8` 失败就 `continue`。
4. Rollout 加"append 失败后中毒"标志，之后的写入一律拒绝。
5. `resume_session` 先拿 lease 再只 inspect 一次;repair/snapshot 改为 move,`ResumedSession`
   不再持有两份。
6. 列表改用 `session_digest`;`--continue` 改成惰性迭代，找到第一个可用会话就停。
7. 按问答处理 lock 文件。

## 验收

- 单测/集成(旧代码先红):`thread/start` 后断开，不留文件或文件可被 list/resume;
  分页数正确;rewind adopt 失败后分支可 resume;尾部截断多字节字符的会话 digest 仍取到标题
  且 sub-agent 会话照常被过滤;append 失败后再写被拒;resume 只读一次文件(可用计数夹具);
  `--continue` 只 digest 到第一个可用会话。
- 外层执行沙箱之外 `make check`;DESIGN 涉及恢复语义的段落先读再改;HANDOFF 记进展与教训。
