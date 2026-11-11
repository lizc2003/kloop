# Plan 255 — bash 前台输出、超时语义与 hook 子进程 IO

> 2026-10-28 立。来源：全仓只读审查(HANDOFF「〇⁵」节)。
> 状态：✅ 已完成(2026-11-11)。行号是 2026-10-28 的读数，开工先自己量;标「待复核」的条目开工第一步
> 先读代码确认，不成立就在本文件划掉并写明原因。

## 问题

1. **脱离进程组又持有管道的后代，把成功的命令变成错误**(已复核代码路径;DESIGN 与 HANDOFF
   都没提)。`core/src/tools/bash.rs:399-405` 根 shell 正常退出 → `cleanup_after_exit` 只看
   pgid,认为树已空 → `collect_foreground_output` → `finish_reader`(`:484-497`)等不到 EOF,
   2 秒后 `task.abort()` 并 `bail!("timed out draining output pipe …")`,经 `Ok((stdout?, stderr?))`
   让整个结果变 Err:已读到的 stdout/stderr 与退出码全部丢失，逃逸进程继续活着。典型触发：
   `ssh -f -N -L …`(daemon 会 setsid 但保留 stdio)、`python3 -c 'os.setsid(); …' &`。
   Windows 靠 Job 与调试器把这类后代收进来，两平台不一致。
2. **超时/中断时已读到的输出被丢弃**(已复核代码)。`bash.rs:407-427`:
   `drain_foreground_readers(..).map(|_| ())` 后只报 `timed out after Nms`/`interrupted`,模型看不到
   卡住前打印了什么(比如卡在哪个测试)。
3. **`timeout_ms` 不拒绝 0、也没有上限**(待复核)。`bash.rs:58-59`、`249`、`388`:传 0 时命令先
   spawn,第一次 poll 就被 SIGKILL;PowerShell 的 `timeout_ms()`(`powershell.rs:67-75`)拒绝 0,
   两边不一致。schema(`builtin.rs:668` 附近)没写上下界。
4. **stdout 超过 3 万字符时 stderr 整段消失**(待复核)。`bash.rs:519-538` `format_output` 合并
   stdout 在前再只留头部 30000 字符。"stdout 在前、头部截断"是 DESIGN 约 2612–2619 行写明的取舍，
   这里说的是它的后果：失败原因写在 stderr 时模型只看到截断标记与退出码。
5. **后台输出尾部的 UTF-8 接缝**(待复核)。`tools/bash/background.rs:741-745` `read_tail` 从
   `len-30000` seek,可能落在多字节字符中间，开头出现 U+FFFD;前台 150KB 截断处同理。
6. **`stop_bash` 过早报 Stopped;terminate 失败不重试**(待复核)。`background.rs:122-129`:等满
   5 秒仍是 Stopping 也返回 `Stopped {id}`;`:633-638` `killer.terminate()` 出错后
   `signal_sent = true`,此后不再重试。
7. **轮询与预分配**(优化)。`background.rs:83-92`、`122-128` 的 `bash_output`/`stop_bash` 每 100ms
   查一次 registry,而 `BackgroundShells.activity` watch 已存在;~~`bash.rs:501` 每次前台调用给两个流
   各 `with_capacity(150KB)`~~(开工复核不成立：读取缓冲早已从 `Vec::new()` 起按需增长)。
8. **hook 输出无上限，大 payload 下可能互锁**(上限已确认，互锁疑似)。`core/src/hooks.rs:405-422`:
   注释"The payload is tiny, so write-then-wait cannot deadlock",但 post_tool payload 带完整
   tool_result、pre_tool 带 Write 全文，超过管道缓冲时一个回显输入的 hook(`tee -a log`)会与
   "先写完 stdin 再 wait"互相卡到超时(默认 10s),随后按 fail-open 放行;stdout 全量收进内存，
   经 `agent.rs:990` 原样作为 `Injected::Hook` 写进历史。
9. **PowerShell 与 bash 两套输入校验**(重复)。`powershell.rs:67-90` 手写白名单与 `timeout_ms`,
   bash 用 serde `deny_unknown_fields`——第 3 条的不一致就是这样来的。

## 开工先问用户

- 第 3 条上限取多少?推荐 600000(10 分钟),超过时报错并提示用后台执行。
  **定：600000**(2026-11-11，按推荐)。
- 第 4 条：(a) 两个流分别给预算(如 stderr 至少保留尾部 8KB);(b) 对合并结果按"头 + 尾"截断。
  推荐 (a),保留 DESIGN"stdout 在前"的主体取舍。**定：(a)**，stderr 尾部 8KB。

## 做法

1. 读取任务把字节写进共享缓冲(如 `Arc<Mutex<BoundedStream>>`),排空超时时返回已采集的输出，
   附一行说明("有后代脱离进程组仍持有输出管道"),不再整体报错。
2. 超时/中断分支复用 `collect_foreground_output`,用 `format_output` 渲染已有输出后再追加
   超时/中断说明;PowerShell 同样。
3. `parse_bash_input` 校验 `1..=MAX`;PowerShell 改用 serde 结构体并共用同一校验函数;schema
   写上下界。
4. 按问答改 `format_output`。
5. `read_tail` seek 后跳过开头的续字节(`0b10xxxxxx`);前台截断处同理。
6. 超时后按实际状态报 "stop requested, still stopping";terminate 失败由 watchdog 定期重试。
7. 轮询改为 `activity.subscribe()` + `timeout_at(deadline, changed())`;缓冲从小容量起按需增长。
8. hook:写 stdin 与读输出并发(`tokio::join`);stdout 设上限(如 64KB,附截断说明)后再进
   `context_message`。

## 验收

- 单测/集成(旧代码先红):后台起一个先 `os.setsid()` 再 sleep、且不关 stdio 的后代(macOS 没有
  `setsid` 命令，用 python3 或测试自带的小程序),随后 `echo ok`,结果是 `ok` + 说明、退出码保留
  (Unix,外层执行沙箱之外跑);超时结果里带已打印的内容;`timeout_ms: 0` 与超上限
  被拒，PowerShell 同;大 stdout + stderr 失败原因仍可见;中文输出 tail 首字符不是 U+FFFD;
  stop 超时后的文案;回显 stdin 的 hook 收到 200KB payload 不超时、输出被截断。
- DESIGN 改写约 2612–2619 行与 hooks 一节;外层执行沙箱之外 `make check`;HANDOFF 记进展与教训。

## 完成记录 ✅(2026-11-11)

- **第 1/2 条**：读取任务只负责 `read`，字节写进共享的 `Arc<Mutex<BoundedStream>>`。排空窗口
  到期只中止任务并保留已采字节，结果加一行说明"输出管道仍被脱离进程组的后代持有"，不再把
  成功的命令变成错误；超时分支同样拿回已采输出，`bash`/`PowerShell` 把它连同
  `[timed out after Nms]` 作为错误文本返回。转向中断仍严格返回 `interrupted` 并丢弃部分输出
  ——dispatcher 与历史契约都按这段原文判定。
- **第 3/9 条**：共用 `bounded_timeout_ms`（`1..=600000`）；`parse_bash_input` 只在非 background
  时校验（该字段在后台被忽略），PowerShell 改调同一函数。schema 写 `minimum`/`maximum`。
  外层 deadline 用例改为越界时回落到默认值。PowerShell 仍保留手写字段白名单而非 serde
  `deny_unknown_fields`：后者会丢掉"foreground-only ... 不支持 background"的定向提示。
- **第 4 条**：内联摘录改为 stdout 头 + stderr 尾（至少 8,000 字符，剩余预算继续给 stderr），
  两段都被保留且有截断时加 `[... output omitted ...]` 接缝标记；落盘的仍是有损解码后的全文。
- **第 5 条**：每流 1MiB 截断后用 `truncate_incomplete_utf8` 裁掉不完整序列（两个流也改为
  分别解码，接缝不再互相吞字节）；`read_tail` 跳过开头的续字节，裁掉的字节计入 omitted。
- **第 6 条**：`stop_bash` 用 `stop_report`：窗口内没停下来就报
  `stop requested for <id> (<command>); it is still <state>`；terminate 失败由 watchdog 每 tick
  重试，错误文本保留到最终状态。
- **第 7 条**：两个 waiter 改为 `activity.subscribe()` + `timeout_at(deadline, changed())`，
  醒来重读一次表；预分配一条已过期（见上）。
- **第 8 条**：payload 写入与 stdout/stderr 读取 `join!` 并发；两个流都排空到 EOF、各保留
  64KiB，超出部分附 `[hook output truncated: N bytes omitted]`。
- **回归**（修复前先红）：setsid 后代持管道仍返回 `ok` + 说明 + 退出码；超时结果带已打印内容；
  `timeout_ms` 0/越界被拒且未 spawn；大 stdout 时 stderr 失败原因可见；1MiB 截断处不出现
  U+FFFD（手工禁用裁切验证过先红）；后台尾部首字符不是 U+FFFD；stop 未完成的文案；回显
  stdin 的 hook 收 200KB payload 不超时且输出被截断。
- **验证**：外层执行沙箱之外 `cargo test -p kloop-core --lib` 全绿（1366 项）；
  `cargo clippy -p kloop-core --tests -- -D warnings` 与 `cargo fmt --all` 通过。未跑
  `make check`，Windows 与 PowerShell 原生用例未实测（无本机环境）。
