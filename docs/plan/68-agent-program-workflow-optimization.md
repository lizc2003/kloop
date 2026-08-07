# Plan 68 — Agent / Program / Workflow 端到端优化

> 状态：✅ 已完成（2026-08-07；plan68 提交，SHA 以本文件所在提交为准）
>
> 依赖：Plan 24、26、51–53、59、66

## 背景

真实 `claude-sonnet-4-6` 与 OpenAI Chat `gpt-5.5` 概念测评均通过：8 个原语选择场景和 12 个运行时边界判断全部正确。模型能理解“开放式认知任务用 Agent、固定工具编排用 Program、用户明确授权的 multi-agent orchestration 用 Workflow”，但自由概括仍可能误称 Program 不可恢复，或把 Workflow `phase` 当成幂等检查点。模型理解不能代替运行时契约。

审查同时确认四个实现缺陷：

1. QuickJS 把 `__call_tool` / `__agent` / `__log` / `__phase` 暴露给模型源码，Program 可绕过 `tools` catalog 调递归 runner 和后台控制工具。
2. journal 用全局 `(seq,key)` 标识 `agent()`；并发 pipeline 下游按上游完成顺序分配 seq，恢复时会漏命中或错配重复调用。
3. `run_program` 文案要求 same source，运行时却未持久化、比较 source。
4. 后台 Program 只返回 `program-N`，未返回失败恢复所需的 durable `run-*`。

本计划端到端修复上述边界，同时补齐 agent fan-out、后台结果上下文预算和双 provider 真执行验收；不触碰 Plan 67 的 PDF 工作。

## 已拍板契约

- raw host bridge 必须对模型源码不可见；Rust bridge 仍以精确允许集做第二层拒绝，不能把 JS 包装或 prompt 当安全边界。
- journal v2 以稳定拓扑 call ID + 完整结构化输入匹配；pipeline 保持 item-local 流水线，不为恢复稳定性增加 stage barrier。
- 并发 helper callback 使用显式 scope 中的 `agent`；并发作用域里无 scope 的全局 `agent` fail closed。顶层确定性顺序调用继续兼容。
- Program 恢复必须 byte-identical source；旧 run 缺 manifest 时拒绝。Workflow 继续允许编辑 managed script 后恢复，只复用 call ID 与完整输入均未变化的调用。
- `phase` 只是进度标签；journal replay 只承诺相同调用的 best-effort memoization，不承诺模型文本确定、外部副作用 exactly-once 或 workspace state 幂等。
- 每个 Program/Workflow 最多同时运行 16 个 live `agent()`；总 agent 1000、单 helper 4096 items、session detached execution 8 的既有硬上限保留。
- background Agent/Program 的超大成功结果落到现有 offload store，只回灌有界预览和 `read_offloaded` 指针；Workflow 继续使用 bounded summary + `result.json`。
- `wait_for_activity` 仍是无 ID、non-draining 的 session activity barrier；`program-N` 只用于 stop/lifecycle，`run-*` 只用于 Program resume，`workflow-N` 与 `wf_*` 同理。

## 实施

1. `kloop-codemode` 在私有闭包捕获 host functions，源码执行前删除 raw globals；core 把 Program 实际 catalog 交给 `CoreBridge` 并在 dispatch 前校验。
2. 将 `HostBridge::call_agent` 改为稳定 call ID；parallel/pipeline 建立可嵌套 scope，journal 改成 versioned structured v2，v1 只按安全 cache miss/拒绝策略处理。
3. Program run 创建时原子保存 source manifest，resume 在 spawn 前字节比较；前后台共用路径，后台响应同时返回 transient 与 durable ID。
4. `Limits` 增加 live agent concurrency，bridge 用取消感知 semaphore；复用 History spill helper约束后台 Agent/Program 成功回灌。
5. 更新模型可见 TypeScript API、README、capability report 和 HANDOFF；修正旧文档中“只有 TUI autowake”的陈述，plain/native server 也会由 inbox activity 自动 delivery。
6. 增加默认 ignored 的 native-server real evaluator，以协议事件、tool-use/result 配对、artifact、唯一 terminal/delivery 和 journal hit 计数验收三原语，不以最终自然语言自评。

## 验证矩阵

- engine/core：raw bridge 逃逸、source-tool 碰撞、稳定拓扑、反向完成顺序、重复输入、nested helpers、无 stage barrier、v1、source mismatch/缺 manifest、双 ID、semaphore peak/cancel、超大结果 offload。
- parity：Plan 52/53/59 报告及 background typed-stop 全部不回归；headless Workflow 继续 fail closed。
- 全量：fmt、workspace all-target Clippy `-D warnings`、workspace tests、mock、refs verifiers、diff check。
- 真实 Anthropic：`KLOOP_PROVIDER=anthropic ANTHROPIC_MODEL=claude-sonnet-4-6`。
- 真实 OpenAI Chat：`KLOOP_PROVIDER=openai OPENAI_MODEL=gpt-5.5`；当前代理不兼容 Responses rail，不把 transport 限制归因于模型。
- key、代理地址、raw provider response/transcript 只在 gitignored 临时环境中使用，不进入输出和提交。

## 完成记录

- raw QuickJS bridge 已移入私有 closure，Program Rust bridge 再以精确 catalog fail closed；递归 runner、typed stop/wait、后台 shell query/stop 和 `bash {background:true}` 都不能从 Program 绕过父会话控制面。
- journal v2 已改为稳定 topology call ID + 完整结构化 input；显式 callback scope 支持 nested parallel/pipeline，反向完成顺序和重复 prompt 不串线，pipeline no-stage-barrier 保持；v1 只安全 miss/拒绝。
- Program 新 run 已原子保存 source/manifest，resume 在任何 spawn 前 byte-compare；后台响应同时给 `program-N` 与 `run-*`。Workflow 仍支持 edited managed script + `wf_*` resume。
- Program/Workflow live Agent 默认 16 路 semaphore，总量 1000、单 helper 4096 保留；排队取消、journal hit 和峰值均有测试。后台 Agent/Program 超大 success 已复用 History offload。
- 模型说明、README、capability report 与 HANDOFF 已同步三原语分工、scope API、ID、phase/best-effort replay 和 TUI/plain/native-server idle delivery。
- 新增默认 ignored 的 native-server evaluator；真实 `claude-sonnet-4-6` 与 OpenAI Chat `gpt-5.5` 均实际完成：1 次 direct Agent、2 次 same-source Program（只 spawn 1 次 child，第二次 journal hit）、1 次双 Agent Workflow、唯一 terminal/delivery 和 result artifact 校验。
- `cargo fmt --all --check`、workspace all-target Clippy `-D warnings`、`cargo test --workspace`、mock 六轮、full/corpus-only exact-binary verifier 与 `git diff --check` 全绿。真实凭据、endpoint 和 raw trace 未输出、未落入仓库。
- 提交：本次 `plan68` 收口提交（SHA 以本文件所在提交为准）。
