# Plan 85 — CodeWhale 借鉴：session snapshot/recovery seam

> 状态：✅ 已完成（2026-08-14；当前工作树未提交）
>
> 验证：`cargo test --locked -p kloop-core rollout`（40 passed）；`cargo test --locked -p kloop-core history`（16 passed，含相关 compact/agent/clear tests）；`cargo test --locked -p kloop-server --test server`（32 passed）；`cargo test --locked -p kloop --test headless_contract`（3 passed）；`cargo fmt --all -- --check`、workspace clippy `-D warnings`、`cargo test --locked --workspace`、text/JSON/plain mock smoke 与 `git diff --check` 全绿。Workspace 有 2 项真实 provider credential tests ignored。
>
> 基线：`3cca84a`（Plan 84）
>
> 依赖：Plan 7、Plan 68、Plan 77、Plan 81；参考快照 `refs/codewhale` 当前观察 HEAD `5e3ac84c5cb925b4c90c34dfe582b75f04605cb1`，固定历史审计基线 `b494236312ef3ac36489c83706a0b11ab73935a1`

## Context

近期只读审计的 CodeWhale 当前参考库为 `refs/codewhale` HEAD `5e3ac84c5cb925b4c90c34dfe582b75f04605cb1`；旧 Plan 60 的 `b494236` 只是历史基线。最适合 kloop 直接吸收的机制是提交 `c0e69f4ab`（2026-08-12）确立的 session snapshot 只读语义与显式 crash recovery 语义分离：普通检查不能把仍在运行的 tool call 误判为崩溃，明确 resume 才能 repair，并返回可审计统计。

kloop 已有 Plan 77 的 generation/sequence/cursor/snapshot 分层和 Plan 81 的 durable provider usage ledger，但 `rust/crates/core/src/rollout.rs` 当前仍让 `load_session_snapshot` 和 `resume_session` 共同调用 `repair_pairing`；`resume_session` 同时负责 torn-tail 物理截断。需要把“只读解析、内存配对归一化、显式恢复修复、物理截断”边界写成可验证契约，避免 server 的 list/read/spawn seed 路径产生恢复副作用。

本计划只落这一条持久化边界，不把近期 CodeWhale 的 route receipt、opaque reasoning、usage stream receipt、deferred activation cache 或具体 provider 接入混入同一个机制切片。

## 契约与范围

### 本次吸收

1. `load_session_snapshot` 是纯只读入口：读取现有 rollout 的可重放内容和 snapshot-only runtime/terminal metadata；可以复用纯内存的 pairing normalization 让 list/read/provider projection 得到可消费 history，但不截断、不 append、不写回、不生成 repair marker，也不把 dangling `tool_use` 对外报告为 crashed repair。
2. 新增 core 内显式 recovery seam（名称可按现有风格确定，例如 `recover_session_for_resume`），只由 resume/restart/fork 的恢复主路径调用；它在一次解析中识别 intact boundary、按既有规则物理截断 torn tail、执行 pairing repair，并返回结构化 `SessionRecovery`/`ResumedSession` 与 `SessionRepairStats`。修复后的完整 messages 以 additive rollout `repaired` canonicalization marker 持久化；不复用 `compacted` marker，因为后者当前会改变 runtime/terminal snapshot 语义。该 marker 不是 public protocol，也不是第二真值：`parse_session` 读取它时只替换 effective messages，继续保留同一文件内的 runtime、terminals、provider_usage、lineage 和 sequence。
3. repair stats 至少区分 `repaired_tool_uses`/missing uses、`duplicate_results`、`orphan_results`/stray results、`repaired_messages`/dropped messages；同时报告 torn-tail 截断字节数（必要时行数），不能把物理修复隐藏在普通成功结果里。统计按顺序配对计算：正常同一 assistant turn 的多个不同 tool-use 不算 duplicate；同一紧邻 pair 对同一 id 的多余 result 才算 duplicate；前一条 assistant 没有对应 use 的 result block 才算 orphan。重复 tool-use id 不静默猜配对，按 malformed/fail-closed 处理或列为本计划非目标。
4. repair 持久化后必须幂等：第二次显式 recovery 不新增 repair、重复 ledger、重复 runtime/terminal metadata 或重复 history patch，统计为零。
5. 保留合法 legacy rollout、unknown additive line fields、既有 compacted marker、fork lineage、provider usage ledger 和当前 malformed-middle-line/last-intact-tail 规则；只改变 owner/seam 语义，不改变 provider message wire。

### 所有权与生命周期

- `parse_session` 继续是 rollout 的单次解析真值，不能新增 usage-only 或 repair-only 第二次文件扫描。
- snapshot/list/read/spawn seed 只读，供 Plan 77 public projection seed 使用；它们不恢复 running tool、approval/question、process/kernel、scheduler delivery、mailbox side effect 或 live execution state。
- `thread/resume`、CLI/TUI resume 和明确 fork recovery 才可调用显式 recovery；resume 仍创建新的 public generation，volatile state 仍 reset。
- `provider_usage` 只从同一次 replay 恢复；recovery 不重新累计 ledger，也不把 runtime/terminal/usage 混入 provider history messages。
- 共享/并发 recovery 继续受现有 active-thread/History owner 约束；本计划不承诺跨进程 exactly-once 或 crash-safe side-effect replay。

### 非目标

- 不复制 CodeWhale 的 `SessionManager`、Fleet、role/profile/worktree、app-server/ACP wire 或第二套 session registry。
- 不新增 sidecar、durable public-event journal、snapshot 第二真值、new protocol version、Desktop DTO 或 public `thread/read`/`thread/events/sync` 字段。
- 不实施 child route provenance、opaque reasoning origin、逐 model-call stream usage receipt、deferred tool activation cache、MCP startup planning 或新 provider。
- 不改变 `History::replace_all`、compaction、fork legal-cut、provider adapter、permission/sandbox 和 Plan 77 projection 的既有产品边界。
- 不把任意 malformed middle line 静默重写成合法历史；仍沿既有解析失败/最后完整边界策略处理。

## 实施步骤

### 1. 审计并拆分 rollout seam

- 复核 `rust/crates/core/src/rollout.rs` 的 `ParsedSession`、`parse_session`、`load_session`、`load_session_snapshot`、`resume_session` 与 `repair_pairing`。
- 为 `RolloutLine` 增加仅供 rollout replay 的 additive `repaired` variant，并在 `parse_session`、fork copy/remeta、metadata/sequence exhaustive matches 中处理；该 variant 只承载 canonical replacement messages，不进入 `SessionSnapshot`、native JSON-RPC 或 provider request。
- `parse_session` 遇到 `repaired` marker 时替换 effective messages，但保留当前文件中的 runtime、terminals、provider_usage、lineage 和 sequence；一次 recovery 最多写一份 canonical marker，marker 本身完整写入后才算 repair 持久化成功。
- 对 legacy 文件没有 marker 的 pairing normalization 也可以在 snapshot/list/read 中纯内存执行，以保持现有展示和 provider seed 语义；但只丢弃内部统计，不 truncate、append 或补 repair marker。显式 recovery 才能把 normalization 持久化。

### 2. 建立显式 recovery API

- 在 `core/src/rollout.rs` 增加显式 recovery 返回类型，复用一次 `parse_session` 的 items、ledger、runtime、terminals、sequence 和 intact boundary。
- 将 torn-tail truncate 与 pairing repair 限定在 recovery；明确 recovery 是否已经把 repair canonical state 持久化，避免“只返回内存修复但下次又重复”的歧义。持久化必须沿现有 `Rollout` append/compaction/History owner，不新建 writer 或 sidecar。
- `repaired` marker 的 replacement 必须同步定义 `SnapshotTerminal.after_message` 的 old→new boundary 映射；不能因删除 stray/duplicate block 而让 terminal metadata 指向错误消息。若该 metadata 被明确声明为 raw display metadata，则仍需测试其 index 语义不被 provider history normalization 误用。
- 让重复 recovery 对已 canonical 的文件返回 no-op stats，并验证 provider usage、lineage 与 sequence 不重复。

### 3. 迁移调用方并锁定负契约

- `rust/crates/server/src/lib.rs` 的 thread resume/fork 使用显式 recovery；list/read/spawn seed 和 `thread/events/sync` 继续使用只读 snapshot。
- CLI、TUI、History resume 的机械调用点改用结构化恢复结果，保持现有 runtime/cwd migration、generation replacement 和 fork 语义；`--list-sessions` 与 picker 继续只读，是否展示 repair stats 不在本计划扩展为新的 CLI public 文案。
- 保持 `load_session` 的既有兼容行为或提供窄兼容委托，但新只读调用方不得继续走兼容 recovery 委托。

### 4. 补齐测试矩阵

- 纯 pairing stats：合法零、stray/orphan result、缺 result、partial pair、duplicate/reused id；断言整对象与统计。
- read-only：重复 snapshot 读取前后文件 bytes/hash 不变；torn tail 读取不截断；snapshot 不产生 repair receipt。
- recovery：显式 recovery 才 truncate/repair；recovery output canonical；保存后第二次 recovery stats 全零、ledger/runtime/terminal 不重复。
- 生命周期：resume across restart、fork cut、legacy session、compacted/clear、unknown additive fields、malformed middle line、usage line/torn usage line。
- server：list/read/spawn 不写盘；resume/fork 进入 recovery；public snapshot/history/wire shape 不变；现有 active-thread 并发 guard 不回归。

### 5. 同步文档与证据边界

- 更新 `rust/DESIGN.md` 的 session/recovery 说明、`docs/plan/HANDOFF.md` 顶部完成事实与教训、`docs/capability-report.md` 的对应路线销账，以及 `refs/README.md` 的 CodeWhale 当前 HEAD/借鉴范围。
- 保留 CodeWhale 固定 HEAD、提交 `c0e69f4ab` 和只读审计边界；不要把候选设计写成已实现能力。

## 必须复用的现有 seam

- `ParsedSession` / `parse_session` 的单次 rollout replay；
- `intact_lines`、`intact_end` 与现有 torn-tail truncate；
- `repair_pairing` 的双向 ToolUse/ToolResult 合法性规则；
- `ResumedSession`、`History::resume`/`rebase`、`Rollout::append_line`；
- `fork_session` 的 legal cut、prefix copy、lineage remeta；
- `SessionSnapshot` 的 runtime/terminals snapshot-only 字段；
- `provider_usage` replay 与 Plan 81 的 append-only ledger owner；
- server 既有 `thread_resume`、`thread_fork`、`spawn_thread` 调用链和 active-thread guard。

## 关键文件

- `docs/plan/85-session-snapshot-recovery-seam.md`（本计划）
- `rust/crates/core/src/rollout.rs`
- `rust/crates/core/src/history.rs`
- `rust/crates/core/src/agent.rs` 及其 resume/turn 测试
- `rust/crates/server/src/lib.rs`
- `rust/crates/server/tests/server.rs`
- `rust/crates/cli/src/args.rs`
- `rust/crates/tui/src/lib.rs`
- `rust/DESIGN.md`
- `docs/plan/HANDOFF.md`
- `docs/capability-report.md`
- `refs/README.md`

预计不修改：`rust/crates/provider/src/{anthropic,openai,responses,sse}.rs`、`rust/crates/server/src/{events,wire}.rs`、Desktop 仓库、Cargo manifests、`Cargo.lock`。如果调用方迁移确实触及其中任一文件，完成记录必须解释原因，且不得突破本计划的 public protocol 和无新增依赖边界。

## 验证

### 定向契约与生命周期

从 `<repo>/rust` 执行：

```bash
cargo test --locked -p kloop-core rollout
cargo test --locked -p kloop-core history
cargo test --locked -p kloop-server --test server
cargo test --locked -p kloop --test headless_contract
```

覆盖：snapshot no-write、显式 recovery、repair stats、torn-tail、重复 recovery、resume/fork/clear/compaction、legacy/unknown fields、server read/list/spawn/resume/fork。

### 总质量门

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace
cargo run --locked -p kloop -- --mock
cargo run --locked -p kloop -- --mock --headless
cargo run --locked -p kloop -- --mock --headless --json
cd .. && git diff --check
```

验证记录必须写明 Darwin/macOS 实际结果、focused passed 数量、workspace ignored 项，以及真实 provider、Linux sandbox/CI、Windows、物理终端、Desktop E2E 未执行的部分。Mock 只证明本地 lifecycle，不冒充真实 provider 或 crash-safe 外部 side-effect 恢复。

## 完成标准

- snapshot/read/list/spawn seed 无写入、无 repair 副作用；显式 resume/recovery 才 repair/truncate。
- repair stats 可审计，保存后第二次 recovery 幂等且 ledger/sequence/lineage 不重复。
- provider history、runtime/terminal snapshot、provider usage ledger、public protocol 和 Plan 77 generation 语义保持既有边界。
- 新增行为有纯 helper、rollout、server lifecycle 和负向测试；README、HANDOFF、capability report、refs README 同步。
- `cargo fmt`、clippy、workspace tests、mock smoke、git diff check 全绿；不新增依赖，不修改 Desktop，不 push。
- 实施完成后将本文件状态改为 `✅ 已完成`，补实际日期、commit SHA、focused/workspace 验证结果和未执行环境；代码、测试、文档一次范围明确的 kloop commit 收口。
