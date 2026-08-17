# Plan 92 — CodeWhale 借鉴：会话内 provider/model 热切换

> 状态：✅ 已完成（2026-08-18；提交 `02650ee`）
>
> 依赖：Plan 39、Plan 40、Plan 77、Plan 81、Plan 85、Plan 86、Plan 89；完成后纳入 Plan 91 跨面验收；参考快照 `refs/codewhale` 当前观察 HEAD `5e3ac84c5cb925b4c90c34dfe582b75f04605cb1`，固定历史审计基线 `b494236312ef3ac36489c83706a0b11ab73935a1`
>
> 开发期决策：kloop 尚未发布稳定兼容契约。本计划直接替换单 provider/model runtime、model-only public projection 和旧 rollout 形状；不做旧 session/config/wire 迁移、serde 默认补洞、双写或兼容别名。旧 rollout 缺少新 route timeline 时明确拒绝恢复，不从旧 `model`、message provenance 或环境变量猜 route。

## Context

kloop 当前在 thread/session 构建时冻结单个 provider 与 model；`fallback_model` 只是在同一 provider 内换模型，CLI/TUI/server 没有同一会话的 provider control transition。Plan 89 又有意要求：reasoning provenance 与当前 provider/API family/exact wire model 不匹配时，在网络前 fail closed，不能靠剥离 opaque state 后偷偷重试。因此，单纯把 `Config.provider` 或 `Config.model` 改成可写，会同时破坏 in-flight request、fallback、resume/fork、compaction 和 reasoning continuity。

CodeWhale 已提供目标 UX：`/provider` 打开 picker，`/provider <name>` 切 provider，`/provider <name> <model>` 同时切 model，并记住每个 provider 最近选择的 model（`refs/codewhale/crates/tui/src/commands/groups/core/provider.rs:39-113`、`refs/codewhale/crates/tui/src/tui/ui/provider_routes.rs:393-580`、`refs/codewhale/crates/tui/src/tui/app.rs:1197-1204`）；Responses 只在 provider/API/model 完全匹配时回放 encrypted reasoning，不匹配时丢弃该 reasoning 再继续（`refs/codewhale/crates/tui/src/client/responses.rs:440-465,752-779`）。本计划吸收这个显式热切换能力，但不照搬其不完整的 Anthropic provenance、Chat model heuristic，或 direct `/provider` path 未与 model picker 一样执行 `is_loading` gate 就 shutdown/re-spawn engine 的边界不一致（`refs/codewhale/crates/tui/src/tui/ui/apply.rs:372-425,1159-1184`、`refs/codewhale/crates/tui/src/tui/ui/provider_routes.rs:519-538`）。

核心区分是：

- **意外 mismatch**：继续执行 Plan 89，fail closed，不发请求；
- **用户显式 switch**：在 idle boundary 持久化 typed route transition，后续新 turn 才可构造有据可查的 lossy provider request view；
- **retry/fallback/continuation**：不是用户 switch，永远不能借该 lossy view 绕过 Plan 89；
- **canonical history**：不因切换改写；不兼容 reasoning 只从目标 provider 的 request view 排除，切回兼容 route 后仍可恢复 exact replay。

## 已拍板契约

### 1. Provider catalog 与 route ownership

1. 启动配置解析为一个 immutable `ProviderCatalog` 和一个 initial route，不再只构造单个 process-wide `Provider`。每个逻辑 provider profile 直接采用新 canonical schema：稳定 provider id、typed API family、不含凭据的 endpoint fingerprint、必填 `default_model`、有序去重的 `models` allowlist、可选且必须属于同一 allowlist 的 fallback model，以及 provider-local request settings；本计划不增加 model-specific settings overlay。credential 只在 provider 实例化时解析，绝不进入 descriptor、rollout 或 event。`KLOOP_PROVIDER`/`KLOOP_MODEL` 及 rail-specific model env 只覆盖新 session 的 initial route，且必须通过同一 catalog 校验，不能向 catalog 注入任意裸 model。
2. 每个 thread/session 独占 `SessionProviderState`：active route、单调 route revision、每个 provider 最近成功选择的 model。不同 server thread 互不影响；process profile/env 只决定新 thread 的 initial route。
3. agent turn、manual compaction、foreground child 与 background Agent/Program/Workflow admission 都只消费 immutable `FrozenProviderRoute`。sampling、compaction、fallback 和该 operation 创建的 child 在整个 operation 内使用同一 snapshot；运行中的 background child 不随 parent 后续 switch 改 route，新 child 才继承新 snapshot。
4. mutable route 归 thread/session supervisor，不塞进可广泛 clone 的 `Config` 共享单元。`Config` 中现有 provider/model/fallback 单真值直接拆除或改成 frozen operation input，避免 child clone、hook、tool worker 意外取得切换权。
5. `AgentType.model`、fork skill 的 model、`run_agent`/Workflow Agent 的 model override 不再是脱离 provider 的裸字符串：要么使用完整 route ref，要么明确表示“在 inherited frozen provider 上覆盖 model”，并在 child admission 时一次 resolve/freeze；任何 override 都不能回写 parent session route。

建议 canonical 类型边界（具体命名开工时可按现有模块调整）：

- `ProviderCatalog`：配置真值与 lazy provider factory/cache；
- `ProviderRoute`：provider id、API family、endpoint fingerprint、primary/fallback model 及 provider-local request settings；
- `SessionProviderState`：revision、active route、per-provider remembered model；
- `FrozenProviderRoute`：一次 operation 的不可变 route/client/config receipt；
- `FrozenProviderAttempt`：从 frozen route 派生的 actual primary/fallback attempt，固定 actual model 与 attempt kind；
- `ProviderRouteChanged`：append-only transition，不是 prompt message、turn terminal 或 usage record。

### 2. Switch transaction 与并发边界

1. provider/model switch 只允许 depth-0 用户控制面发起，不新增 model-visible tool；模型、hook、child、fallback 与 provider response 都不能自行提交 route transition。
2. 只在 thread **没有 active operation** 时切换。active turn、manual compaction、正在 settlement 的 cancellation/terminal、foreground control command 均返回 typed busy；不实现 interrupt-and-switch，也不修改已经冻结的 request。
3. switch 与 `turn/start`、TUI autowake、scheduler wakeup、manual compact 共用 thread single-flight/CAS seam，结果必须二选一：旧 route 已冻结并完整跑完，或新 route 先提交且下一 operation 全量使用它；不存在一半 rounds 用旧 provider、一半用新 provider。
4. transaction 顺序固定：在 gate 外解析/校验 target并构造可用 provider client → 取得 idle/single-flight gate并复核 expected route revision → 在同一 History-owner 临界区以 hypothetical next revision 和 target **primary attempt** 生成 request-view plan及 typed continuity receipt（`Preserved` 或 `Filtered`）→ 通过 strict/fallible route append写 durable transition → 原子发布 memory state → 发 public event。坏/未知 provenance 使 preflight fail closed；合法但不兼容的旧 reasoning 产生 `Filtered`，不得静默。任何前置或持久化失败保持旧 state、旧 remembered model、零 event、零 provider I/O；route append失败不能沿用普通 `History::persist` 的“detach rollout 后继续内存会话”降级。
5. 相同 provider identity + 相同 model 是无副作用 NoOp；同 provider 换 model、同 family 换 endpoint/provider id、跨 family 均是新 revision。bare `/provider <id>` 使用该 session 对该 provider 的 last model，没有时使用配置 default；只有成功 commit 才更新 remembered model。
6. provider switch 本身不发模型请求、不创建 turn、不写 user/assistant message、不触发 continuation、不制造 usage。切换后必须由新的 user turn/明确 operation 才产生 provider I/O。
7. session switch 永不修改 `~/.kloop/config.toml`、环境变量或其他 thread 的 default；若未来需要“设为启动默认 provider”，必须是独立显式配置操作，不复用本 transaction。

### 3. Durable route timeline

1. 每个持久化 rollout 在首条可采样 message 前通过 strict/fallible append 写 initial route receipt；失败则 session 创建失败，不进入 best-effort memory-only 模式。每次成功 switch 追加 `provider_route_changed`，至少绑定 route revision、rollout sequence/message boundary、逻辑 provider id、API family、不含凭据的 endpoint fingerprint、primary/fallback model 与 bounded reasoning continuity（preserved/filtered，不含 reasoning 内容）。
2. provider-produced assistant message 的 private provenance 在 Plan 89 identity 上增加 producing route revision 与不可变 origin rollout boundary，并继续记录 actual final wire model/attempt kind。partial/cancel fragment与 normal response 都由 History owner 在 append 时绑定该 origin；不能让调用方事后填旧 revision。compaction provider 输出仍按 Plan 81 记 actual attempt usage，但 canonical replacement summary 是 synthetic user context，不伪装成 provider assistant provenance。
3. restore 只接受完整的新 route timeline。route revision 缺失、重复、倒退、未知 source、message provenance 引用不存在/future revision、origin boundary 不落在该 revision 的有效区间、route/attempt identity 不一致时 fail closed；在较新 revision 已提交后追加却伪称旧 revision 的 message 必须拒绝，不能因为“旧 revision 可 strip”而洗白。不读取旧 `SessionRuntime.model` 猜 route，不为旧 rollout 合成 initial transition。
4. resume 恢复最后 committed route 和 remembered models；fork/原地 rewind 按所选 canonical rollout boundary 恢复当时 route，而不是 process 当前默认或源 thread 最新 route。route transition 必须参与 cut/recovery 顺序，不能只按 message count 猜前后。
5. `compacted`/`repaired` 记录必须保留或可验证 retained message 的 origin mapping 与 route interval；folded synthetic summary 不获得 assistant provenance。compaction 不改变 active route，也不删除 kept-tail 所需 evidence；`/clear` 清 provider context/history 后旧 message 不再参与 request-view provenance，但保留当前 route 和 per-provider model memory。普通 recovery 遇 torn transition 复用 Plan 85 intact-tail 规则，不能发布半次 switch。
6. 不强制 switch 前 compaction，也不创建 provider-neutral summary/checkpoint。manual/predictive/reactive compaction 始终使用其 operation 开始时的 frozen route 和同一 request projection。

### 4. Explicit-switch reasoning projection

新增唯一 internal `provider_request_view(history, frozen_attempt)` seam；三条 adapter 不各自猜来源，也不能复用 display-safe `into_public_projection`。switch preflight只对 hypothetical target primary attempt 规划；实际 sampling 的 primary 与每次 fallback attempt 都重新从同一 frozen route 派生 attempt并生成 view，再进入 `Provider::stream` final guard。attempt kind 同时是 projection authority：只有用户显式选中的 current primary 可消费该 route transition 的 lossy authorization；automatic fallback 只能 exact replay（Chat no-replay boundary 除外），不能因为与 primary 共用 revision 就额外 strip。

对每个含 reasoning 的 provider assistant message依次执行：

1. **验证 source**：producing route revision 必须存在，origin boundary 必须落在该 revision 的有效区间；message 的 provider/API/actual model/attempt kind 必须符合该 revision 冻结的 primary 或合法 fallback attempt。证据缺失或自相矛盾直接 fail closed，显式 switch 也不能洗白坏 provenance。
2. **exact compatible**：source provider endpoint fingerprint、API family、actual wire model 与本次 target attempt 完全一致时，Thinking text+signature、redacted blob、Responses encrypted content 原样保留；允许 A→B→A 后重新使用 A 的合法 opaque state。
3. **sanctioned incompatible**：仅当本次是用户显式选择的 current primary attempt，source revision 严格早于当前 active revision，且中间存在成功的显式 route transition 时，才从 request view 删除完整 reasoning block；readable thinking 与 signature/encrypted/redacted payload作为一个语义单元一起删除，绝不降级成普通 assistant text。
4. **unsanctioned mismatch**：同一 active revision 内的 model/provider mismatch、automatic fallback 想借 primary 的显式 transition跨 exact-model reasoning、config drift、伪造 current provider 或无 transition 的 mismatch继续使用 Plan 89 non-retryable protocol failure，且在网络前停止。
5. **Chat boundary**：Chat target 始终不 replay reasoning，但仍须先通过 active route/source-revision 验证；不按 model 名、URL 或“当前是 Chat”推断未知 history 可安全接受。

过滤后按 provider-neutral history 规则移除真正空的 assistant shell，但必须逐字保留 text/image、ToolUse、ToolResult、tool call id、ordering 和完整 pairing；nested blocks 同样递归检查。最终 view 仍进入 `Provider::stream` exact replay validator，形成“route authorization → lossy projection → adapter final guard”三层边界。adapter 内现有重复 reasoning strip/heuristic 随之删除，不能保留第二套策略。

### 5. 用户与 native protocol surface

1. CLI/plain/TUI 共用一个 typed switch command：
   - `/provider`：TUI 打开 provider→model picker；plain 输出有界 catalog、active route 和用法；
   - `/provider <provider>`：切到该 provider 的 remembered/default model；
   - `/provider <provider> <model>`：显式切 provider/model；
   - 现有 model-only selector若保留 UI 入口，也必须调用同一 transaction；不再直接写 `Config.model`。
2. native server 建立唯一 provider-aware surface：provider catalog/read、`thread/provider/switch` 和 session-scoped `thread/provider/changed`。switch request 带 thread id、target 与 expected route revision；response/changed event/snapshot 带新 revision、逻辑 provider id、model、API family 与 bounded continuity receipt，不带 endpoint、env key、credential、private provenance 或 route history。UI 在 idle 显示 selected route，在 operation 期间显示 frozen active route，terminal 后再回 selected route；late terminal/usage 始终从冻结 attempt 内部归因，不为“last served route”另建 public mutable state。
3. model-only `model/list`、thread runtime `model` 字段及相关 DTO 直接被 provider-aware descriptor/active route 替换，不双发旧字段、不保留 adapter。若 wire shape 发生不兼容变化，原位提升 native protocol version并同步所有 kloop 客户端/fixtures；不维护 v1/v2 双栈。
4. `thread/start` 可选择 initial provider/model；未指定时使用 resolved initial route。`thread/read`、initial/refreshed snapshot、resume/fork response 始终投影 active route；event sequence/generation 继续复用 Plan 77，不建第二套前端 reducer。
5. provider catalog 只枚举配置已声明的 provider/model。未选 provider 的 credential 缺失不阻断其他 provider 启动，但 descriptor 只给 bounded availability code；switch 在状态 mutation 前拒绝 unavailable target。不在本计划做网络 model discovery、健康探测或运行时 credential 编辑。

### 6. Usage、fallback 与 public safety

1. Plan 81 的 accepted-terminal `Some(Usage)` once-only、usage→message/compacted ordering、context anchor 与 parent/child ownership不变；dynamic switch 不清 ledger。
2. usage fact 必须能区分逻辑 provider、API family、route revision/purpose 和 actual final model。若当前 ledger 只有 model，本计划直接替换 schema并更新 `/cost` 为 provider+model 分组；不迁移旧 ledger，不计算价格、金额或跨 child billing。
3. 现有 same-provider model fallback 只能从 frozen route 派生明确的 fallback attempt，并为该 actual model重新生成 request view、再走 Plan 89 exact reasoning guard；它不更新 active route、不更新 remembered model、不发 changed event。自动 cross-provider fallback不在本计划内。
4. public history/snapshot 继续使用 Plan 89 display-safe projection：可以展示既有 reasoning summary text，但不能暴露 route timeline、endpoint fingerprint、signature、encrypted/redacted payload。target provider 的 request view 与 public display view是两个不同 owner，禁止互相复用。

## 关键文件

- `kloop/crates/protocol/src/lib.rs`
- `kloop/crates/provider/src/{lib.rs,anthropic.rs,openai.rs,responses.rs}`
- `kloop/crates/core/src/{config.rs,agent.rs,agent/sampling.rs,history.rs,rollout.rs,compact.rs,usage.rs,commands/}`
- `kloop/crates/cli/src/{provider_config.rs,startup.rs,main.rs,args.rs}` 的 registry、initial route、standalone resume 与 server factory
- `kloop/crates/core/src/{agent_type.rs,skills.rs,tools/subagent.rs}` 的 child model/route override
- `kloop/crates/tui/src/` 的 slash routing、picker、status/header 与 idle worker
- `kloop/crates/server/src/{lib.rs,events.rs,wire.rs}`
- CLI/TUI/server/provider/core 的 route lifecycle、request capture、resume/fork/rewind/compaction tests
- `kloop/README.md`、`docs/plan/HANDOFF.md`、Plan 91 acceptance matrix

## 非目标

- 不做 mid-turn switch、interrupt-and-switch、已发请求改路由或 provider response 驱动切换。
- 不做自动 cross-provider fallback、负载均衡、健康路由、quota/budget 路由或 provider retry broker。
- 不新增 provider rail，不重写三条 wire parser，不把 Messages/Responses opaque reasoning跨 provider转换。
- 不把 reasoning summary 当普通 assistant text，不因 switch 改写 canonical history，不强制生成 handoff summary。
- 不从 provider 在线拉 model catalog，不在 UI 编辑 endpoint/key，不持久化或公开 credential/raw endpoint。
- 不给 model/child/hook 暴露 switch tool，不把 route transition伪装成 turn、message、terminal或 usage。
- 不兼容 pre-Plan-92 rollout/config/native wire，不做 migration、dual-read、dual-write、deprecated alias或旧 fixture 保留。
- 不在本计划完成 Desktop 产品 UI、价格/金额、child billing、Linux/Windows/物理终端 E2E。

## 实施顺序

1. **Catalog 与 frozen attempt**：直接替换 provider config schema，把 resolved settings 从单 Provider改成 immutable catalog + initial route；建立 session router、frozen route/attempt input，删除 core `Config` 的 live provider/model mutation可能性与 arbitrary model string 旁路。
2. **Durable transition**：增加 strict initial/changed route append、route revision、origin boundary 与 assistant provenance binding；接通 compacted/repaired mapping、restore、recovery、resume、fork、rewind、clear、compaction。
3. **Request view**：集中 source/interval validation、explicit-switch strip、per-attempt exact replay 和 Chat boundary；让 primary/fallback 三 adapter 只消费已授权 view并保留 final guard。
4. **Control surfaces**：接 `/provider`、picker/plain、initial flags、native catalog/switch/event/snapshot；所有入口共用同一 transaction和 route revision CAS。
5. **Usage 与收口**：provider-aware usage、fallback negative tests、public redaction、README/HANDOFF/Plan 91 更新；删除旧 model-only state/wire/dead helpers。

## 测试矩阵

### Route/lifecycle

- 三条 rail各至少两个 configured provider profiles；default/allowed/fallback model 的排序去重与校验；provider/model env只选 initial route，不能注入 catalog外 model。
- Anthropic→Responses、Responses→Anthropic、任意 rail→Chat、Chat→Messages/Responses；同 provider A-model→B-model→A-model；同 family/model但不同 endpoint/provider id。
- `/provider <id>` 的 remembered/default model、显式 model override、NoOp、unknown provider/model、missing credential、provider factory failure、persist failure。
- running turn、manual compact、terminal settlement 与 switch并发；turn/start/autowake/scheduler 与 switch CAS；断言旧或新 route整次冻结、失败零 mutation/event/I/O。
- foreground/background child 在 switch 前后 admission；已运行 child保持旧 route，新 child使用新 route；不同 server threads互不影响。

### Reasoning/history

- Anthropic signed/redacted thinking 和 Responses encrypted+summary 在 exact route逐字节 round-trip；A→B 时完整移除，B request无 opaque/readable残片；B→A 后恢复原始 A blocks。
- current revision mismatch、missing/future revision、source route/provenance 不一致、origin boundary 越过 route interval、较新 revision 后伪称旧 revision的迟到 message、自动 fallback跨 model reasoning 全部在 HTTP 前失败；只有 source interval 合法且被后续 durable user switch 跨过的 revision 才能 strip。
- primary 与 fallback 分别生成 `FrozenProviderAttempt` request view；primary 失败不能复用其已投影 view 给另一 model，fallback不能消费 primary 的 lossy authorization，也不制造 route transition/remembered-model/event。
- Chat 不 replay reasoning且不接受未知 provenance；不使用 model-name heuristic。
- mixed text/reasoning/tool blocks、reasoning-only assistant、nested ToolResult blocks、complete/oversized tool pairs；过滤后 provider request ordering与 ToolUse/ToolResult id不变。
- EndTurn/ToolUse/OutputLimit/Refused/Filtered/Incomplete、semantic partial、cancel后显式 switch；switch不重试旧 turn、不生成第二份回答。

### Persistence/compaction/public

- strict initial/changed route append失败不 detach rollout、不切 memory state；initial route、multiple transitions、torn/duplicate/regressed revision、resume；fork/rewind 在 switch 前后 cut；`/clear` 保留 route；compaction before/after switch、NoOp、kept tail origin mapping、synthetic summary无 assistant provenance与 compaction actual-attempt usage。
- pre-Plan-92/model-only rollout明确拒绝，不迁移、不推断；new rollout fixed JSON bytes与typed equality。
- usage按 provider/API/actual model/purpose记一次，fallback不改 active route，switch零 usage；`/cost` 不合并同名跨 provider model。
- TUI picker/plain command/native switch/read/resume/fork snapshot和 changed event；route revision/sequence 单调，public JSON 不含 endpoint、credential、private provenance、signature、encrypted/redacted content。
- fake-provider request capture覆盖所有负契约；真实验收至少在同一 session 完成 Anthropic↔Responses（可用配置允许时再覆盖 Chat）并验证切回后继续，不打印 key、endpoint、raw opaque transcript。

## 完成标准

- 同一 session 可在 idle boundary 显式切 provider/model，下一 operation 全量使用新 route；in-flight operation、child 和并发 turn不存在混用。
- canonical history、tool pairing、typed terminal、compaction、resume/fork/rewind 与 Plan 81 usage ledger在多次切换后保持单真值。
- exact-compatible reasoning可恢复回放；只有有 durable explicit transition 的旧 revision可做完整 reasoning strip；accidental mismatch、fallback和坏 provenance继续 fail closed。
- CLI/plain/TUI/native server 共用单一 catalog、switch transaction、route revision与public projection；无 model-only second truth、无 secret/raw reasoning泄漏。
- 旧 provider/model runtime、旧 rollout/wire reader和兼容分支已删除；无新增 provider、依赖、自动跨 provider routing或 Desktop UI。
- focused tests、`cargo fmt --all -- --check`、workspace Clippy `-D warnings`、workspace all-target/all-feature tests、mock interactive/headless/NDJSON、diff check全绿；真实 provider、未执行平台和 UI 环境逐项如实记录。

## 实现验收记录（2026-08-18）

已落地 immutable provider catalog、session route revision、frozen route/attempt、durable route receipts、origin-bound provenance、唯一 request-view seam、provider-aware usage、CLI/plain/TUI/native provider control surface 与 protocol 2.0。旧无 route timeline rollout 在 inspect/resume/fork 时 fail closed，不从 model/runtime/environment 猜路由。focused provider/protocol/core/server/CLI/TUI tests 与 mock smoke 已执行；真实 provider、Linux/Windows/Desktop/物理终端未执行。
