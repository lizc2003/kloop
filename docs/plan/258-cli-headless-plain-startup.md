# Plan 258 — CLI 前端：headless 输出、plain 输入与启动

> 2026-10-28 立。来源：全仓只读审查(HANDOFF「〇⁵」节)。
> 状态：✅ 完成(2026-11-13)。行号是 2026-10-28 的读数;开工时 7 条全部复核成立。
> 开工问过用户：第 1 条按 (a)(headless 不带 id 的 `--resume` 在 `parse_args` 拒绝);
> 第 5 条按 (a)(错误映射成不含敏感信息的类别写进 warn，不建私有日志)。
> 会话驱动的统一(四个前端各写一份 turn 生命周期)在 plan 269,这里只修具体 bug。

## 问题

1. **headless 的 stdout 被提示文本和选择列表污染**(已复核)。plan 241 记过的待修问题仍在，
   而且范围更大。`cli/src/args.rs`:
   - `:580-586` `[forked …]` 与 `:593-596` `[resumed session …]` 用 `println!`,
     `--headless --json -c/--resume/--fork` 的事件流开头混入普通文本;文本模式
     `out=$(kloop --headless -c "q")` 也捕获到它，违背 `headless.rs:122` "stdout 只有最终答案";
   - `:512-529` `pick_session` 只有 stdin、stdout 都是终端时走 TUI 选择器，否则 `println!` 列表 +
     `stdin().read_line`:`echo "fix bug" | kloop --resume --headless` 把管道里的提示词当选号，
     报 `invalid selection 'fix bug'`;stdin 是终端、stdout 接 JSON 消费者时列表进了管道、进程
     阻塞等输入。
2. **plain REPL 两个 stdin 读者竞争**(已复核)。`cli/src/plain.rs:112-177`:`next_line()` 一被 poll
   就 `requests.send(())`,读线程阻塞在 `read_line`(持有 Stdin 锁);`select!` 选中 Inbox 分支时
   丢弃这个 future 直接 `run_turn`(`:283` 附近)。这一轮要审批时，`CliApprover`
   (`ui.rs:86-89`、`245-248`)的 `spawn_blocking(read_line)` 排队等锁;用户输入 `y` 被 REPL 线程
   读走、进了 unbounded channel,下一次 `next_line()` 把它当用户消息发给模型。每次 Inbox 唤醒
   还会多积压一个读请求。`ui.rs:52-55` 注释"REPL 的 stdin reader 在一轮运行期间空闲"不成立。
3. **`--plain` 在信任提问之前就连接 MCP**(待复核)。`main.rs:133-138`、`369-376`:
   `if args.serve || args.plain || args.headless { mcp::connect_servers(...) }` 在 `run_front_end`
   之前;未信任目录里 stdio 子进程已以该目录为 cwd 启动(`npx` 会读项目 `.npmrc`),用户选 No 时
   已经跑过了。TUI 走延后连接，没有这个问题。
4. **启动期外部输入触发 panic**(待复核)。`cli/src/user_config.rs:74-92` `[env]` 名字含 `=`/`\0`
   或值含 `\u0000` 时 `set_var` panic,panic 信息带出 `{value:?}`(可能是代理凭据),也破坏了
   "配置坏了 `--list-sessions` 仍可用";`main.rs:279`/`283`/`294` 用 `std::env::args()`,非 UTF-8
   参数直接 panic;`startup.rs:823-824` `[program] memory_mb` 巨大值时 `as usize * 1024 * 1024`
   溢出(debug panic、release 回绕)。
5. **MCP 启动失败原因被整个丢掉**(已确认，部分刻意)。`cli/src/mcp.rs:421-444`:`Err(error)` 只用于
   `classify_startup_failure`,提示写着 "see engine log",全仓没有这个日志。DESIGN 规定不回传原始
   错误链(脱敏),但"看日志"是假的。
6. **MCP 资源按名查找不优先精确匹配**(待复核)。`cli/src/mcp/resources.rs:115-121`
   `find(精确 || 归一化)`:同时有 `Foo` 与 `foo`(`qualified_name_component` 保留大小写，
   `config.rs:134` 的冲突检查不拦)时，请求 `foo` 可能读到 `Foo` 的资源，审批界面却显示 `foo`。
7. **`merge_builtins` 与 `merge_commands` 函数体逐字相同**(重复)。`startup.rs:610-618`、`670-678`;
   `skills_from_roots`(`:725`)与 `commands_from_roots`(`:684`)遍历骨架相同。

## 开工先问用户

- 第 1 条 headless + 不带 id 的 `--resume`:(a) `parse_args` 阶段直接拒绝(要求带 id);
  (b) 回退列表与提问走 stderr、仍从 stdin 读。推荐 (a)——headless 的 stdin 是给提示词的。
- 第 5 条：(a) 把错误映射成不含敏感信息的类别(spawn 失败/超时/HTTP 状态码/协议错误)写进
  warn;(b) 写进私有日志文件。推荐 (a)。

## 做法

1. 两条提示改 `eprintln!`;按问答处理 headless Pick。
2. 审批与提问改走 `PlainInputThread` 同一通道(单一读者);Inbox 分支前不发读请求，或在
   分支开始前取消未完成的读请求;改 `ui.rs` 注释。
3. 信任判定挪到 `ProcessState::load` 之前，或 plain 像 TUI 一样信任通过后再连接。
4. `load_env_overrides` 校验名字与值(拒绝 `=`、`\0`,报错不带值);argv 改 `args_os()` 并给
   正常错误;内存/栈换算用 `checked_mul`。
5. 按问答处理 MCP 启动失败，并修正那句提示。
6. 先精确匹配;只有归一化结果唯一时才走模糊回退。
7. 合并成 `merge_first_wins(primary, extra)`;两个 `*_from_roots` 共用遍历骨架。

## 验收

- 集成(旧代码先红):`--headless --json -c` 的 stdout 每行都是 JSON;headless 不带 id 的
  `--resume` 按问答处理;plain 下 Inbox 唤醒的一轮里审批回答被审批收到、不进下一条消息;
  未信任目录 `--plain` 选 No 时没有 MCP 子进程启动过(用会写标记文件的假 server);坏 `[env]` 与
  非 UTF-8 argv 报错不 panic 且错误里不含值;`Foo`/`foo` 两个 server 各读各的。
- 外层执行沙箱之外 `make check`(含 plain PTY 用例);HANDOFF 删掉 plan 241 那条待修记录并记进展。

## 完成记录(2026-11-13)

- 1 `[forked …]` 与 `[resumed session …]` 改走 stderr；headless 下不带 id 的
  `--resume` 在 `parse_args` 直接拒绝（提示改用 `-c`），`-c`/`--fork` 不变。回归：
  `--mock --headless --json -c` 的 stdout 每行都是 NDJSON、resume 提示在 stderr；
  首次运行写真实会话文件再 `-c`，不手写 route 夹具。
- 2 plain 前端只有一个 stdin 读者：新增 `PlainStdin`（容量 1 的队列，线程持续读行），
  REPL、审批、提问都从同一句柄取行；不再有“先发读请求、Inbox 胜出后丢弃”的路径。
  `prompt_questions` 改成 async 读同一队列，删掉 `spawn_blocking` 与
  `read_terminal_line`。回归钉住：Inbox 唤醒后审批拿到 `y`，REPL 下一条消息读不到它；
  原调度器唤醒回归改用真实 `next_plain_input`。
- 3 `--plain` 的 MCP 服务器改为先 plan、信任通过后再 `connect_deferred_mcp()`；
  serve/headless 仍立即连接（都不问信任）。PTY 回归：未信任目录配置一个会写标记文件的
  stdio server，回答 No 后标记文件不存在。
- 4 `[env]` 拒绝名字含 `=`/NUL、值含 NUL（错误不回显值）；argv 改 `args_os()`
  并对非 UTF-8 给正常错误；`memory_mb`/`stack_kb` 用 checked 乘法，溢出报配置错误。
- 5 MCP 启动失败映射成单一类别（进程启动失败/超时/`HTTP <status>`/认证被拒/协议或传输
  错误）写进 warn 与状态消息，去掉不存在的 “see engine log”；原始错误链仍不外露。
- 6 资源工具的 server 查找先精确匹配；归一化回退只在唯一命中时使用，`Foo`/`foo`
  各读各的。
- 7 `merge_builtins`/`merge_commands` 合并为 `merge_first_wins`；两个 `*_from_roots`
  共用 `entries_from_roots` 的 read_dir + 排序骨架。

验证：`cargo fmt --all -- --check`、`cargo check -p kloop --all-targets`、
`cargo clippy -p kloop --all-targets -- -D warnings`、`cargo test -p kloop --bin kloop`（167 项）、`--test headless_contract`
（7 项）、`--test plain_pty`（9 项，含新 PTY 回归）通过；未跑全量 `make check`。
