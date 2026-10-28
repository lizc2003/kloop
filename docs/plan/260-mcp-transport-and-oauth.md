# Plan 260 — MCP 传输与 OAuth

> 2026-10-28 立。来源：全仓只读审查(HANDOFF「〇⁵」节)。
> 状态：待实现。行号是 2026-10-28 的读数，开工先自己量;标「待复核」的条目开工第一步
> 先读代码确认，不成立就在本文件划掉并写明原因。

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

## 开工先问用户

- 第 5 条：`tools/call` 只重试 `is_connect()` 与 429/503 这类明确"未处理"的情况，其余直接把失败
  交给模型——要不要?推荐要;其它幂等方法(`tools/list`、`resources/read`)维持现状。
- 第 9 条要不要改用 `process_tree` 起 stdio server(整棵树终止)?推荐要，但单独一个提交，Windows
  只能交叉编译验证(教训 183/201)。

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
