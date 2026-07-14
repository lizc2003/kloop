# Plan 27 — code mode:MCP 工具暴露给 program(✅ 切片 1+2+3 全部完成)

> **✅ 切片 1+2+3 全部完成**(提交见文末完成记录;三侧真 key 验收均过)。以下为原备忘;
> 完成记录在文末。开工前读 HANDOFF + `docs/plan/24-code-mode.md`(首片 + 三个追加片
> 完成记录 + 回源结论)+ `refs/README.md` 的 code-mode 一节。plan 24 已把 code-mode 的
> 引擎/`exec`/op 层 gate/`agent()`/观测/`parallel`+`pipeline` 都做完并真机验收;本 plan
> 只补一件事:**让 program 能调 MCP(外部 ToolSource)工具**——"code execution **with MCP**"
> 的招牌能力。**这是 plan 24 挂账里最有价值、也最需要回源的一片**,故单立一 plan。

## 是什么

现在 program 的 `tools` 对象只含**内置**工具(plan 24 首片:`program_tool_names()` 从
`tool_defs(0)` 取、减 {exec, task})。MCP 工具(经 `ToolSource` 缝接入,`{server}__{tool}`
命名)**不在 program 面里**,模型在 program 里调 `tools.srv__lookup(...)` 会因 `tools` 上没这
方法而 `TypeError`。本 plan 把外部 source 工具也暴露成 program 可调的类型化 API。

## 为什么值得

- **招牌用例**:Anthropic「code execution with MCP」、Cloudflare「Code Mode」的核心论证就是
  "把 MCP server 变成 TS API 让模型写代码调"(Cloudflare 自己的数字:转 TS API 省 81% token)。
  kloop 现在 code-mode 只能编排内置工具,还没吃到这块最大红利。
- **组合性**:一个 program 里把多个 MCP 工具 + 内置工具 + `agent()` 串起来(fetch→transform→
  store 那类),中间结果留变量、只回最终产出——正是 code-mode 相对逐轮 tool_use 的杠杆。

## 现状(plan 24 已就位的地基,别重做)

- **执行/gate 已经通**:`CoreBridge.call_tool` 对任意名字调 `run_one`→`execute_tool`,未知内置
  名会落到 `find_source(&ctx.cfg.tool_sources, other)`→`source.call`。所以**只要 program 里发起
  了对 `srv__x` 的调用,执行 + 权限门 + 并发分类(`is_concurrency_safe` 已查 `source.is_readonly`)
  就已全部工作**。缺的只是"让 program **知道**并**能发起**这个调用"两件事:
  1. 运行期 `tools` 对象要含 source 工具方法(`program_tool_names()` 现在不含);
  2. `exec` 的 TS 声明(description)里要列出 source 工具,否则模型不知道能调、也不知道 schema。

## 关键决定(回源后定 / 开工时定)

1. **`exec_def` 生成点上移(结构改动)**:`exec_def` 现在在 **`tool_defs(depth)`**(纯函数、无
   sources)里被调用、只能列内置。要列 source 工具,得把 exec 的 def 生成挪到**能看到 sources 的
   地方**——`all_tool_defs(depth, sources, threshold)`(它已持有 sources)。即 `tool_defs` 不再产
   exec,`all_tool_defs` 在 depth-0 用 `builtins + sources` 造 exec 追加。**注意**:`all_tool_defs`
   有 defer 分支(超阈值时只返 builtins + tool_search + call_tool、**不含 source defs**),exec 要在
   两个分支都出现(见决定 2)。会牵动 `all_tool_defs_appends_sources_and_skips_collisions` 等既有
   测试的期望数组/计数,要一并改(教训:磁盘/接口形态测试要同步)。
2. **defer 交互(本 plan 命脉决定)**:超阈值(默认 30 工具)时,顶层请求把 source 工具 defs 撤下
   (plan 16),模型只见 builtins + tool_search。那 program 里还能调 MCP 吗?——**code-mode 恰恰是
   defer-bloat 的另一解**:与其"撤 defs + tool_search 现搜",不如"把工具压成紧凑 TS API 塞进 exec
   一个工具的 description"。所以倾向:**defer 开时,exec 的 description 仍带 source 工具的 TS 声明**
   (TS 比 JSON schema 紧凑、且集中在一个工具里,比发 N 份 defs 省),program 可直接调,**无需在
   program 里 tool_search**(program 里也没法 tool_search)。**但要回源核对 codex 怎么处理**:它有
   `globals.rs` 注入全局 `ALL_TOOLS`([{name,description}]) + `DEFERRED_NESTED_TOOLS_GUIDANCE`,似乎
   是"deferred 时只给紧凑名单、不给全量 TS 声明,让 program 自己 filter"。两条路(全量 TS 声明 vs
   紧凑名单 + 按需)取舍要看 codex 的真实做法 + 工具规模的 token 账。**开工回源后定**。
3. **结果形状**:MCP 工具在 codex 里返 `CallToolResult<T>`(结构化 content),`description.rs` 为此
   生成 `CallToolResult<T>` TS 类型。kloop 的 `ToolSource::call` 返 `String`(content 已拍平成文本,
   plan 10)。倾向**沿用 `Promise<string>`**(与内置一致、最小),结构化 content 挂账;若某 MCP
   工具的文本结果是 JSON,program 自己 `JSON.parse`。
4. **命名**:source 工具名已是 `{server}__{tool}`(plan 10 消毒过:非 alnum+`_` 折成 `_`)。`__` 在 JS
   里是合法标识符字符,但 prelude 用 `tools["srv__x"]` 括号访问最稳(plan 24 首片已是括号形态)。
5. **program 面组成**:`program_tool_names()` 要并入 `ctx.cfg.tool_sources` 的工具名(exec_tool 有
   ctx、能取)。仍减 {exec, task, tool_search, call_tool}(后两个是 defer 元工具,program 不该调)。
   受限 agent 类型(`tool_allowlist`)对 source 工具的过滤要一致(现 exec 只在 depth-0,子 agent 不
   给 exec,故本 plan 主要是主 agent 面;但 allowlist 逻辑要想清)。

## 回源待办(开工必做,教训 11 + 14)

- **codex code-mode**:`code-mode/src/runtime/globals.rs`(`build_all_tools_value`/`ALL_TOOLS`、
  enabled tool 挂载)、`code-mode-protocol/src/description.rs`(`normalize_code_mode_identifier`、
  `render_json_schema_to_typescript`、`CallToolResult<T>`、`MCP_TYPESCRIPT_PREAMBLE`、
  `DEFERRED_NESTED_TOOLS_GUIDANCE`)——**重点:deferred 工具在 code-mode 里怎么暴露**(全量 TS 声明
  还是紧凑名单 + filter?),结果类型怎么给。
- **Anthropic code-execution-with-MCP**:`./servers/<server>/<tool>.ts` 布局、`search_tools` 端点、
  progressive disclosure 与 token 账的原始论证(决定 2 的依据)。
- **cc dynamic workflows**:MCP 工具在 workflow 脚本里怎么出现(若有)。

## 候选切片(开工和用户定)

1. **defer 关(≤阈值)时暴露 source 工具**:`exec_def` 上移到 `all_tool_defs`,program 面 + TS 声明
   并入 source 工具;用进程内 stub source 端到端测(program 调 `srv__x` 过 gate 跑通、TS 声明含它)。
   修既有 tool_defs/all_tool_defs 测试期望。
2. **defer 开(>阈值)时的暴露策略**:按回源结论落地(全量紧凑 TS 声明 or 名单 + 按需);测跨阈值
   两侧行为。这是本 plan 命脉,单独一片。
3. **结构化结果**(可选,最后):`CallToolResult` 形态,若有真实 MCP 需求再上。

## 其余 code-mode 挂账(不在本 plan,记此备忘)

plan 24 完成记录里还挂着:token `budget`(program 里读 spent/remaining 做动态循环)、后台 program
+ `yield`/`wait`(codex observation frontier)、保存复用 + journal resume(cc 具名 workflow /
agentCallKey)、`Limits` 走配置/env(现硬编码 64MiB/512KiB/5s)。都是更边缘的小件,可各自随手做或
另立 plan,不阻塞本 plan。

## 测试(方向)

stub source 端到端:program 调 `srv__x(args)` 经 gate 跑通、被 deny 的 source 工具在 program 里
同样被拒;`exec` 的 TS 声明含 source 工具签名(defer 两侧各断言);program 面并入 source 名、仍减
元工具;既有 tool_defs/all_tool_defs 计数/顺序测试同步更新。真 key:配一个真实(或 mock)MCP server,
模型写 program 编排"内置 + MCP 工具 + agent()"跑通闭环。

## 完成标准

按所选切片;fmt/clippy/test 全绿;真 key 至少一次"模型在 program 里调 MCP 工具、gate 生效";
README、HANDOFF、plan 24 挂账(②)与本 plan 更新;未选切片记挂账。

## 完成记录(切片 1+2,2026-07-13,提交号 5ffb01f)

开工时用户定"都做"(切片 1+2 一起;切片 3 结构化结果挂账)。

### 回源结论(三家真读,教训 11+14)

- **codex(唯一有真 Rust code-mode 实现,决定性)**——两个面**解耦**:
  - 运行期 `tools` 对象(`code-mode/src/runtime/globals.rs` `build_tools_object`)= 从
    `enabled_tools` 建,**所有启用工具(含 MCP)永远可调**;另有 `ALL_TOOLS` 全局 =
    `{name, description}[]` 紧凑清单,也永远在。
  - `exec` 工具的 **description**(`code-mode-protocol/src/description.rs`
    `build_exec_tool_description`)才受 defer 影响:非 defer → 每工具**完整 TS 声明** +
    `CallToolResult<T>` 前言;**defer 开 → 不放任何 MCP 的 TS**,只加一句
    `DEFERRED_NESTED_TOOLS_GUIDANCE`("部分工具从描述省略,但仍在 `tools`/`ALL_TOOLS`,
    按 name/description 过滤 `ALL_TOOLS` 去找")。分流在 `core/src/mcp_tool_exposure.rs`
    `build_mcp_tool_exposure`:`search_tool_enabled` 时**全部** MCP 进 `deferred_tools`、
    `direct_tools` 清空(全或全无)。
  - **命脉裁定(决定 2)**:plan 原倾向"defer 开仍塞全量 TS"被**回源纠偏**——codex
    做法相反,defer 开降级紧凑 name+desc。理由见教训 19(a):defer 触发就意味工具多到
    不发全 schema,code-mode 不该破例把 bloat 换个地方灌回。
- **Anthropic「code execution with MCP」**(公开设计):MCP 呈现成文件树
  `./servers/<srv>/<tool>.ts`,渐进 import,中间态留代码。三目标(工具→代码 API、
  渐进披露、中间态不进上下文)与 kloop 一致,底座不同,不改形态。
- **cc dynamic workflows**:是**编排 agent**(不是编排 tool_call),脚本里子 agent 各自
  `ToolSearch` 按需加载 MCP,不把工具烤进脚本全局面。形态不搬,只印证"deferred→按需发现"。

### 落地(切片 1+2)

- **结构改动**(决定 1):`run_program` 的 def 生成从 `tool_defs`(纯函数、无 sources)
  上移到 `all_tool_defs`(持 sources)。`tools/mod.rs` 拆出私有 `builtin_defs(depth)`(内置 +
  depth-0 的 task,**不产 run_program**);`tool_defs` 仍产**内置版** run_program(仅供
  `defer_active`/`tool_merge_warnings` 计数 + `program_tool_names` 过滤,**从不发给模型**);
  `all_tool_defs` 在 depth-0 产**含 source 的** run_program。counts/collision/阈值语义**逐字节
  不变**(builtin_defs 少的 1 个 run_program 由 all_tool_defs 补回),唯一改的是 `all_tool_defs`
  里 run_program **从 source 之前挪到之后**(TS 依赖 sources,故最后生成)——既有测试
  `all_tool_defs_appends_sources_and_skips_collisions` 期望数组同步。
- **program 面**(决定 5):`program_tool_names(sources)` 并入 `merged_source_defs`——**运行期
  `tools` 永远含所有 source 方法**,defer 与否都可调(否则 `tools.srv__x` TypeError);减
  {run_program, task}(tool_search/call_tool 天然不在,因它们不在 builtin_defs 也不在 source)。
- **run_program def**(`tools/codemode.rs`):签名 `run_program_def(callable, deferred)`。
  `callable` 得全量 TS 声明(内置 + inline source);`deferred` 非空时追加 `- tools.<name>: <desc>`
  紧凑清单 + 引导("cannot call tool_search from inside a program;要精确 schema 先普通轮
  tool_search 再写 program")。命名(决定 4)沿用 plan 10 消毒后的 `{server}__{tool}`,`__` 是
  合法 JS 标识符字符,直接作方法名。
- **结果形状**(决定 3):沿用 `Promise<string>`(内置一致、最小);结构化 `CallToolResult<T>`
  挂账(切片 3)。
- **locked 门交互**(实现踩坑,回源未覆盖):defer 开时顶层直调 deferred 工具被 `run_one` 的
  `locked()` 弹回(必须先 tool_search);但 program 调它经 `CoreBridge` 也走 `run_one`,会被同一门
  弹回。解法:`ToolCtx` 加 `from_program: bool`,`CoreBridge::new` 置 true,`run_one` 仅在
  `!from_program` 时查 `locked()`。**纯发现门绕过,非安全门**(教训 19b):deny/权限/沙箱/hooks
  照常;保住 plan-16"仅 tool_search 解锁"不变量(program 不写 `unlocked_tools`,顶层直调仍弹回)。

### 测试(+6,共 364)

`tools/codemode/tests.rs`:非 defer 时 program 调 `srv__echo` 经 gate 跑通(`program_calls_an_mcp_source_tool`)、
deny 的 source 工具在 program 里被拒(`denied_mcp_source_tool_is_refused_in_a_program`)、defer 时顶层直调
弹回但 program 调它跳过 locked 跑通(`program_calls_a_deferred_mcp_tool_that_top_level_cannot`)、
`program_tool_names` 含 source 名(`program_surface_includes_source_tools`)。`tools/mod.rs`:inline 时
run_program TS 含 source 全量签名(`run_program_def_declares_inline_source_tools`)、defer 时降级紧凑名单 +
引导(`run_program_def_lists_deferred_source_tools_as_a_manifest`)。既有 `all_tool_defs_appends_sources_and_skips_collisions`
顺序期望同步(run_program 挪到末尾)。

### 真 key 验收(anthropic/sonnet-5,已过)

用户定"验收"。临时 stub MCP server(scratchpad `stub_mcp.py`,几十行 Python newline JSON-RPC,
暴露 `echo`/`danger` 两工具)+ 临时 `.kloop/config.toml` `[mcp.servers.stub]`(验完撤,未入库)+
`AGENT_ALLOW=stub__echo`/`AGENT_DENY=stub__danger`,真 key(`.kloop/env.local`)跑 `--plain`:

- **切片 1(inline,默认阈值)**:模型自发写 `run_program` 调 `tools.stub__echo({text:"plan27"})` +
  try/catch 调 `tools.stub__danger(...)`,返回 `{"echo":"echo: plan27","danger":"blocked"}`——echo 经
  MCP 链跑通(结果确从 stub server 回来)、danger 被 deny 规则**在 program 内**拒掉(catch 到),
  **gate 生效**。
- **切片 2(defer,`AGENT_DEFER_THRESHOLD=5` 强制)**:启动打印"16 tools registered (> 5) …
  deferred";模型写 `run_program` **直接**调 `tools.stub__echo({text:"defer27"})`(**没 tool_search**——
  从 run_program description 的紧凑清单发现),返回 `echo: defer27`——deferred MCP 工具在 program 里
  跑通(`from_program` 跳过了会弹回顶层直调的 `locked()`)。

完成标准的"真 key 至少一次模型在 program 里调 MCP、gate 生效"两侧(inline + defer)均已过。

### 自发采用验证(教训 18,已过 —— plan 27 打开的生态位真的被模型自发选中)

教训 18 的推论:run_program 的独有生态位 = "编排 bash/直调干不了的工具(MCP)",而这生态位
**要等 plan 27 才打开**;开了之后要单独验"模型会不会**自发**选它"(验法:不点名 run_program、
给一个落在该生态位的任务)。stub 加一个 `analyze(word)→{vowels,length}` 工具,`AGENT_ALLOW=stub__analyze`,
prompt = "这 10 个单词用 stub__analyze 拿统计,告诉我元音最多的 3 个"(**点了工具、没点 run_program**)。
结果:sonnet-5 **自发写 run_program**——`const words=[...]` 循环里对 10 个词各调 `tools.stub__analyze`
(trace:1 条 run_program + 其内 10 条 analyze op,顺序为 run_program 先、10 个 analyze 紧随=嵌套),
只把 top-3 结果回上下文(10 份中间 MCP 结果留在程序里),答案正确(banana/elderberry/honeydew 各 3 元音)。
**这正面印证 plan 27 论点**:补上"别人干不了的活"(MCP 扇出)后,聪明模型不再回退到逐次直调,
而是自发用 code-mode 编排。教训 18 的闸门(独有生态位)是对的、且 plan 27 把它打开了。

### 切片 3(结构化 `CallToolResult`,已完成,2026-07-14,提交号 <待填>)

用户定"完整版"(不走精简"只补 structuredContent 到文本"路)。回源:codex 把工具结果作
**结构化 JSON 对象**送进 JS(`code-mode/src/runtime/module_loader.rs` `resolve_tool_response`
→ `json_to_v8`;`RuntimeCommand::ToolResponse{id,result}` 的 result 是原始 CallToolResult),
program 拿 `Promise<CallToolResult<T>>`。kloop 原本一路字符串契约(wire `render_content` 拍平丢
`structuredContent` → `ToolSource::call → String` → `HostBridge::call_tool → Result<String>` → JS 串)。

3-crate 落地:
- **kloop-mcp**:`call_tool_structured(name,args) -> Result<Value>` 保留原始 CallToolResult(isError
  仍 → Err 带渲染文本);`call_tool -> Result<String>` 变薄壳 = `render_result(call_tool_structured?)`;
  `render_result(&Value)` 提为 pub(CLI 渲染文本免二次 wire 调)。wire 契约测试零改(`call_tool`
  语义不变)。
- **core**:`ToolSource::call` 返回 `SourceOutput{text, structured: Option<Value>}`(6 impl 改,
  非 MCP 用 `SourceOutput::text(..)`);`ToolCtx` 加**按调用旁路** `program_result:
  Option<Arc<Mutex<Option<Value>>>>`;`execute_tool` source 臂命中时把 `out.structured` 投进旁路、
  仍回 `out.text`(model 路径/tool_result 不变——protocol content 是文本)。**关键**:旁路是
  per-call 的(CoreBridge 每次调用新建槽、设在 clone 的 ctx 上),故 `Promise.all` 并发调用互不串。
- **codemode.rs**:`CoreBridge.call_tool -> Result<Value,String>`,run_one 回 String 后从旁路 `take()`
  出结构化(有则回对象、无则 `Value::String(text)`——内置保持字符串契约);`run_program_def(builtins,
  sources, deferred)` 三参,source 声明 `Promise<CallToolResult>` + 注入紧凑 `type CallToolResult` 定义,
  内置仍 `Promise<string>`。
- **engine(kloop-codemode)**:`HostBridge::call_tool -> Result<Value,String>`;`envelope` 收 Value——
  prelude 本就 `JSON.parse` envelope,故对象天然作对象到达 JS,**prelude 零改**;`call_agent` 仍回
  String,包成 `Value::String` 过同一 envelope。program 最终 `return` 值仍串化(`wrap_source` 不变)。

isError 语义:结构化路径同字符串路径——isError → 抛异常(program `try/catch`),故解析出的
CallToolResult 只在成功时到手、不带 isError:true(TS 类型省略 isError)。

**真 key 验收(anthropic 轨,已过)**:stub `analyze(word)` 返回带 `structuredContent{word,vowels,length}`
的 CallToolResult;模型写 program `const r = await tools.stub__analyze({word:"elderberry"}); return
{vowels: r.structuredContent.vowels, length: r.structuredContent.length, firstText: r.content[0].text}`
→ `{"vowels":3,"length":10,"firstText":"elderberry: 3 vowels"}`,**直接读结构化字段、免解析**,全链
(wire structuredContent → SourceOutput → 旁路 → Value → JS 对象)跑通。

### 挂账(切片 3 后)

- 无 code-mode-MCP 侧挂账。code-mode 其余挂账见 plan 24(budget / 后台 yield-wait / 保存复用),不在本 plan。

### 教训

写进 HANDOFF 教训 19:(a)同预算的两套机制(defer 渐进披露 + code-mode 工具→TS API)要
**组合**而非叠加后互相拆台——defer 也压 code-mode 面,别让新 surface 把省下的预算吐回;
(b)门分**发现门**(`locked`,可按"来源已具备能力"豁免)与**安全门**(deny/权限/沙箱,绝不豁免),
`from_program` 只关发现门。
