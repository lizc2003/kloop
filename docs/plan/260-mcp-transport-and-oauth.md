# Plan 260 — MCP 传输与 OAuth

> 2026-10-28 立。来源：全仓只读审查(HANDOFF「〇⁵」节)。
> 状态：✅ 已完成（2026-11-13）。行号是 2026-10-28 的读数，开工时已按函数重核。
> 9 条全部成立，无划掉条目；第 5、9 条按用户拍板：`tools/call` 只重试“未处理”，
> stdio 改用共用的 `process_tree`（第二个提交）。

## 问题

1. **Windows 上用 `cmd /C start` 打开授权页**(已复核)。`cli/src/mcp_auth.rs:220-237`
   `("cmd", vec!["/C", "start", "", url])`:std 的 `Command` 只给含空白的参数加引号、不转义 `&`;
   `build_authorize_url`(`mcp/src/oauth.rs:333-343`)用 `&` 拼了 6 个以上参数，浏览器只收到
   `…?response_type=code`,自动打开的登录必然失败;`&` 之后的片段被 cmd 当命令执行，而
   `authorization_endpoint` 来自远端 AS 的 metadata。
2. **stdio 写请求不在超时范围内，并锁死后续请求**(待复核)。`mcp/src/stdio.rs:179`(及 `:225`、
   read_loop 里的 refusal 写入):先 `write_line(&self.writer, &msg).await`,之后才
   `timeout(timeout, rx)`。子进程不读 stdin 时，超过管道缓冲(约 64KB)的 tools/call 永久阻塞，
   持有的 `tokio::Mutex` 写锁挡住后续所有请求;服务端此时发来带 id 的请求，read_loop 也要拿这把锁
   写 -32601,于是停止读 stdout,形成管道互锁。
3. **401 之后的 token 刷新没有超时**(待复核)。`mcp/src/http.rs:452` `refresh_after_401` 在
   `post_once` 的 timeout 之外;`oauth.rs` 里所有 Client 都是 `reqwest::Client::new()`(无默认超时),
   `http.rs:115` 同样。token endpoint 接受连接不响应时 tools/call 永久挂起，期间持有 `refresh_lock`
   与 `session_gate` 读锁(挡住 reinitialize)。`login`(`oauth.rs:498`)的发现与换码请求同样。
4. **一次临时 5xx/429 把 `needs_relogin` 永久置真**(待复核)。`oauth.rs:674`、`692`、`716` 三处
   `store(true)`,成功路径从不复位;`cli/src/mcp/tools.rs:339-343`、`470-478` 因此把之后任何普通
   错误判成认证失败。
5. **不幂等的 `tools/call` 在 5xx/传输错误后自动重放**(疑似是否接受)。`http.rs:214`、`276`、`307`:
   请求已送达并执行但网关回 502/504、或读响应头时连接被重置，`post_with_retry` 按 250ms、1s 再重放
   两次，"创建 issue""发消息"最多执行 3 次。DESIGN 约 1967 行对 404 明确不重放原操作，5xx 与网络
   错误没区分幂等。
6. **OAuth 回调串行 accept**(疑似)。`oauth.rs:352-400`:一个连上后不发请求行的连接(浏览器预连接、
   本地端口扫描)让 `read_request_line` 卡到 300s;任何带错误 state 或 `error=` 的本地请求直接中止登录。
7. **SSE 每次 feed 从头扫分隔符**(优化)。`mcp/src/sse.rs:21`、`41`:约 8MiB 的单帧按 16KB 分块到达，
   约 512 次 feed、每次平均扫 4MB。
8. **`read_sse_message` 的 8MiB 上限按整条流累计**(观察)。keep-alive 与 progress 通知也算在内，
   长时间运行、频繁推进度的工具调用理论上会撞上限，与注释"keep-alive 不会迫使缓冲"不完全一致。
9. **stdio 子进程只杀直接子进程**(观察)。`kill_on_drop`/`start_kill`:`npx` 这类再起孙进程的命令
   退出时可能留下孤儿;TUI 退出时中止连接任务也依赖这个机制。

## 开工先问用户（已定）

- 第 5 条：用户同意。`tools/call` 只重试 `is_connect()` 与 429/503；500/502/504 等等不再重放，
  其它幂等方法维持原重试表。
- 第 9 条：用户选择“共用”。`core/src/process_tree/` 整体搬进 `kloop-process-spawn`，
  core 与 MCP stdio 共用同一套整树终止；Windows 补 stdin 管道分支，只能交叉编译验证。

## 做法

1. Windows 改用 `ShellExecuteW`(windows-sys 需加 `Win32_UI_Shell` feature)或
   `rundll32 url.dll,FileProtocolHandler <url>`,不经过 cmd 解析;开工比较两者并记下选择。
2. 同一个 deadline 包住写入与等待响应;写超时 `close_local(WriteFailed)`;read_loop 的 refusal 改用
   `try_lock` 或单独发送队列。
3. OAuth 与 http 的 Client 设 `.timeout()` 与 `.connect_timeout()`,或把 refresh 放进请求剩余的 deadline。
4. 只对 400/401 且 `invalid_grant` 这类错误置 relogin;刷新成功后 `store(false)`。
5. 按问答处理重放与孙进程。
6. 单个回调连接读取加短超时;state 不匹配时忽略该请求继续等，而不是 bail。
7. SSE 记录已扫描偏移，从 `scanned.saturating_sub(3)` 接着扫。
8. 上限只计 data 字节，或在 DESIGN 写明现状。

## 验收

- 单测/集成(旧代码先红):授权 URL 生成的 Windows 命令行不含未转义的 `&`(单测命令构造;
  交叉编译通过，未在 Windows 执行要照实写);不读 stdin 的假 server 上大请求按时超时、后续请求
  不被锁;不响应的 token endpoint 让刷新按时失败;503 后刷新成功 `needs_relogin` 复位;tools/call
  遇 502 不重放;回调端口上一个哑连接不阻塞真实回调。
- DESIGN 约 1967 行先读再改;外层执行沙箱之外 `make check`;HANDOFF 记进展与教训。

## 完成记录（2026-11-13）

两个提交：

1. **第 1–8 条**（`fix(plan260): bound MCP writes, OAuth requests and SSE scans`）
   - 第 1 条：Windows 改用 `rundll32 url.dll,FileProtocolHandler <url>`（不经 cmd 解析，
     不需要新 windows-sys feature）；`browser_command(platform, url)` 可单测，断言整条
     带 `&` 的授权 URL 是一个 argv 元素。
   - 第 2 条：`write_line_before` 用 `timeout_at(deadline)` 把“排队拿写锁 + 写 + flush”
     放进与响应同一个 deadline；notify 用 `HANDSHAKE_TIMEOUT`；read_loop 的 -32601 拒绝
     改 `try_lock`（拿不到就放弃这条拒绝），写本身也带 5s 上限。回归：64 字节 duplex 上
     64KiB 请求 100ms 超时、health=Closed(WriteFailed)、后续请求立即失败；锁被占用时
     `write_refusal` 立刻返回 None。
   - 第 3 条：OAuth 统一 `oauth_http_client()`（connect 10s、整请求 30s），login 的发现/
     注册/换码与 request-time refresh 共用；MCP HTTP client 加 10s connect 预算。
     回归：只 accept 不应答的 token endpoint 在 200ms 客户端预算内失败，且不置 relogin。
   - 第 4 条：`refresh_failure_is_definitive`——只有 400/401 且 error 为
     invalid_grant/invalid_client/unauthorized_client 才置 `needs_relogin`；5xx/429/非 JSON
     视为瞬时；刷新成功 `store(false)`。回归：503 → 不置位；成功后清位。
   - 第 5 条：`Attempt::NotProcessed`（connect 失败、429/503）与 `MaybeProcessed`（其余
     408/5xx、读失败）分开，`Replay::OnlyUnprocessed` 给 `tools/call`。回归：502 只请求
     一次；503 重试后成功；其它方法保持旧行为。
   - 第 6 条：回调每个连接 5s 租约，state 不匹配 / 无 state 的 `error=` / 无 code 一律
     忽略继续等，匹配的 `error=` 才终止。回归：错误 state 后仍能完成登录；哑连接不挡住
     真回调；匹配的 error redirect 仍报错。
   - 第 7 条：`SseParser` 记录 `scanned`，从 `scanned-3` 继续扫；回归用 `examined` 计数
     钉住 1MiB/64 块只扫约一遍。
   - 第 8 条：SSE 上限改计 data 字节 + 未完成帧大小；回归：8MiB+ keep-alive 注释后仍能
     收到响应。
2. **第 9 条**（`fix(plan260): share the process tree with MCP stdio`）
   - `core/src/process_tree/` 搬进 `kloop-process-spawn`（`spawn_process_tree`，公共类型
     `ProcessSpec/ProcessStdio/ProcessTreeChild/Killer/Exit/Pipe/Writer`），core 只改 `use`。
   - Unix 侧加 `ProcessStdio::Pipe` stdin 与 `take_stdin`；Windows 补 `prepare_input`
     （CreatePipe，读端可继承、写端清除继承标志）并贯通普通与 debug 两条 spawn 路径。
   - `resolve_program`：Windows 的 `CreateProcessW` 显式 application path 不查 PATH，
     按 cwd → PATH → `.exe` 解析，保持 `command = ["npx", …]` 可用；Unix 原样。
   - MCP stdio 改用它起子进程；abort 用 `killer().terminate()`，shutdown 用
     `terminate_and_wait(5s)`。
   - 回归：piped stdin 往返；root 退出后后台孙进程仍在、`cleanup_after_exit` 收掉整组。

复审修正（同日，第一轮四条）：(1) 读循环拒绝写超时不再当成功——`write_all` 被取消可能已在管道
里留下半帧，下一条消息会粘上去；现在返回错误让传输关闭，并用“先写 8 字节再卡住”的 writer
钉住。(2) Windows 批处理入口：`CreateProcessW` 会把 `.bat`/`.cmd` 交给 `cmd.exe`，而 cmd 有
第二遍解析（`%VAR%` 展开、`^` 转义、引号规则），std 为此专门转义；新 `process_tree/batch.rs`
按同样的规则自己构造 `cmd.exe /e:ON /v:OFF /d /c "<script>" …`，普通与 debug 两条 spawn 路径
都走它，纯 u16 函数在本机单测（空格/引号/尾反斜杠/百分号/非法脚本名）。(3) 相对可执行路径
带分隔符时按子进程 cwd 补成绝对路径，不再被“必须绝对”拒绝。(4) `resolve_program` 接收
子进程实际会拿到的 PATH（配置 env 里的 PATH 覆盖继承值），先查 cwd 再查该 PATH。另修
`make check-windows`：给三条只在 unix/macOS 测试里用到的 helper 补 `cfg`，整仓 msvc 目标
clippy 现在全绿（仍未在 Windows 实跑）。

第二轮复审两条：批处理脚本路径先按 std 的规则把 `\\?\C:\…` 与 `\\?\UNC\…` 转成
cmd 能懂的写法（`to_user_script_path`，其余形式原样保留），并在拼接前拒绝脚本路径里的 NUL
（否则命令在 NUL 处截断、参数一起丢失）；两条都有单测。第三轮补上转前缀前的等价检查：
候选路径先过 `GetFullPathNameW`，规范化结果与候选项不一致（例如 `\\?\C:\tools.\server.cmd`
里的 `tools.` 会被 Win32 去掉末尾点）就保留原 verbatim 路径、让 cmd 明确失败，而不是执行
另一份脚本；判定逻辑接一个可注入的规范化函数，本机用模拟 Win32 规则的假函数钉住。

验证：`cargo test -p kloop-mcp`（43 lib + 14 集成）、`cargo test -p kloop-process-spawn`
（15 项，含 piped stdin、孙进程收尾、批处理转义与路径解析）、`cargo test -p kloop-core --lib`、`cargo test -p kloop --bin kloop mcp_auth`
（10 项）、`cargo clippy -p kloop-mcp -p kloop --all-targets -D warnings`、`cargo fmt --check`。
Windows：`cargo xwin clippy --target x86_64-pc-windows-msvc -p kloop-process-spawn -p kloop-mcp
--all-targets -D warnings` 与 `-p kloop-core --lib` 通过；整仓 `make check-windows` 仍被
core 测试目标里三条既有 dead-code（`PathFacts::gather`、`skip_nested_seatbelt_test`、
`Builtin::concurrency_safe`，都与本计划无关）挡住，未在 Windows 上实际运行。
未跑全量 `make check`、parity 与真实 API。
