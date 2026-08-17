# Plan 91 — CodeWhale 借鉴：agent boundary acceptance/conformance

> 状态：✅ 已完成（2026-08-18；Plan 87–90 deterministic acceptance 与 Plan 92 provider route/switch 跨面验收已实施；提交 `da6f7f3`）
>
> 依赖：Plan 87、Plan 88、Plan 89、Plan 90、Plan 92；复用 Plan 54、Plan 63、Plan 68、Plan 77、Plan 81、Plan 85、Plan 86；参考快照 `refs/codewhale` 当前观察 HEAD `5e3ac84c5cb925b4c90c34dfe582b75f04605cb1`，固定历史审计基线 `b494236312ef3ac36489c83706a0b11ab73935a1`

## Context

Plan 87–90 与 Plan 92 分别收口 deferred capability binding、child route provenance、provider terminal/reasoning continuity、MCP readiness 和显式 session provider route transition。它们完成后仍需要一个只做跨面验收的计划，证明这些内部契约在 resume、fork、compaction、worktree 切换、child lifecycle、tool refresh、provider fallback 和 provider switch 后保持一致，并且 CLI、TUI、plain、headless、native server 不各自重新解释内部状态。

本计划不承载新的领域实现，只建立 deterministic conformance 和 public-boundary acceptance；不把未执行的真实 provider、Linux sandbox、Windows、Desktop 或物理终端写成通过。

## 契约与范围

### 本次吸收

1. 验证 discovery/unlock receipt 的 Workspace/worktree/policy/authority/source-generation binding，在 refresh、workspace switch、fork、resume、compaction、child clone 后 stale 时 fail closed。
2. 验证 Agent/Program/Workflow/mailbox/background execution 的 route/provenance identity 与 terminal owner 不混淆；late delivery、duplicate terminal、resume/fork lineage 和 root Task ownership 不回归。
3. 验证 provider typed terminal、incomplete/reasoning continuity、tool pairing、fallback/continuation、显式 provider route transition 和 Plan 81 usage record 边界一致；incomplete 不成为成功 history，只有 durable user switch 可生成 lossy request view，usage 不重复记账。
4. 验证 MCP configured/planned/starting/ready/degraded/failed/stale receipt、catalog generation、auth availability、resource read 和 tool call 的 failure semantics 一致；MCP failure 不伪装成 missing tool。
5. CLI/TUI/plain/headless/native server 只消费 bounded stable projection，不暴露 raw receipt、secret、opaque reasoning 或内部 registry；公共 protocol、snapshot、generation/sequence shape 不扩张。

## 关键文件类别

- core integration tests/evaluator：config/workspace/tools/agent/compact/history/rollout/event
- protocol typed projection and negative tests
- provider fixtures/stream/semantic lifecycle tests
- MCP transport/catalog/readiness fixtures
- server events/wire/read/list/resume/fork tests
- CLI/TUI/plain/headless contract tests
- `README.md`、`docs/capability-report.md`、`docs/plan/HANDOFF.md` 与各 Plan 完成记录

## 非目标

- 不新增 capability、route、provider、MCP registry 或 execution runtime。
- 不扩 public protocol 字段，不创建 public durable event journal、snapshot 第二真值或 Desktop DTO。
- 不汇总 Agent/Program/Workflow/MCP 子调用费用，不改变 Plan 81 ledger owner。
- 不把真实 provider、Linux sandbox/CI、Windows、Desktop E2E、物理 terminal 未执行冒充 deterministic local test。
- 不以自然语言自评、单次 mock smoke 或 aggregate count 替代 typed state、bytes、lineage、terminal 和 negative assertion。

## 为什么不能与 Plan 87–90、92 合并

Plan 91 必须在领域契约稳定后才能收口；如果提前合并，验收会反向固化未完成的内部类型，并掩盖 capability、provenance、provider terminal/switch 和 MCP 各自的 owner 边界。它只能验证和投影，不能偷偷承载领域实现。

## 实施与测试方向

建立跨面矩阵：每个维度至少覆盖 happy path、stale/deny/failure、duplicate/late、resume/fork/compaction、provider switch、child/worktree scope 和 public projection。用固定 JSON/rollout bytes、typed object equality、generation/sequence/lineage/route revision、terminal count、provider request capture 和 secret-negative mutation 断言；测试过滤器必须校验实际命中，避免 `0 passed` 被误判为通过。运行 focused core/server/provider/MCP/CLI tests、fmt、clippy、workspace test、mock/headless smoke 和 diff check，分别记录未执行环境。

## 完成标准

- Plan 87–90、92 的跨面负契约在 resume/fork/compaction/worktree/child/refresh/fallback/provider-switch 生命周期全覆盖。
- capability、execution provenance、provider terminal、provider route、MCP readiness 五类内部事实的 owner、scope、generation/revision、terminal、failure 和 public projection 不互相冒充。
- CLI/TUI/plain/headless/native server wire shape 保持既有边界；无 secret/raw reasoning/second truth。
- 完成记录准确区分 Darwin 本地、mock、真实 provider、Linux/Windows/Desktop/物理 terminal 未执行项；不新增依赖、不修改 Desktop、不 push。

## Plan 87–90 deterministic acceptance（2026-08-17）

本轮先收口当前已有领域契约。矩阵中的 `witnessed-local` 只表示 Darwin 本地 deterministic/mock witness；Plan 92 所有 provider-switch cell 保持 `pending(plan92)`，不能由 Plan 89 fallback/reasoning continuity 代替。真实 provider、真实第三方 MCP/OAuth 和平台/UI 项使用 `not-executed`，不与 pending 混淆。

| canonical owner / consumer boundary | happy / typed state | stale、deny、failure | duplicate、late、terminal | resume、fork、compaction、child、worktree | bounded public projection | evidence status |
|---|---|---|---|---|---|---|
| Deferred capability — owner `core::tools::tool_search`；consumer `ToolSource` dispatch | `search_returns_schema_and_generation_from_one_source_snapshot`；`same_response_search_then_readonly_call_observes_request_order` | `readiness_revision_invalidates_discovery_without_calling_source`；`wire_call_rejects_readiness_refresh_after_dispatch_validation`；`same_name_winner_change_cannot_reuse_or_hop_an_unlock`；`receipt_field_mutations_fail_before_permission_and_source`；`tools::tests::unavailable_source_route_fails_before_call_and_is_not_unknown` | stale/deny 均断言 permission/source call 为零；失败 wire call 不 mint 新 receipt | `same_session_compaction_preserves_deferred_unlock`；`worktree_transitions_require_rediscovery_even_after_returning_to_base`；`subagent_has_fresh_receipts_and_cannot_use_a_shared_parent_receipt`；`conversation_reset_revokes_live_receipts` | receipt 不进入 history/rollout；`notice_lists_every_deferred_name_and_stays_static_across_unlocks` 固定 provider tool array | `witnessed-local` |
| Execution provenance — owner `core::execution_provenance`；consumers background registries | `canonical_execution_ids_are_typed_and_strict`；`parent_reference_is_flat`；`provenance_history_appends_valid_attempts_and_preserves_bad_input` | `non_agents_cannot_claim_mailbox_identity`；`persisted_receipt_revalidates_kind_combinations` | `typed_registration_rejects_a_stale_receipt_handle` 同时断言 canonical receipt、Agent kind、`BackgroundExecutions/ParentInboxBody`；`stop_wins_the_terminal_race_and_suppresses_delivery`；`completion_wins_before_stop_and_finishes_only_once` | Program/Workflow `provenance.json`、journal v3、sub-agent worktree tests 保留 typed parent/workspace evidence；坏 evidence 只 safe miss | `receipt_is_bounded_and_omits_free_form_secrets`；receipt 不进入 Event/Inbox/provider/history/Task/usage | `witnessed-local` |
| Provider terminal/reasoning/usage — owners provider adapter → agent/History/Rollout | 三 rail typed stop/outcome fixtures；`every_valid_terminal_outcome_keeps_reported_usage`；`validated_terminal_usage_is_recorded_before_assistant_message` | `semantic_error_outcomes_record_content_without_retry_or_fallback`；`partial_stream_error_completes_open_item_without_retry`；`fallback_fails_closed_on_incompatible_reasoning_history`；`failures_and_missing_usage_do_not_create_records` | accepted terminal usage only once；`server::partial_stream_is_recoverable_with_error_terminal` 锁定单 terminal | `reasoning_provenance_and_typed_terminal_survive_resume_and_fork`；`compaction_keeps_reasoning_provenance_on_the_verbatim_tail`；usage fork/compaction tests | protocol `public_projection_strips_provider_and_opaque_reasoning`；server `snapshots_project_reasoning_without_replay_secrets` / `thread_read_strips_reasoning_replay_secrets` | `witnessed-local` |
| MCP readiness — owner CLI `McpToolSource`；consumers MCP transport/core `ToolSource` | `readiness_receipt_is_typed_monotonic_and_secret_free`；stdio/HTTP handshake、tool/resource fixtures | `static_bearer_availability_tracks_missing_and_rejected_credentials`；`configured_failed_resource_server_is_unavailable_not_missing`；`refresh_failure_retains_catalog_but_blocks_until_recovery`；`session_reinitialize_updates_capabilities_and_requires_revalidation` | `closed_receipt_cannot_be_overwritten_by_late_refresh_or_request`；pending drain/drop、health-close tests | `program_manifest_rejects_readiness_revision_change` 阻断 sampling ABA；refresh 恢复 generation+1 并要求 rediscovery；lifecycle owner explicit shutdown | native `mcpServerStatus/list` 继续 immutable protocol 1.0 startup snapshot；readiness receipt、endpoint/credential 不进入 public wire | `witnessed-local` |
| Public projection — owners protocol/server wire；consumers headless/TUI | `outgoing_shapes_carry_the_jsonrpc_field`；`mock_headless_json_is_ndjson_only`；TUI `history_replays_into_cells_with_tool_status_pairing` | strict cursor/generation/sequence 与 malformed input tests | background/message lifecycle 以 id upsert；partial terminal 只投影一次 | `event_sync_replays_then_resume_changes_generation_and_snapshots_history`；`thread_read_list_resume_and_fork_preserve_runtime` | snapshot/read/headless 只消费 `Message::into_public_projection` / `wire::project_event`，无 receipt、secret、opaque reasoning 或第二真值 | `witnessed-local` |
| Provider route transition — Plan 92 owner | catalog/frozen route、typed timeline、request view、provider-aware usage 已实现 | switch CAS、busy/persist failure、bad provenance、fallback fail closed 已有 focused witness | route-aware late terminal/usage、switch 前后 child/resume/fork/compaction 已接通 | provider-aware public route revision、native route event/snapshot 已接通 | `witnessed-local` |

### 本轮新增的最小 gap tests

- `core::tools::tool_search::same_session_compaction_preserves_deferred_unlock`：实际执行 manual compaction，整对象断言 replacement、exact unlock receipt 与 provider tool surface 不变，随后同 receipt 调用成功且 source 只执行一次。
- `core::tools::tool_search::wire_call_rejects_readiness_refresh_after_dispatch_validation`：普通 discovered direct call 在 wire gate 内发生 readiness revision 变化时 fail closed；零 source I/O，旧 receipt 不自动刷新。
- 扩展 `core::tools::background_executions::typed_registration_rejects_a_stale_receipt_handle`：winner callback 必须收到同一 canonical receipt，且 kind/terminal owner/delivery 为 `Agent` / `BackgroundExecutions` / `ParentInboxBody`。

其余 cell 直接复用 Plan 87–90 已有 owner-local typed/bytes/lineage/terminal/negative witness；没有为 Plan 91 复制私有 receipt、建立静态自评 report 或扩 public/production API。

### 本轮实际验证

- 先以 `cargo test -p kloop-core -- --list` 确认三个 focused witness 的完整路径，再分别用完整路径 `--exact` 运行，均为 1/1；曾用短名配 `--exact` 得到 `0 passed`，未将其计为通过。
- `cargo test -p kloop-core`：720 passed。
- `cargo fmt --all -- --check`；workspace all-target/all-feature Clippy `-D warnings`；workspace all-target/all-feature tests 全绿，真实 credential contract 2 项 ignored。
- 代表性数量：CLI 118 + headless contract 3、MCP 31 + client 14、protocol 15、provider 33 + Anthropic 18 + Chat 14 + Responses 17、server 25 + integration 33、TUI 167。
- `cargo run --locked -p kloop -- --mock` 与 `--mock --plain` 均完成 7 rounds；`--mock --headless` 输出最终答案；`--mock --headless --json` 输出同一 `threadId` 的 NDJSON 并以唯一 `turn/completed` 收口。
- `git diff --check` 通过；未新增依赖、production runtime、public schema、Desktop 改动或 push。
