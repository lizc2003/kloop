# Plan 81 — Durable provider usage ledger 与累计 `/cost`

> 状态：✅ 已完成（2026-08-12；提交 SHA 以本文件所在提交为准）
>
> 基线：`e03ed9f`（Plan 80 后续修正提交）
>
> 依赖：Plan 23、Plan 39、Plan 77

## 背景

Pi（调研基线 `earendil-works/pi@2e4d23959485279aa2da1a45103de2ea22d46395`）的新 AgentHarness 把逐响应 usage record、append-only log、reducer 与 session storage 明确分层。这个机制可以作为 kloop 的设计参考，但 Pi 不成为依赖或架构上游，也不复制其源码、第三方资产、扩展信任模型或远程 runtime。

kloop 已有更适合自身的落点：三个 provider rail 都把 terminal usage 归一为 `kloop_protocol::Usage`；`SampleOk` 已把它带到 core；rollout 已支持 append-only、torn-tail repair、resume、fork 与 compaction；`/cost` 已有稳定的命令入口。当前缺口是 usage 到达 core 后只被折成 `History` 的 context anchor，既不逐响应持久化，也无法在 resume/fork 后报告当前 transcript 的累计 provider-reported token 分类。

Plan 77 已明确把 usage ledger 留给独立计划。本计划只补这一个缺口，不重做 native event recovery、agent/task runtime 或 provider adapter。Plan 67 的 PDF 原生分页读取仍保持既有暂停原因与路线位置；Plan 81 是用户指定的 Pi-derived 独立切片，不取代 PDF 总体优先级。

## 契约

### 1. Context estimate 与 durable ledger 是两类状态

- 继续复用 `kloop_protocol::Usage` 的四个 canonical 字段：
  - `input_tokens`
  - `output_tokens`
  - `cache_read_input_tokens`
  - `cache_creation_input_tokens`
- `History::usage_anchor`、`History::estimated_tokens()`、`Event::Usage(u64)` 和 server `thread/tokenUsage/updated` 继续只表示当前 context estimate；其 resume/fork/compaction reset 语义不变。
- 新 ledger 只累计当前 rollout 中结构验证通过、且 terminal 携带 `Some(Usage)` 的 provider response。不得用 chars/4 估算补造 record。
- `Usage::total()` 继续只给 context anchor 使用；ledger 保留四个原始分类，不把四项简单相加称为 billable total。
- 空 ledger 表示 `unavailable`；provider 明确报告的一条全零 usage 是 available 的四个零，不能混淆。

### 2. Rollout 增加逐响应事实记录

增加 additive `provider_usage` line，最终 wire 至少固定为：

```jsonc
{
  "type": "provider_usage",
  "id": "session#7",
  "parent": "session#6",
  "ts": 0,
  "model": "actual-model",
  "operation": "sampling",
  "usage": {
    "input_tokens": 120,
    "output_tokens": 30,
    "cache_read_input_tokens": 80,
    "cache_creation_input_tokens": 0
  }
}
```

`operation` 当前只有 `sampling | compaction`。model/operation 进入 raw record，供审计与后续独立统计使用；本计划不做分组 UI。

普通 sampling 的记录边界：

1. `Sampled::Ok` 先通过既有 `validate_assistant_result`；validation 失败不写 message 或 ledger。
2. validation 通过且 terminal usage 为 `Some` 时写一条 `sampling` record，model 取实际 `active_model`。
3. `EndTurn`、`ToolUse`、`OutputLimit`、`Refused`、`Filtered`、`Incomplete` 都是 provider 已完整结束并通过结构验证的 response；若携带 usage，均保留 provider 报告，不因后续业务结局而丢账。
4. cancellation、context overflow、transport/protocol failure、partial-output failure、未到 terminal 或 `usage: None` 不写 record，也不猜测 provider 是否收费。
5. retry/fallback 期间没有 terminal usage 的失败 attempt 不记录；最终有效 terminal response 只记一次，并保存实际 primary/fallback model。

Compaction 的记录边界：

- `sample_summary` 把 terminal `Option<Usage>` 与 summary 一起带回 `run_compaction`。
- 只有 outcome 为 `EndTurn`、summary 非空且 compaction 将被接受时，才写 `operation: compaction` 的 record；随后走既有 `replace_all`/`compacted` marker。
- 预测、响应式和手动 `/compact` 共用 `run_compaction` 这一处记账 seam，调用方不重复记录。
- `usage: None` 不阻止成功 compaction，但 ledger 不增加；空 summary、非成功 terminal、取消和 provider failure 均不改 history 或 ledger。

usage record 位于对应 assistant message或 `compacted` marker 之前。两条 JSONL line 不是原子事务，恢复契约明确为：

- 完整 usage line 后下一条 torn：usage 仍可恢复，因为 provider call 已经完成；assistant/compaction state 按现有 intact-tail 规则处理。
- usage line 自身 torn：忽略该 line，writable resume 继续由现有逻辑截断到最后 intact boundary。
- 不增加 request id、response hash、dedupe 或 exactly-once 声明。

rollout append 失败沿用 `History::persist` 的降级：打印既有 secret-safe warning、卸下 writer、session 继续在内存中运行；当前进程 ledger 仍累计，但重启后只能恢复成功写入的 intact records。记账落盘失败不能推翻已完成的模型响应。

### 3. Ledger 的 owner、resume、fork 与 clear

- 新增窄模块 `core/src/usage.rs`，定义 `UsageOperation`、`ProviderUsageRecord`、`UsageLedger`；aggregate 使用 checked arithmetic，不 wrap/saturate。
- `History` 是 live ledger 的唯一 owner；不增加 sidecar、SQLite、全局 registry 或 `Arc<Mutex<_>>`。
- 扩展既有 `parse_session`，在同一次 rollout replay 中同时重建 messages/runtime/terminals 和 ledger；不做 usage-only 第二次文件扫描。
- 用结构化返回值（例如 `ResumedSession { messages, provider_usage, rollout }`）替代 `resume_session` 当前 tuple，避免 CLI、TUI、server 漏传 ledger。
- `History::new` 从空 ledger 开始；`resume` 安装 replay ledger；`rebase` 安装 fork baseline；`replace_all` 只改 provider history和 context anchor，不清 ledger。
- `/clear` 仍是在同一 rollout 中 append compacted-to-empty，因此清 messages/context anchor，但保留 transcript 累计 usage。
- fork 继续物理复制 cut 前 raw prefix并重新 envelope：cut 前 usage 成为 branch baseline，cut 后 usage 不继承；source 与 fork 后续独立累计；fork-of-fork 不做跨文件全局去重。
- 主 History 不汇总子 Agent、Program、Workflow、MCP 或其他 rollout。`/cost` 的“当前 transcript”不等于一次用户任务触发的所有 child 计算成本。

### 4. `/cost` 只报告 provider-reported token 分类

- 保留现有 `model:` 与 `context:` 行以及 context-window on/off 文案语义。
- 追加 `provider-reported usage across all models`，分别显示四个分类和 `N reported responses`。
- `model:` 仍表示当前配置模型；ledger 可跨实际 model，不能把累计值全部归因给当前 model。
- 空 ledger 显示 `unavailable`；一条全零 record 显示四个零。
- 不显示金额、货币、price、quota、budget、coverage 百分比或四分类伪 total。
- `/cost` 不增加参数，也不实时查询 provider。server 继续用既有 slash-command textual system output，不新增 typed RPC。

### 5. Public protocol 负契约

本计划不修改：

- `Event::Usage(u64)`；
- `thread/tokenUsage/updated`；
- Plan 77 snapshot 的 `tail.tokenUsage`；
- `SessionSnapshot`、`thread/read`、`thread/events/sync`；
- headless JSON event；
- Desktop DTO/adapter。

ledger 不进入 provider replay messages，不写入 public display event，也不从 event projection 反推。dormant session 不增加 usage RPC；resume 成 active History 后，`/cost` 才读取恢复出的 ledger。

## 实施

### 1. 建立 canonical ledger 类型

修改：

- `rust/crates/protocol/src/lib.rs`
- `rust/crates/core/src/lib.rs`
- `rust/crates/core/src/usage.rs`（新增）

工作：

- 给既有 `Usage` 增加 rollout 所需 serde 支持与 round-trip test，不在 core 重复定义四个 token 字段。
- 实现 operation/record/aggregate、reported-response count、checked accumulation，以及 empty 与 reported-zero 的区别。
- 不增加价格、金额或 billable-total helper。

### 2. 扩展 rollout 与 History 生命周期

修改：

- `rust/crates/core/src/rollout.rs`
- `rust/crates/core/src/history.rs`
- `rust/crates/cli/src/args.rs`
- `rust/crates/tui/src/lib.rs`
- `rust/crates/server/src/lib.rs`

工作：

- 增加 `provider_usage` variant 和 append API，并纳入 metadata、fork remeta、turn-boundary 等 exhaustive matches。
- 在 `parse_session` 的单次 replay 中重建 ledger；`Compacted` 不清 ledger，torn-tail repair保持一条共同边界。
- 引入结构化 `ResumedSession`，机械适配 CLI resume、TUI fork/rebase、server resume/fork 与测试。
- `load_session` 仍只返回 messages；`load_session_snapshot` shape 仍只有 messages/runtime/terminals。
- fork 继续复用既有 physical prefix copy、legal cut、line remeta 和 lineage 规则。

### 3. 接入 sampling 与 compaction

修改：

- `rust/crates/provider/src/lib.rs`
- `rust/crates/core/src/agent.rs`
- `rust/crates/core/src/agent/tests.rs`
- `rust/crates/core/src/compact.rs`

工作：

- 给 Mock 增加可脚本化 canonical usage 的 response variant；旧 variants 继续默认 `usage: None`，不批量改 fixtures。
- ordinary sampling 在 core validation 后按上述 outcome/model 契约记录；保持 `History::note_usage(usage.total())` 的 context-anchor职责。
- compaction 从 terminal 带回 usage，只在 accepted summary 上记录一次。
- 不修改三条真实 provider adapter；继续复用其已有 wire→canonical contract tests。

### 4. 扩展 `/cost` 并同步文档

修改：

- `rust/crates/core/src/commands/cost.rs`
- `rust/crates/core/src/commands/mod.rs`
- `rust/README.md`
- `docs/plan/HANDOFF.md`
- `docs/capability-report.md`
- `refs/README.md`
- 本 Plan 文件

工作：

- 固定 available/unavailable、全零、混合 model、多 response、resume/fork/compaction/clear 的输出语义。
- README 说明 provider-reported 四分类、当前-transcript 范围以及非账单边界。
- HANDOFF 记录：context anchor 是可失效的预测状态，provider ledger 是 append-only 历史事实；两者不能共享 reset 或 total 语义。
- capability report 只销账累计 provider-reported token 分类；金额、全局归因与 exact billing 继续未实现，Plan 67 状态不变。
- refs README 记录 Pi 调研基线和取舍：仅重实现 per-response ledger 机制，不复制代码、runtime 或弱化安全边界。

## 必须复用的现有 seam

- `kloop_protocol::Usage`；`Usage::total()` 仅继续供 context anchor 使用。
- `StreamEvent::Terminal { outcome, usage }`。
- `SampleOk { usage }`、`sample_with_retry`、`validate_assistant_result`。
- `History::persist`、`History::note_usage`、`History::replace_all`。
- `Rollout::append_line`、`intact_lines`、`parse_session`、现有 torn-tail repair。
- `fork_session` 的 prefix copy/remeta/legal-cut 逻辑。
- `run_compaction`。
- `commands::run` 与 `BUILTINS`。
- Anthropic、OpenAI Chat、OpenAI Responses 的已有 usage parser 与 provider contract tests。
- 现有 `Event::Usage` 及 TUI/server/headless context projection。

不得另建 usage parser、第二次 rollout 扫描、`.kloop/usage.*` sidecar、全局 ledger registry，或持久化 raw provider response。

## 非目标

- Provider/model 价格表、金额、货币、税费、折扣、云账单对账。
- Token budget、quota、admission、rate-limit policy 或自动停止。
- 对没有 terminal usage 的失败、超时、取消 attempt 猜测收费。
- Request/response id、global dedupe、exactly-once billing 或 exactly-once external execution。
- Project/user/global cross-transcript 总账，或 parent 汇总 child Agent/Program/Workflow。
- 按 model/operation 分组 UI、导出或 analytics dashboard。
- Public native usage event、snapshot/replay schema或 Desktop 展示。
- Durable Task graph/execution checkpoint、approval/process/MCP/side-effect replay。
- Permission、workspace、sandbox、dispatch、MCP 或 native JSON-RPC 架构调整。
- 修改 chars/4 context estimate、compaction threshold 或 `Usage::total()` 语义。
- 重做真实 provider usage parser。
- 改变 Plan 67/PDF 的暂停状态或总体路线优先级。

如果实施发现必须引入金额、失败-attempt telemetry、全局归因、public protocol、message+usage 原子事务、usage reset 命令或按 model 分组，停止扩 scope并另立计划。

## 关键文件

- `docs/plan/81-durable-provider-usage-ledger.md`
- `rust/crates/protocol/src/lib.rs`
- `rust/crates/core/src/usage.rs`（新增）
- `rust/crates/core/src/rollout.rs`
- `rust/crates/core/src/history.rs`
- `rust/crates/core/src/agent.rs`
- `rust/crates/core/src/compact.rs`
- `rust/crates/core/src/commands/cost.rs`
- `rust/crates/provider/src/lib.rs`
- resume signature 的机械调用点：`rust/crates/{cli/src/args.rs,tui/src/lib.rs,server/src/lib.rs}`
- `rust/README.md`、`docs/plan/HANDOFF.md`、`docs/capability-report.md`、`refs/README.md`

预计不修改三个真实 provider adapter、`core/src/event.rs`、`server/src/events.rs`、`server/src/wire.rs`、Desktop 仓库、Cargo manifests 或 `Cargo.lock`。若最终 diff 触及这些文件，完成记录必须逐项解释，且不得突破本计划的 public/security/无新增依赖边界。

## 验证

### 类型与 aggregate

- `Usage` 四字段 serde round-trip。
- 多 record 分别累计四个分类与 response count。
- empty ledger 与一条 all-zero reported response严格区分。
- 累计 overflow fail closed，不 wrap/saturate。

### Sampling 生命周期

- `EndTurn`、`ToolUse` 多轮、`OutputLimit` continuation、`Refused`、`Filtered`、`Incomplete`：validated terminal + `Some(Usage)` 各写一条 sampling record。
- `usage: None`、cancel、overflow、transport/protocol/partial failure、invalid assistant result不写。
- retry failures 后成功只写 terminal response；fallback 写实际 fallback model。
- record 与 assistant message 顺序用整段 rollout JSON断言。

### Compaction

- accepted non-empty `EndTurn` summary + usage：先写 compaction record，再写 compacted marker。
- `usage: None` 可成功但不增加；空 summary、非成功 terminal、cancel/failure 不改 history或 ledger。
- predictive/reactive/manual 三条入口共享同一 seam。
- compaction 清 context anchor但保留之前和本次累计。

### Rollout、resume 与 fork

- exact `provider_usage` JSON、next seq和parent chain。
- 完整 usage + torn next line保留 usage；torn usage line忽略并在 writable resume截断。
- `load_session` 只返回 messages；`SessionSnapshot` 与 provider history不出现 ledger。
- `Compacted` 和 `/clear` 不清 ledger；unknown additive fields继续可读。
- fork cut 前继承、cut 后排除；source/fork后续独立；fork-of-fork及 compaction 两侧正确。
- usage line不被 `opens_user_turn` 当成 user-turn boundary，也不改变 lineage/tool-pair repair。

### `/cost` 与 public boundary

- context window on/off 保留既有输出前缀；覆盖 unavailable、reported-zero、多 response、混合 model、resume、fork、compaction、`/clear`。
- 输出不出现 `$`、currency、price、quota、budget、coverage或伪 total。
- `Event::Usage`、`thread/tokenUsage/updated`、Plan 77 snapshot、`thread/read`、headless JSON shape不变；Desktop无 companion commit。

### 总质量门

从 `rust/` 运行：

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked -p kloop-protocol
cargo test --locked -p kloop-provider
cargo test --locked -p kloop-core
cargo test --locked --workspace
cargo run --locked -p kloop -- --mock
python3 -B ../refs/claude-code-2.1.220/verify.py --corpus-only
git diff --check
```

Plan 81 不修改真实 provider wire parser，因此不要求真实 API key：已有 provider contract tests证明 wire→canonical usage，scripted Mock证明 core persistence lifecycle。若实施必须触碰任一 adapter，先增加对应 contract test，并在完成记录解释原 seam 为什么不足；不能把 Mock 结果称为真实账单验收。

## 完成记录要求

实施完成后：

- Plan 状态改为 `✅`，补日期与提交 SHA（可写“SHA 以本文件所在提交为准”）。
- 代码、测试、README、HANDOFF、capability report、refs README 与本 Plan 完成记录同一个范围明确的 kloop commit；不 push。
- 记录最终 rollout JSON、included/excluded outcome、compaction/clear/resume/fork 语义、`/cost` available/unavailable 示例、focused tests及 workspace gates结果。
- 明确真实 provider、远端 CI、Windows、物理终端和 Desktop 未执行的证据面；不得用 Mock 冒充这些环境。
- 确认无新增依赖、`Cargo.lock` 不变、public server event与 Desktop 不变；任何偏差逐项记录理由。

## 完成记录（2026-08-12）

- 新增 `core/src/usage.rs`：`UsageOperation`、`ProviderUsageRecord`、`UsageLedger` 与 checked aggregate；protocol 的 canonical `Usage` 增加 serde round-trip。empty ledger 与 reported all-zero 分离，overflow fail closed。
- rollout 新增扁平 `provider_usage` line。代表性 wire：

  ```json
  {"type":"provider_usage","id":"session#2","parent":"session#1","ts":<unix-ms>,"model":"actual-model","operation":"sampling","usage":{"input_tokens":120,"output_tokens":2,"cache_read_input_tokens":3,"cache_creation_input_tokens":4}}
  ```

  `parse_session` 单次扫描同时恢复 messages/runtime/terminals/ledger；`resume_session` 改返 `ResumedSession`，CLI/TUI/server 全部显式传 ledger。usage 后下一行 torn 仍保留 usage；usage 自身 torn 被共同 intact-tail 规则忽略并在 writable resume 截断。
- sampling 在 `validate_assistant_result` 通过后、assistant message 之前写 record。EndTurn、ToolUse、OutputLimit、Refused、Filtered、Incomplete 的 terminal `Some(Usage)` 均记；`None`、cancel、overflow、transport/protocol/partial failure 与 invalid result 不记。retry/fallback 只记最终有效 terminal response，model 为实际 active model。
- compaction 只在 non-empty EndTurn summary 被接受时记 `compaction`，且顺序为 usage→compacted。`usage: None` 仍可成功；空 summary、非成功 terminal 与 provider failure 不改 ledger/history。predictive/reactive/manual 三入口复用同一 seam。
- resume 恢复累计；fork 只继承 cut 前 raw usage，fork-of-fork 延续其单文件 baseline；source/fork 后续独立。compaction 与 `/clear` 保留当前 transcript ledger但清/reset provider messages/context anchor。parent 不汇总 child rollout。
- `/cost` 示例：empty 为 `provider-reported usage across all models: unavailable`；available 显示 input/output/cache-read/cache-creation 和 `N reported responses`，model 行仍只指当前配置 model。输出不含金额、currency、price、quota、budget、coverage 或四分类伪 total。
- focused 验证覆盖 protocol serde、aggregate、exact JSON/chain、torn tail、resume/fork/fork-of-fork/compaction/clear、所有 included/excluded sampling outcome、fallback actual model、compaction acceptance 与 `/cost` 文案；最终 workspace gates 结果见本提交说明。
- 证据边界：未调用真实 provider，未跑远端 CI、Windows 或物理终端，未修改 Desktop。三个真实 provider adapter 未修改；依赖与 `Cargo.lock` 不变；`Event::Usage`、native `thread/tokenUsage/updated`、snapshot/read/sync/headless public shapes 不变。
