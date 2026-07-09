# HANDOFF — kloop(Rust agent MVP)会话交接文档

> 项目名 **kloop**:k is for keel——作为龙骨的循环(agent loop 是整个项目的结构骨架)。命名时否掉的备选:skyloop(家族感强但用户想要更工具化)、keelloop(语义足但偏长)。crate/二进制均为 `kloop`。
>
> **目录布局**:根目录是工作区;`kloop/` = 工程本体(Rust crate,未来 git init / 推远端的就是它);`refs/` = 参考资料(claw-code 本地拷贝 + 三个参考代码库的导读,见 refs/README.md);根目录另留给用户放各种参考。

> 给新会话的 Claude:这是上一个会话的完整交接。读完本文件即可继续实现,无需重新调研。
> 用户偏好:中文交流,短句增量澄清,别用多选项问卷。

## 一、这个项目是什么

一个 **Rust 单 crate 的最小 agent MVP**,目的是在动手写"新 agent"之前,用约 1200 行代码验证架构蓝图里的 5 个核心赌注(见下)。它是"第一块砖"——用户明确选了 Rust 而非 TS,理由是产出可回流到 codex fork(变成 ext/ crate),并可复用团队 Rust 技术栈。

**刻意不做**(Phase 2 再说):压缩/compaction、TUI、MCP、hooks、权限系统、多轮会话持久化。

## 二、背景结论(三轮调研的浓缩)

上一会话对比了三个代码库,结论:

1. **codex**(openai/codex 的生产级 Rust fork,refs/codex):地基最硬——分层循环(任务→主循环→provider故障转移→请求重试→流消费,各一层)、append-only 历史硬规则、多模型工具画像(model_info 按模型切工具形态)、unified exec 持久 shell 会话。弱在:上下文耐力(只有 pre/mid-turn 两处被动压缩)、恢复语义少、工具默认不并行、shell 万能导致权限粒度粗。
2. **claude-code**(TS,~/work/claude-code,逆向重实现版):赢在生存层——七层上下文防线(含 predictive/reactive 压缩)、丰富恢复语义(输出截断升级重试、fallback 模型、孤儿 tool_result 修补、Terminal 原因枚举)、专用工具(Read/Edit/Grep/Glob)+ `isConcurrencySafe(input)` 按入参动态并发(只读批并发上限 10)、子 agent 递归复用同一 query() 循环。
3. **claw-code**(github.com/ultraworkers/claw-code,agent 自治维护的 Rust 克隆,已精读 11.6 万行):**不可作底座**。约 60% 真实/25% 孤儿/15% 表演;压缩是假的(不调模型,关键词模板套 Claude Code `<summary>` 戏服,触发数学错误)、工具严格串行、Worker/Cron 是内存模拟。仅三样值得抄:mock-anthropic-service + JSON 输出契约测试纪律;`openai_compat.rs` 的 tool_calls 流式翻译状态机(对多模型适配有直接参考价值);压缩边界回退避免切开 tool_use/tool_result 对。
4. 两边独立收敛的"必然解"(直接照抄不必发明):tool_use 有无判续跑(别信 stop_reason)、deferred 工具+tool_search、超长输出落盘+回读工具、MCP `server__tool` 命名。
5. 总战略建议:优先在 codex fork 上吸收 cc 之长(P1 predictive/reactive 压缩 → P2 恢复语义 → P3 工具形态),MVP 是并行的验证实验。**P0 已拍板(2026-07-09,用户确认)**:① 编辑工具选 Edit(old_string/new_string)形态,不迁就 apply_patch(实测连 gpt-5.4-mini 都首试即过;将来按模型切换靠 model_info 画像机制);② 目标模型双轨,Claude(sonnet-5)为主、OpenAI-compat 为副;③ codex 上游从"可合并性"降级为"可跟随性"(定期 rebase 吸收基础设施修复,不追求功能层对齐)。下一步:总战略 P1——在 codex fork 上做 predictive/reactive 压缩。

## 三、MVP 要验证的 5 个赌注

1. **append-only 历史 + 录入时 offload**:历史只追加;tool_result 超过上限(8000 字符)在 record 时落盘到 offload 目录,历史里只放 preview + 指针,提供 `read_offloaded` 工具回读。
2. **续跑信号 = 是否存在 tool_use 块**,绝不用 stop_reason 判断。
3. **按入参动态并发**:工具声明 `is_concurrency_safe(name, input)`(bash 按命令解析只读性);连续的安全调用合并为一批并发执行,其余串行——学 cc 的 partition 策略。
4. **子 agent 递归复用同一个 run_turn 循环**(深度限 1,通过 `Pin<Box<dyn Future>>` 打断类型递归)。
5. **provider 适配缝**:内部统一 Anthropic Messages 形状;两个适配器——Anthropic 原生 SSE + OpenAI-compat(chat/completions 的 tool_calls 增量流翻译成统一 StreamEvent);另有 Mock provider 供无 key 端到端验证。

附加(便宜且高价值):流式重试(3 次,指数退避 250ms<<n + 纳秒抖动,不引入 rand 依赖)、CancellationToken 中断、**中断时孤儿 tool_use 补 is_error 的 tool_result** 保持历史合法(cc 的教训)。

## 四、当前进度

**✅ MVP 已完成(2026-07-09)**:全部 8 个文件写完,`cargo build` 通过,7 个测试全绿,`cargo run -- --mock` 六轮演示端到端跑通(并发批 → offload → 回读 → 子 agent → 收尾,Completed after 5 rounds)。与下述计划的两处实现偏差:
1. offload 编号用进程级全局 `AtomicUsize` 而非 per-History 字段——父子 agent 共用 offload 目录,per-History 计数会互相覆盖文件。
2. 子 agent 递归不是裸 `Box::pin`:rustc 对递归 async fn 的 Send auto-trait 推断存在共归纳盲区("cannot satisfy"),裸 Box::pin 转 `dyn Future + Send` 编不过。实际方案:`execute_tool` 签名改为返回 `Pin<Box<dyn Future + Send>>`(类型擦除的递归边界)+ 子 agent 用 `tokio::spawn` 跑成独立任务(task_tool 只持有无条件 Send 的 JoinHandle)。
**真实 API 验证(2026-07-09)**:两个适配器都过了真实流量。
- OpenAI-compat(用户代理 + gpt-5.4-mini):工具调用、多轮续跑、offload 落盘→模型主动 read_offloaded 回读、task 子 agent 全部跑通。
- Anthropic 原生 SSE(taijiaicloud 代理,Bedrock 后端 claude-sonnet-5):流式 text_delta、input_json_delta 累积、多轮工具、offload 闭环全部跑通。注意 `ANTHROPIC_BASE_URL` 不含 `/v1`(适配器自己拼 `/v1/messages`)。
另:子 agent 的文本已不再流式输出到主 UI(depth>0 静音,产出走 tool result 返回)。
- **编辑工具数据点(P0 相关)**:同一个"修 buggy.py 并验证"任务,gpt-5.4-mini 和 claude-sonnet-5 都是首试即过——先 read_file,再精准 old_string/new_string 的 edit_file(唯一匹配、无重写全文),bash 验证输出。连 mini 档的 GPT 都能用好 Edit 形态,apply_patch 不构成选型约束。

原计划(已按此完成,留作参照):
- `Cargo.toml` — 依赖:anyhow, futures, reqwest(json/stream/rustls-tls), serde, serde_json, tokio(full 等价特性集), tokio-util
- `src/types.rs` — Role / ContentBlock(Text|ToolUse|ToolResult,serde tag="type" snake_case,与 Anthropic 线格式一致)/ Message(user_text/assistant/tool_results 构造器)/ StreamEvent(TextDelta|BlockDone|Done{stop_reason})/ ToolDef{name,description,schema}
- `src/sse.rs` — 增量 SSE 解析器(跨 chunk、CRLF、多行 data、注释行),带 2 个单测

待写(按此顺序):
1. `src/provider.rs` — `enum Provider { Anthropic{key,base}, OpenAiCompat{key,base}, Mock{turns: Mutex<VecDeque<Vec<ContentBlock>>>} }`。方法 `stream(&Arc<Self>, model, system, messages, tools) -> mpsc::Receiver<anyhow::Result<StreamEvent>>`:请求体在 spawn 前构好(避免生命周期),tokio::spawn 内发请求、bytes_stream 喂 SseParser、翻译成 StreamEvent 发 tx。
   - Anthropic:POST {base}/v1/messages,headers x-api-key + anthropic-version: 2023-06-01,body {model, max_tokens:8192, system, messages, tools:[{name,description,input_schema}], stream:true}。事件按 data.type 分发:content_block_start(记 index→{type,id,name,text,json})/ content_block_delta(text_delta→发 TextDelta 并累积;input_json_delta→累积)/ content_block_stop(finalize→BlockDone;json 解析失败退 `{}`)/ message_delta(记 stop_reason)/ message_stop(→Done)/ error(→Err)。未知块类型(thinking 等)忽略。
   - OpenAiCompat:POST {base}/chat/completions,Bearer;历史翻译:user text→{role:user,content};assistant→content + tool_calls[{id,type:function,function:{name,arguments:序列化字符串}}];tool_results→逐条 {role:tool,tool_call_id,content};system→{role:system}。流:choices[0].delta.content→TextDelta+累积;delta.tool_calls 按 index 累积 id/name/arguments;finish_reason 或 [DONE] 时 finalize(text 块若非空 + 各 tool_use,arguments 解析失败退 Value::String(raw))→Done。
   - Mock:pop 一轮 blocks,Text 块先发 TextDelta 再逐块 BlockDone,最后 Done;队列空时发 Text("mock exhausted") 终止循环。
2. `src/history.rs` — `History{items:Vec<Message>, offload_dir, cap:8000, spill_count}`;`record()` 对超限 ToolResult 落盘 `off-{n:04}.txt`,替换为前 1500 + "…[truncated]…" + 后 500 + 指针行("full output offloaded, id=off-XXXX, use read_offloaded");`messages()` 返回切片。附单测:超限落盘+指针、未超限原样。
3. `src/tools.rs` — 工具:bash{command,timeout_ms?}(sh -lc,tokio::process,默认 60s 超时,合并 stdout/stderr,非零退出码注明)、read_file{path,offset?,limit?}(行号格式 "{n}\t{line}",默认 2000 行)、write_file{path,content}(create_dir_all 父目录)、edit_file{path,old_string,new_string,replace_all?}(0 次或歧义多次报错)、read_offloaded{id}(id 只允许 [A-Za-z0-9-])、task{prompt,max_rounds?}(depth>=1 时报错"子 agent 不能再生成子 agent";否则新 History + `Box::pin(run_turn(...)) as Pin<Box<dyn Future<Output=TurnOutcome>+Send>>` 递归,深度+1,轮上限 15,返回最终文本)。
   - `is_concurrency_safe`:read_file/read_offloaded→true;bash→命令解析:按 ;|&& 切段(过滤空段),段含 '>' 即不安全,首 token 在允许表 {ls,cat,rg,grep,find,head,tail,wc,pwd,which,file,stat,tree,du,echo},git 需二级词 ∈{status,log,diff,show,branch};其余工具→false。附单测。
   - 派发 `dispatch_tools`:按"连续同安全性"分批(学 cc partition);安全批 join_all 并发,其余顺序;每个执行包 `tokio::select!{ _=cancel.cancelled() => 补 is_error:true 的"interrupted" ToolResult, r=execute(...) => r }`;取消后剩余调用全部补 interrupted 结果(孤儿修补,保证每个 tool_use 都有配对 tool_result)。
   - `ToolCtx{cfg:Arc<Config>, ui:Arc<dyn Ui>, cancel:CancellationToken, depth:u8}` 全可克隆。
4. `src/agent.rs` — `run_turn(cfg:&Arc<Config>, history:&mut History, ui:&Arc<dyn Ui>, cancel:&CancellationToken, depth:u8) -> TurnOutcome{reason:EndReason{Completed|MaxRounds|Aborted|Error(String)}, final_text, rounds}`:
   ```
   for round in 0..cfg.max_rounds {
       let sampled = sample_with_retry(...)?;            // 3 次退避重试;Cancelled 直接返回 Aborted
       history.record(Message::assistant(sampled.blocks.clone()));
       let tool_uses = 过滤 ToolUse;
       if tool_uses.is_empty() { return Completed(最后 Text 块) }
       let results = dispatch_tools(...).await;          // 见上
       history.record(Message::tool_results(results));   // 先记录再判断取消 → 历史永远合法
       if cancel.is_cancelled() { return Aborted }
   }
   return MaxRounds
   ```
   `sample_once`:consume receiver,select 取消;channel 提前关闭→Retryable("stream closed early")。`Ui trait(Send+Sync){text_delta,note}`。附端到端单测:Mock provider 三轮(2 个安全 bash 并发批 → 1 个不安全串行 → 最终 Text),断言历史形状 [user, assistant(2 tool_use), user(2 tool_result), assistant(1 tool_use), user(1 tool_result), assistant(text)] 与 Completed。
5. `src/main.rs` — REPL:tokio stdin 按行读,"exit"/Ctrl+D 退出;每 turn 新 CancellationToken + spawn ctrl_c watcher(触发 cancel,turn 结束后 abort watcher;提示用户空闲时用 Ctrl+D 退出)。`--mock` 标志用 Mock provider 跑演示脚本。Config::from_env:AGENT_PROVIDER 显式指定,否则 ANTHROPIC_API_KEY→Anthropic(默认模型 claude-sonnet-5),否则 OPENAI_API_KEY→OpenAiCompat(AGENT_MODEL 必填,OPENAI_BASE_URL 默认 https://api.openai.com/v1);都没有且非 --mock 则报错。系统提示词简短:"You are a coding agent…cwd…"。offload 目录 `.kloop/offload/`。
6. `README.md` — 5 个赌注、范围内/外、运行方式(--mock / 真 key)、验证清单。

然后:`cargo build` → 修错 → `cargo test` → `cargo run -- --mock` 端到端验证,向用户报告结果。

## 五、代码风格约定(沿用 codex 的 AGENTS.md 精神)

format! 内联变量;collapsible_if 折叠;match 尽量穷尽;避免 bool 位置参数(必要时 /*param*/ 注释);测试用 pretty_assertions 风格的整体对象断言(本项目未引依赖,用标准 assert_eq! 整体比较即可)。

## 六、给新会话的第一步建议

1. 先在 `kloop/` 下 `cargo build` 确认已写的三个文件无误,再按上面顺序补齐其余文件。
2. 全部跑通后向用户汇报 5 个赌注的验证结果,然后回到悬而未决的 P0 问题(目标模型)。
