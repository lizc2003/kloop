# Plan 251 — 会话持久化与恢复

> 2026-10-28 立。来源：全仓只读审查(HANDOFF「〇⁵」节)。
> 状态：✅ 已完成(2026-11-03)。原行号是 2026-10-28 的读数；复核结果与偏差见文末「完成」。

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

## 完成(2026-11-03)

- **第 1 条**：复核发现「首个 route 要等第一轮才写」已不成立——`thread/start` 在
  `spawn_thread` 里就 `ensure_initial_provider_route`，成功建起的线程文件是
  runtime+route、0 消息。其余仍成立并已修：claim 建好的文件被 `path.exists()` 判成
  别人的，空线程退出不删；「只有 preamble、没有 route」的文件（硬杀）被空 timeline
  判成损坏。做法：`new_with_runtime_pending_route` 改收 `ClaimedSession` 并显式
  `preexisting = false`；空 timeline 只在没有消息/usage 时视为空会话；`thread/list`
  先筛可读再分页（游标指向下一个未读路径）。另补了一条 fail-closed：整份文件连首行
  都不完整的仍然报错，不能被当成空会话再被恢复截断。
- **第 2 条**：adoption 失败回退到分支 timeline 最后一条 receipt（经
  `ProviderCatalog::restore_route`），并记录 note；不再回退到旧会话的当前 route。
- **第 3 条**：`session_digest` 改 `read_until` + `from_utf8`，非法 UTF-8 行跳过。
- **第 4 条**：`Rollout` 增加 `poisoned`，任何 append 失败后拒绝后续写入（权限恢复
  之后也一样）；下一次显式 resume 会先截断残行。
- **第 5 条**：新增 `inspect_session_for_writing`：先取租约、再读一次；
  `recover_for_writing` 只接受带租约的读。`PairingRepair` 不再复制
  messages/terminals，`ResumedSession` 只持有 snapshot 一份，`resume_options` 直接
  借用快照。回归用按路径的读取计数断言 resume 只读一次。
- **第 6 条**：`--list-sessions` 改走 `session_digest`；`--continue` 用惰性的
  `resumable_rows(..).next()`，不再先 digest 全部。注意：列表不再报消息数——plan 162
  曾专门为诊断视图保留它；251 的指令是改用 digest，本计划按 251 执行。
- **第 7 条**：先按推荐落了「只在 Drop 删除空 `.jsonl` 时顺带删 `.lock`」；随后按用户
  「不用考虑兼容性」的要求去掉旁路锁：**会话文件本身就是锁**。writer 在生命周期内持有
  transcript 的独占 flock，恢复用 `try_acquire_existing` 先锁再读；空会话 Drop 先释放
  再 unlink。`StateLock` 只保留给 `project.lock`/`mcp-oauth.lock` 这类没有数据文件可
  依附的锁。不再产生任何 `.lock` 文件。

验证：外层执行沙箱之外 `make check`（fmt + clippy + workspace 全量测试 + parity）。

## 审查跟进(2026-11-03)

- server 的失败路径不再按路径二次删除：线程文件从 claim 起归 writer 所有，`spawn_thread`
  失败时 History 的 Drop 已经删掉空文件；再按路径删一次，会在另一进程同秒复用该 ID 时删掉
  对方的会话。构造失败分支同理。
- 会话锁的 I/O 只走被锁住的那个句柄：读取、截断、追加都用 lease 里的 `File`，不再按路径
  重开；空会话仍在持锁时 unlink（平台拒绝才先释放再重试）。这样即使文件被 unlink、ID 在同
  一秒被复用，也不会出现“锁住旧 inode、却读写新文件”的两个 writer。
- CLI 打开会话改走 `History::adopt_or_initialize_provider_route`，与 server 共用“没有
  timeline 就写 initial route，否则 adopt”的分支；preamble-only 文件现在 CLI 也能 resume。
- 有了常驻 fd 之后，改权限/删父目录不再能让 append 失败，失败注入改成 test-only 的
  `inject_append_failure`；新增“writer 跟随自己的 lease（unlink+复用名字后不会写到新文件）”、
  “无 timeline 的历史初始化 route”、CLI 端到端 resume preamble-only 文件三条回归。
  外层沙箱之外 `make check` 全绿（core 1326、TUI 314、server contract 46、2 项 ignored，
  本机 parity baseline verified）。

## 审查跟进二(2026-11-03)

- Windows 的 `LockFileEx` 是强制锁：writer 锁住 transcript 会让同进程/其他进程的普通
  读取失败，`thread/read`、`thread/list`、fork 都会坏。租约抽成 `SessionLease`：Unix 锁
  transcript（flock 是 advisory，读不受影响，无 sidecar），Windows 锁同名 `.lock`
  sidecar、transcript 保持可读；两个平台的读写/截断仍都走租约持有的 transcript 句柄。
- 抢锁失败的 writer 不再触发删除：`new_subagent_with_route` 先 `create_new` 再抢锁，另一
  进程可在窗口内 resume；失败者的 Drop 现在要求 `lease.is_some()` 才清理，文件归赢家。
  回归：先持租约，再构造 pending-route writer，断言它失败且文件仍在。
  外层沙箱之外 `make check` 全绿（core 1327、TUI 314、server contract 46、2 项 ignored，
  本机 parity baseline verified）。Windows 目标用 `cargo-xwin` 实测 `cargo xwin check
  -p kloop-core --target x86_64-pc-windows-msvc` 通过（原生依赖同编），Windows 运行时未实测。

## 审查跟进三(2026-11-03)

- 空会话清理失败后不再释放租约重试：部分成功后重试会在“ID 同秒复用”的窗口里删掉新会话
  的 transcript。失败就留下空文件；新增 test-only 删除失败注入的回归（第一次失败后文件
  必须仍在，证明没有第二轮删除）。
- Windows-only dead_code（`SessionLease.lock` 的 RAII 字段、`StateLock::file/file_mut`
  的 Unix 专用访问器）已用 `cfg_attr`/`cfg` 收口，并用
  `cargo xwin clippy -p kloop-core -- -D warnings` 复核为零警告。
- 验证：外层沙箱之外 `make check` 全绿（core 1328、TUI 314、server contract 46、2 项
  ignored，本机 parity baseline verified）；`cargo xwin check`/`clippy`（msvc target）通过。
