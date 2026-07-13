# Plan 24 — code mode(代码编排 / CodeAct)备忘

> **备忘,非活动任务**。这里只记方向、关键决定与回源待办;开工前必须先做回源
> (教训 11:这是安全+架构大件,绝不能凭印象实现),回源结论回填本文件后再切片。
> 开工前也读 docs/plan/HANDOFF.md。

## 是什么

模型不再一轮一个 `tool_use`,而是**写一段程序**编排工具/子 agent:fan-out 并行、
pipeline 串接、loop/filter/错误处理都在代码里;工具以函数/API 暴露给程序;程序在
受控 JS 解释器里跑,**中间结果留在程序变量里、不回灌上下文**,只有最终产出回模型。

业界同一族(CodeAct / code-as-orchestration),三个直接参考:
- **cc dynamic workflows**(2026-05-28 上线):模型写 JS 编排后台并行子 agent,
  `ultracode` 关键词 / `/effort ultracode` 触发,`/workflows` 观察,结果可存成命令
  复用。文档 https://code.claude.com/docs/en/workflows.md 。
- **codex code mode**:用 **V8**(`用 v8` ≈ rusty_v8 / deno_core 的 ops 桥)。
  **具体实现未回源,勿凭印象**。
- **Anthropic "code execution with MCP"**:模型写 TS 调 MCP 工具当类型化 API,
  在沙箱跑——token 效率与组合性的原始论证。

## 为什么值得(收益)

- **往返/token**:100 项的循环 = 一段程序,而不是 100 轮 `tool_use` + 100 份结果回灌。
- **确定性**:控制流、数据变换、验证循环用代码表达,不靠模型逐轮推理。
- **规模**:大批处理(迁移、审计、fan-out)不烧主上下文——正是 plan 17 子 agent 之上
  缺的"确定性编排层"。这是比 plan 17 挂账"异步派发"更直接的杠杆。

## 与 kloop 极简 ethos 的张力(先摆明)

嵌 JS 引擎是**重依赖**(尤其 V8:编译时间、二进制体积),和 kloop 一贯的最小主义
(躲 chrono 手写 civil_from_days、拒 vendor CustomTerminal)相反。所以引擎选型是本
plan 第一个真决定,不是默认抄 codex 的 V8。

## 关键决定(开工时定 / 回源后定)

1. **引擎选型(最大决定)**:
   - **V8**(rusty_v8 / deno_core):codex 路线,重,但快 + 完整 + model 最熟;
   - **QuickJS**(rquickjs):轻量可嵌,同样的 JS-fluency 收益,体积/构建成本远低于 V8;
   - **Boa**(纯 Rust):免 native 构建,但慢 / 不完整;
   - 非 JS DSL(Starlark / Lua `mlua`):更轻但 model 不熟、生成质量差,**倾向弃**。
   - 倾向:先认真评估 **QuickJS**(合极简)对 **V8**(生态成熟);回源看 codex 为何
     选 V8(是真需要那个档位,还是顺手)。
2. **安全模型(本 plan 命脉)**:JS 必须是**纯编排器**——自身碰不到 FS / 网络 / 进程,
   一切副作用只能经 Rust op 回调;**每个 op 调工具必须重进 kloop 的
   `dispatch_tools`/`run_one` 链(hooks → 权限门 → 沙箱)**,一个都不能绕。ops 模型
   天然支持(JS 只见 Rust 显式暴露的东西)。要定:op 层怎么把 program 里的工具调用
   喂回权限判定(含 `sandbox_auto_allow` / escalation);program 里的并发
   (`Promise.all`)怎么映射到 kloop 的并发批与审批串行化。
3. **工具暴露形态**:内置工具 + MCP 工具怎么变成 JS 可调 API(命名规范、
   schema → TS 类型声明喂给模型?、结果对象形状、错误 → 异常还是返回值)。
   cc / Anthropic 都给工具生成类型化 API。
4. **与子 agent 的关系**:program 能否 spawn 子 agent(task)?cc dynamic workflows
   的本质就是编排子 agent。倾向 program 里的 `agent()` = 复用 plan 17 的 task 缝
   (含自定义 agent 类型)。深度限沿用(program 里的 agent 不能再嵌 program?)。
5. **触发形态**:cc 是关键词 / effort 档 / 内置 workflow。kloop 对应:一个内置工具
   (模型主动"我要写段程序")还是一个模式?判定"何时写 program vs 逐轮 tool_use"
   交给模型还是提示词引导。倾向:先做成一个工具(`run_program`/`workflow`),模型
   显式调用,最小面。
6. **归属/依赖**:JS 引擎是重依赖,放**独立 crate**(`kloop-codemode`?)——依赖驱动的
   crate 边界,符合 kloop-web(reqwest)/kloop-mcp(线协议)先例;core 保持无引擎依赖,
   通过一个 seam(类似 ToolSource)接入。
7. **产物 / 观察**:program 后台跑还是同步?进度怎么给用户看(cc 的 `/workflows`
   等价物)?中间日志 / 每个 op 调用的 UI 呈现。最小版可先同步 + 收尾汇总。
8. **资源限**:program 超时、内存上限、op 调用数上限;失控 program 怎么杀
   (对齐后台 bash 的 watchdog / kill 语义)。

## 候选切片(开工时和用户定选哪几片)

1. **引擎嵌入 + 最小 op 桥**:选定引擎,一个内置工具(如 read_file)从 JS 调通,
   **过完整权限门**;program 超时 + 杀。
2. **工具全集暴露成 JS API**:内置 + MCP 工具都变成 program 可调,类型声明喂模型。
3. **编排原语**:`parallel()` / `pipeline()` / `agent()`(复用 task),并发映射到
   kloop 并发批与审批串行。
4. **触发 + UI 观察 + 资源限**:模型怎么发起 program、用户怎么看进度、失控保护。
5. **后台运行 + 保存复用**(cc 存成命令那套,最后做,有真实需求再上)。

## 不做 / 待定

- 不追 cc dynamic workflows 全家桶(effort 档自动化、复杂调度)。
- 不做非 JS DSL(model 不熟)。
- program 里再嵌 program(深度递归)不做,同 task 深度限。
- 不预抽象:引擎 crate 的 seam 等真嵌进去、第二个消费者出现再定形。

## 回源待办(开工必做,教训 11 + 14)

三家回源对齐,找收敛的"必然解"(教训 14):
- **codex code mode**:引擎嵌入方式(rusty_v8 还是 deno_core?)、tool 桥形态、
  **op 层怎么过权限/沙箱**(最关键)、并发模型、资源限、触发形态。
- **cc dynamic workflows**:脚本 API 面(agent/parallel/pipeline/log/phase 等)、
  子 agent 编排细节、后台 + 观察、保存复用、审批如何在 program 里发生。
- **Anthropic code-execution-with-MCP**:工具 → 类型化 API 的具体形态与 token 论证。

## 回源结论(2026-07-13,三家真读 + Anthropic 博客,file:line 见下引)

三家各自深读代码(codex Rust/V8 一手实现、cc 逆向 TS `packages/workflow-engine`、
Anthropic 工程博客),交叉核对后收敛信号很干净。核心文件索引在本节末。

### 引擎无关的"必然解"(教训 14 收敛点,kloop 照搬)

1. **嵌套工具调用重入既有工具审批/沙箱链(命脉)**。codex:`ToolCallSource::CodeMode`
   只是来源标签(tracing/取消通知/计时用),**不参与审批/沙箱决策**;嵌套调用回灌同一个
   `ToolRouter`(`core/src/tools/code_mode/mod.rs:285-325 call_nested_tool` →
   `parallel.rs:104-172 handle_tool_call_with_source` → `router.rs:212-245` → 同一
   `registry.dispatch_any`),与顶层 `ToolCallSource::Direct` 落到同一 handler、同一审批/
   沙箱。cc:透传同一个 `canUseTool`(`WorkflowTool.ts` → `hostHandle.ts` →
   `claudeCodeBackend.ts:211,302` → `useCanUseTool.tsx`),后台 worker 优先跑自动检查、
   判不了才打断用户弹框(`useCanUseTool.tsx:154-175`)。**CodeAct 的安全性完全寄生在既有
   工具审批/沙箱上,program 只是把 N 个工具调用批处理,每个调用仍单独过门。** kloop 对应:
   op 回调 `tools/mod.rs` 的 `run_one`(allowlist→deferred锁→hooks→权限→沙箱→execute→
   post_tool 全链),并发批对应 `dispatch_tools`。这是**必须照抄的一条**。
2. **异步 Promise 桥**。codex 因裸 v8 手写全套:JS 调 `tools.x()` → 建 `PromiseResolver`
   存进 map、发 `RuntimeEvent::ToolCall`、立即返 pending Promise(`runtime/callbacks.rs:13`);
   async 侧 spawn tokio task 执行、结果经命令通道 `ToolResponse`/`ToolError` 回灌、
   `resolver.resolve/reject`(`module_loader.rs:66-101`)+ `perform_microtask_checkpoint`。
   **rquickjs 原生 tokio async(Rust future ↔ JS ES6 Promise 双向互转)能省掉这套手写机械**
   ——`agent()`/工具函数作为 async Rust fn 直接绑成 JS async 函数,`Promise.all` 天然映射。
3. **给模型喂 TS 类型声明**。codex `description.rs:372-560`:每个工具生成
   `declare const tools:{ name(args:<InputTS>):Promise<<OutputTS>>; }`,JSON Schema→TS
   递归渲染(enum/const/anyOf/object/array,property description 变 `//` 注释);MCP 工具
   渲成 `CallToolResult<T>`。Anthropic 同族(`./servers/<server>/<tool>.ts` типизированный)。
   **纯字符串处理、引擎无关,直接搬,显著提升模型调用正确率。**
4. **工具暴露成全局对象 + 命名归一化 + 能力裁剪**。codex:全局 `tools` 对象、名字
   `normalize_code_mode_identifier`(`-`→`_`,`description.rs:324-346`)、删
   console/Atomics/SharedArrayBuffer/WebAssembly/import(`globals.rs:16-19`、
   `module_loader.rs:213-235` 一律拒模块)。cc:只注入 8 个标识符 + Date/Math,不给任何
   FS/net 句柄。**program 是纯编排器,副作用只能经工具/agent 发生。**
5. **JSON 作为值边界**。codex `value.rs:178-205`(v8::json stringify/parse ↔ serde_json);
   rquickjs 有 serde 集成更干净。
6. **API 面 + pipeline/parallel 语义**。cc 8 hooks(`agent/parallel/pipeline/phase/log/
   workflow` + `args/budget`),`meta` 纯字面量(正则抠出单独 eval,`script.ts:28-104`)。
   **`pipeline` 无 barrier(item 各自穿过所有 stage)/ `parallel` 有 barrier(`Promise.all`)**
   是全套设计最有价值的一刀,单项失败吞成 `null` 不 reject(`hooks.ts:222-270`)。
7. **中间结果留代码变量、只回最终产出**(Anthropic 的原始 token 论证:拉 1 万行电子表格只
   `log` 前 5 行,150k token→2k token / 98.7%)。**这是 code-mode 相对 kloop 已有 deferred+
   tool_search(plan 16 已覆盖"按需读定义"那半)的真正增量。**
8. **caps + budget 硬顶 + progress 事件流 + 终态 notification 回灌**。cc:总 agent 1000 /
   单次 items 4096 / 并发默认 3 上限 16(`constants.ts`,注:`min(16,cores-2)` 是旧实现)、
   budget 按 output token 硬顶(`agent()` 前 `assertCanSpend()`)、run/phase/agent 事件带
   runId、终态注入 `<task-notification>`(`notifications.ts`)。全部引擎无关,按需搬。

### 分歧(judgment,不必都抄)

- **cc 根本不沙箱脚本**:执行引擎就是 `new AsyncFunction(...params, body)`,hooks 当位置参数
  注入,和主进程共享 Node 全权限;只 shim Date/Math 保证 resume 确定性,白纸黑字
  "same trust level as LLM, no security"(`script.ts:172-179`)。对 kloop 而言用 QuickJS
  **反而是升级**——真隔离、无 FS/net、原生内存/超时配额,该把引擎当真隔离层用,不继承 cc
  的"不设防"态度。
- **codex 后台 cell + yield/wait + observation frontier**:长任务软 yield(默认 10s)不
  阻塞 turn、返 cell_id、模型用 `wait` 续看(`cell_actor/` 一半复杂度在这)。**kloop 建议先做
  "同步 program:跑完返回累计输出"**,yield/wait 作后续可选增量(对齐后台 bash 的思路,但
  code-mode 首版不做)。
- **schema 强制方式**:cc 后端**弃用**了"强制调 StructuredOutput 工具"(8/12 agent 拒绝
  收尾),改成 prompt 追加 JSON Schema 指令 + 从最终文本提取 JSON(`claudeCodeBackend.ts:
  266-287`),且**并不做 Ajv 校验**(只判 plain-object)。ultracode 手册里"forced to call"是
  过时描述。kloop 若要 agent() 结构化输出,记此教训。
- **journal + agentCallKey(sha256(prompt+规范化params)) 做 resume**:cc 语言无关,可抄;
  但 kloop 首版 program 若同步跑,resume 优先级低,记挂账。
- 注入 framing、worktree isolation、adapter registry:cc/codex 特有产品面,按需。

### 引擎选型(本 plan 第一个真决定)——倾向 **QuickJS(rquickjs)**

两个 agent 独立收敛到 QuickJS,理由一致:

- **V8(codex 路线)是重工程包袱**:`v8="=149.2.0"` 裸 crate(非 deno_core,codex 自己就
  放弃了 `#[op2]`/事件循环手写绑定)、**~40MB 链接进二进制**(`core/Cargo.toml:133-137` 原注)、
  feature 三档门控默认不链、还得写 **~3000 行 sidecar/remote_session** 把 V8 移出主二进制 +
  进程隔离、`cell_actor` 一半复杂度是 observation 状态机 + 每 cell 独立 OS 线程 +
  `IsolateHandle` 强杀。核心引擎 ~3400 行,production:test≈1:1。**生产级重工程,与 kloop 极简
  主义(躲 chrono、拒 vendor CustomTerminal)正相反。**
- **cc 不用 JS 引擎**(Node AsyncFunction),不可迁移到 Rust。
- **QuickJS(rquickjs)对 kloop 是升级不是妥协**:体积几百 KB(bundled quickjs + `cc` 编译,
  远轻于 V8);已核实原生具备 `AsyncRuntime`/`AsyncContext`(future-aware lock,专为 async
  Rust)、**Rust future ↔ JS Promise 双向互转**(省掉 codex 手写 resolver map)、
  `set_memory_limit`、`set_interrupt_handler`(执行中定期回调,返 true 抛不可捕获异常交回
  控制权——超时/取消一步到位,不用 OS 线程强杀)、serde 集成(JSON 边界干净)。
  **codex 恰恰缺内存上限(它的短板),kloop 用 rquickjs 一行补上。** Boa(纯 Rust 免 native
  构建但慢/不完整)、非 JS DSL(model 不熟)按原备忘弃。
- **代价/张力**:rquickjs 是 kloop **首个为功能引入的 native(C 编译)依赖**(`similar` 是首个
  纯 Rust 功能依赖)。QuickJS 用 `cc` crate 编译 bundled C 源(bindgen 可用预生成 bindings
  避开),构建成本远低于 V8,但确实引入 C 工具链依赖。**这是开工时要跟用户确认的取舍点。**

### code-mode 在 kloop 的接缝(基于现有架构)

- 归属:**独立 crate `kloop-codemode`**(依赖驱动边界,同 kloop-web/reqwest、kloop-mcp/线协议
  先例);core 保持无引擎依赖,通过一个 seam 接入。但**工具回灌必须能重入 core 的 `run_one`/
  `dispatch_tools`**——这与 kloop-web 那种"core 零改动 ToolSource"不同(ToolSource 是叶子,
  code-mode 要反向回调 core 的门)。故 seam 形态待定:要么把 op 需要的"过门执行一个工具调用"
  能力抽成 core 上的一个 trait 对象注入引擎,要么 code-mode 工具直接实现在 core 里、只把
  JS 引擎薄封装在 kloop-codemode。**开工时定**(倾向后者:引擎在独立 crate,`exec` 工具的
  op 层留在 core 以直接够到 `run_one`)。
- 触发:一个内置 Freeform/普通工具(codex 叫 `exec`,cc 叫 `Workflow`)——模型显式调用、
  裸 JS 源码。倾向最小面先做一个工具(名字开工定,如 `code`/`run_program`)。
- 递归:`execute_tool` 已是类型擦除 future(`tools/mod.rs:517`),code-mode 工具作为一个 arm、
  内部 op 回调 `run_one`,结构上与 `task→run_turn→dispatch_tools` 完全同构,递归 Send 已解。
- `agent()`:复用 `task.rs` 的子 agent 缝(含自定义 agent 类型);深度限沿用(program 里的
  agent 跑在 depth+1、不能再嵌 program,同 task 深度限)。
- 并发:JS `Promise.all` 里多个工具调用 → 收敛到 kloop 现有 `dispatch_tools` 并发批语义
  (连续只读成批、遇写切断)+ 审批串行化(plain `CliApprover` 已有 Mutex)。

### 核心文件索引(供实现追溯)

- codex 引擎/桥:`codex-rs/code-mode/src/{runtime/mod.rs,runtime/globals.rs,
  runtime/callbacks.rs,runtime/module_loader.rs,runtime/value.rs,v8_init.rs}`;actor/会话:
  `{cell_actor/mod.rs,cell_actor/types.rs,session_runtime/mod.rs,service.rs}`;协议/描述:
  `code-mode-protocol/src/{lib.rs,description.rs}`;**审批命脉**:`core/src/tools/code_mode/
  {mod.rs,delegate.rs,execute_spec.rs}` + `core/src/tools/{parallel.rs,router.rs,context.rs}`;
  依赖门控:`code-mode/Cargo.toml`、`core/Cargo.toml:133-137`。
- cc 引擎:`packages/workflow-engine/src/engine/{script.ts,hooks.ts,runWorkflow.ts,
  concurrency.ts,budget.ts,journal.ts}` + `src/constants.ts`;宿主/spawn:
  `src/workflow/backends/claudeCodeBackend.ts` + `src/workflow/{service,ports,hostHandle,
  notifications}.ts`;触发 skill:`src/skills/bundled/ultracode.ts`;权限:
  `src/hooks/useCanUseTool.tsx`。
- Anthropic:engineering/code-execution-with-mcp(工具→`./servers/<server>/<tool>.ts` 类型化
  API、150k→2k token、中间结果留代码变量)。

## 测试(方向)

引擎沙箱证明(JS 直接 FS/网络访问失败,只有经 op 才行);op 调工具过权限门
(被 deny 的工具在 program 里同样被拒、被沙箱兜的同样兜);并行编排真并发(文件
屏障式,同 plan 17);资源超限杀 program;program 结果只回最终产出、中间态不进
上下文(录 MockRequest 断言)。

## 完成标准

按所选切片;fmt/clippy/test 全绿;真 key 至少一次"模型写 program 编排多工具/
子 agent 跑通闭环、权限门在 op 层生效";README、HANDOFF 更新;未选切片记挂账。

## 完成记录(首片,2026-07-13,提交 ee4b54a)

开工时用户拍板两件事:**引擎 = rquickjs(QuickJS)**(过程中追问了 codex 为何用 V8、
`v8_enable_sandbox` 的意义、跑完能否完全释放实例——结论均导向 QuickJS:V8 是血统/平台红利
非本质需求、其内存 cage 对"模型写的编排代码"威胁模型意义低、QuickJS 无 V8 全局态能整个干净
释放);**首片范围**含 `agent()`,其余挂账。

**落地**:
- 新 crate **`kloop-codemode`**(`crates/codemode/`,只依赖 rquickjs + serde_json + tokio):
  `run_program(source, tool_names, bridge, cancel, limits)`。每次新建 `AsyncRuntime` 跑完 drop;
  `set_memory_limit`/`set_max_stack_size`/`set_interrupt_handler`;host 函数 `__call_tool`/
  `__agent`/`__log`(Async 闭包,返回 JSON envelope `{ok,value|error}` 避免跨 await 抛异常),
  JS prelude 在其上建 `tools`/`agent`/`log`/`parallel`;源码包成 `(async()=>{…})()` 令顶层
  `await`/`return` 合法且结果恒串化。`HostBridge` trait = engine↔host 缝(`call_tool`/
  `spawn_agent`/`log`)。
- **`core/src/tools/codemode.rs`**:`exec` 工具 + `CoreBridge`(实现 `HostBridge`,回灌
  `super::run_one` 全链 gate 与 `task::task_tool`;`RwLock` 复刻并发规则)+ JSON Schema→TS
  生成(`exec_def`/`ts_type`/`ts_object`)。接线:`tools/mod.rs`(`mod codemode` + execute_tool
  arm + tool_defs depth-0 push exec)、`permissions.rs`(`exec`→readonly 自动放行)。
- **测试**:引擎 13(sandbox/并发 barrier/parallel/agent/log/结果强制/资源三杀/import 拒)+
  core 9(gate 重入命脉/agent 派子/中间态不泄/异常不带 log/观测性 log 实时+op 行按序/TS
  生成/程序面排除)+ agent 级 1(exec 经 run_turn,下一请求只带 return 值)。**fmt + clippy
  (-D warnings)+ 全量 354 测试全绿。** `cargo run --mock` 冒烟通过。

### 追加片:进度观察(log 实时化,同会话续做,提交 065c911)

用户"继续"→选做 UI 观察片。厘清后发现 op 调用本就通过 `run_one` 发 UI 生命周期(plain
真机验收里 bash/write/agent 都显示了)、TUI/server 消费同样事件——**真缺口只有 `log()`**:原
实现把 log 收集进 Vec、跑完随结果回来(既不实时、又污染上下文)。改为 **`CoreBridge.log`→
`ctx.ui.note` 实时露出、且不再进 tool_result**(结果只回 `return` 值;对齐 cc narrator 模型 +
code-mode 省 token 初衷),`exec_tool`/`format_output` 随之简化(去掉 logs 收集)。三前端零改动
(复用既有 note 通道)。加观测性测试(RecordUi 断言 log 实时成 note、op 的 tool_start 行按序
夹在两条 log 中间、结果只 return 值),error 测试改断言"异常上浮但 log 不进结果"。真 key 验收:
plain 下 `log('scanning')` → `[bash …]` → `log('counted 5')` → `entry_count=5` 完整实时 trace,
模型只拿到 return 值。**踩坑**:改完只跑了 `cargo test`(重建 lib)没重建 binary,首次真机跑用了
旧二进制(log 仍收集不 note)看着像没生效——`cargo build -p kloop` 后即对。挂账收窄:UI 观察
只剩"更富的进度树(cc `/workflows`)",kloop 现为扁平实时 trace,够用。

**教训沉淀**:HANDOFF 教训 17(参考实现的复杂度先归因"功能本质 vs 底座连带",换底座能砍掉
后者)。实现踩坑:同步死循环在 `eval` 阶段被 interrupt(非 await 阶段),故 stop_reason→错误
消息的映射两处都要覆盖;args 跨 JS 边界走 JSON 字符串(prelude `JSON.stringify` / host 侧
`serde_json::from_str`)避开 rquickjs↔serde 转换;`Function::new(ctx, Async(closure))` +
`ctx.async_with(async |ctx| …)`(非 deprecated 宏);`gate.read_owned/write_owned()` 给 'static
future 用的 owned guard。

**挂账(留后续 plan)**:① ~~真 key 验收~~ **已完成**(anthropic/sonnet-5:模型写 program 用
`Promise.all` 并发跑两个 bash + 一个被 `AGENT_DENY` 在 op 层拒掉的 write(writeBlocked=true、
文件未创建)+ 一个 `agent()` 子 agent(返回 DELEGATED),闭环返回 JSON;gate 在 op 层生效)。
② MCP 工具暴露给 program(现只内置)+ deferred 集成。③ ~~`pipeline()` 原语~~ **已完成**
(追加片,见下)+ token `budget`(仍挂账)。④ ~~UI 进度观察~~ **已做 log 实时化 + op 可见**
(追加片,见上);只剩"更富的进度树
(cc `/workflows`)",kloop 现为扁平实时 trace。⑤ 后台 program + `yield`/`wait`(codex
observation frontier;现同步跑完返回)。
⑥ 保存复用 + journal resume(cc 具名 workflow / agentCallKey)。⑦ `Limits` 走配置/env(现
硬编码 64MiB/512KiB/5s)。⑧ program 里 `agent()` 的深度限沿用 task(depth≥1 bail),但 `exec`
本身只在 depth-0(同 task);受限 agent 类型不给 exec。

### 追加片:`pipeline()` 原语(同会话续做,提交 8ce4cd6)

补齐 cc 两个核心编排原语的另一半(首片只做了 `parallel`)。`pipeline(items, ...stages)`——
**每项作独立 async 链穿过所有 stage、stage 间无 barrier**(快的项可到 stage 3 而慢的项还在
stage 1),某 stage 抛错该项落 `null` 并跳过其余 stage(与 `parallel` 一致),stage 回调收
`(prev, item, index)`。纯 JS prelude helper(`build_prelude` 里 `Promise.all(items.map(async
… for stage of stages …))`),零引擎改动;`exec` 的 TS 声明加一行 `declare function pipeline`。
测试 4 个(引擎层):逐项穿两 stage、失败只 null 该项、stage 三参、**无-barrier 证明**(item 1
的 stage 1 等一个只在 item 0 的 stage 2 才打开的 gate——按 stage barrier 必死锁,逐项独立链则
流通)。真 key:`pipeline([2,3,4], n=>n*n, sq=>bash('echo '+sq))` → `["4","9","16"]`。fmt +
clippy + 全量 358 测试全绿。挂账收窄:③ 只剩 token `budget`。
