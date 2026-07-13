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

## 测试(方向)

引擎沙箱证明(JS 直接 FS/网络访问失败,只有经 op 才行);op 调工具过权限门
(被 deny 的工具在 program 里同样被拒、被沙箱兜的同样兜);并行编排真并发(文件
屏障式,同 plan 17);资源超限杀 program;program 结果只回最终产出、中间态不进
上下文(录 MockRequest 断言)。

## 完成标准

按所选切片;fmt/clippy/test 全绿;真 key 至少一次"模型写 program 编排多工具/
子 agent 跑通闭环、权限门在 op 层生效";README、HANDOFF 更新;未选切片记挂账。
