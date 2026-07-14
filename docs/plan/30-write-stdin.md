# Plan 30 — write_stdin(交互式后台进程)

> 一句话定位:kloop 有后台 bash(`run_in_background`+`bash_output`+`kill_bash`),但后台
> shell spawn 时 `stdin(Stdio::null())`——**写不进 stdin**,REPL/ssh/交互确认这类"进程活
> 着、要持续喂输入"的场景做不了。补一个 `write_stdin(bash_id, chars)`,把后台 shell 的
> stdin 从 null 改 piped、存句柄、加个写入工具。最小切法,不碰 PTY。

## 回源结论(已沉淀,2026-07-10 plan 14 第三片后;`refs/README.md:77-85`)

- **codex —— `unified_exec`(唯一有此能力者)**:`exec_command`(cmd/tty/
  yield_time_ms 默认 10s/max_output_tokens)先等一会,等不完就**存进程返回 session_id**;
  `write_stdin(session_id, chars)` 续写,**空 chars = 纯轮询**(默认 5s,上限 300s)。可持
  续写 stdin(REPL/ssh/交互确认)正是 kloop 后台 bash 缺的能力;**PTY 可选**(`tty` 参
  数)。生命周期:上限 64、LRU 淘汰(保护最近 8、优先淘汰已退出),turn 结束全清,无空
  闲超时。输出 HeadTailBuffer 1MiB(头尾各 50%)。源码:`codex-rs/core/src/unified_exec/`
  + `core/src/tools/handlers/unified_exec/`。
- **claude-code —— 无**:后台任务靠模型轮询输出,没有往活进程写 stdin 的工具。
- **claw-code —— 无**。
- **借鉴清单原话(`:85`)**:① `write_stdin` 最小切片(挂 plan 14 备选);③ 小卫生件
  (HeadTailBuffer、进程表上限+LRU)有痛感时再抄。**不抄**:PTY、ToolOrchestrator 审批
  沙箱耦合、多 agent 全家桶、notify。

## kloop 现状与改造点(已读准,`crates/core/src/tools/bash.rs`)

- `BackgroundShells`(:309)= session 级后台 shell 注册表,挂 `Config.background_shells`
  (子 agent clone 继承);`BgShell`(:296)存 child/pid/输出文件。
- `spawn_background`(:319+)后台 spawn:**`cmd.stdin(Stdio::null())`**(:337)、
  stdout/stderr 重定向到 offload_dir 的输出文件(`bash_output` 读其尾,默认阻塞到完成)。
- 权限:`bash_output`/`kill_bash` 已在只读白名单(`permissions.rs:558`,读注册表/发信号)。

**改造面(小)**:
1. `spawn_background`:stdin `Stdio::null()` → `Stdio::piped()`,`child.stdin.take()` 存进
   `BgShell`(新增 `stdin: Option<ChildStdin>` 字段;不写也闲置无害)。
2. 新 `write_stdin(bash_id, chars)` 工具:取该 shell 的 stdin 句柄,write + flush;进程已
   退出/句柄已关 → model-facing Err;未知 id → Err(照 `bash_output` 的错法)。
3. 写完读**新增的输出尾**返回(复用 `bash_output` 的读尾逻辑,让模型看到进程对输入的反
   应),不等进程完成。
4. 工具注册进 defs;权限见决定 2。

## 关键决定(开工时定 / 问用户)

1. **空 chars 语义**:抄 codex"空写 = 纯轮询"(等 yield 后读新尾)吗?kloop 现有
   `bash_output` 默认**阻塞到完成**——交互进程永不完成,所以要么给 `bash_output` 加"非阻
   塞读当前尾"模式,要么让 `write_stdin(chars="")` 兼任轮询。倾向后者(单工具闭环,和
   codex 一致)。
2. **权限**:`write_stdin` 有副作用(往进程写),但目标进程 spawn 时已过 bash 权限门。
   放行(视作与已批准进程交互,像 `kill_bash`)vs 过门(每次写都问,交互场景摩擦大)。
   codex 无审批。倾向**放行**——但写入内容不可见于 spawn 时的审批,若有顾虑可对
   `write_stdin` 单独 ask 一次并缓存。开工定。
3. **stdin piped 的普适代价**:所有后台 shell 统一 piped(take 存句柄、不写闲置)vs 仅
   `run_in_background` 显式声明可交互时 piped。倾向统一 piped(简单,句柄闲置无成本)。
4. **换行处理**:`chars` 原样写还是自动补 `\n`?REPL 通常需要换行提交。倾向**原样写**
   (模型自己带 `\n`,和 codex 一致、可表达"不换行的部分输入"),工具描述点明。

## 不做(挂账)

PTY(`tty` 参数;交互程序探测 isatty 时行为差异——有真实需求再抄,最小切法只连管道
stdin);进程表 LRU 上限 64 + 保护最近 8(kloop 后台 shell 现无数量上限,有痛感再抄);
HeadTailBuffer(kloop 已用输出文件 + 读尾,不换);turn 结束全清后台 shell(kloop 现语义
是 parent turn 结束不 kill,不在本 plan 改);空闲超时。

## 测试

spawn piped → `write_stdin` 写一行 → 读回显含预期(用 `cat` 或 `sh` 交互:写 `echo hi\n`
读到 `hi`);空 `chars` 轮询读新尾;写已退出进程 → Err;未知 bash_id → Err(照
`bash_output`);`write_stdin` 进只读/副作用分类正确(按决定 2)。

## 完成标准

fmt/clippy/test 全绿,一次 commit;真 key 验收(至少一轨:让模型起一个交互进程——如
`python3 -i` 或 `sh` ——用 `write_stdin` 喂命令、读回结果完成一次真实交互);README 同步
`write_stdin` 用法;本文件补完成记录(提交号 + 挂账);HANDOFF 补教训(尤其空 chars 轮询
语义 + 权限选型)。
