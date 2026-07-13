# Plan 27 — code mode:MCP 工具暴露给 program(备忘)

> **备忘,未开工**。开工前读 HANDOFF + `docs/plan/24-code-mode.md`(首片 + 三个追加片
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
