# Plan 182 — 往文件尾巴追加一行,和读一个会话目录,不是一件事

> 本批判据与统一纪律见 `HANDOFF.md` 第〇节。**本批最大的一条之一**,建议排在 177–181 之后。

## 一、现状

`rust/crates/core/src/rollout.rs`,**1500 code 行 / 3413 总行**。四层叠在一个文件:

| 行 | code 行 | 是什么 |
|---|---|---|
| 1–258 | 220 | **错误的线表示**:`TurnError`、`mod provider_failure_serde`、`RecordedTurnError`、手写的 `Serialize`/`Deserialize` |
| 259–424 | 137 | 数据类型:`TurnTerminal` / `SnapshotTerminal` / `SessionSnapshot` / `PairingRepairStats` / `LineMeta` / `RolloutLine` |
| 425–724 | 251 | **写入器**:`Rollout`(含 `Drop`)、`impl Rollout` |
| 725–1042 | 295 | **读与校验**:`intact_lines`(撕裂尾巴)、`validate_envelope`、`validate_provider_routes`、`validate_provider_message`、`parse_session`、`SessionRead` |
| 1043–1501 | 347 | **会话目录查询层**:`inspect_session` / `load_session` / `resume_session` / `fork_session` / `fork_points` / `session_origin` / `session_digest` / `repair_pairing` / `new_session_id` / `session_path` / `sessions_by_recency` |

最后一层最刺眼:`sessions_by_recency` 与 `session_digest`(plan 162 为 picker 加的)
跟"往一个 rollout 文件尾巴追加一行"没有任何关系,它们是**会话目录的查询 API**。

> **2026-09-24 补记(plan 204 先做了)**:`RolloutLine` 多了一种行 `ToolStarted { tool_use_id }`
> (serde tag `tool_started`),另有 `RolloutLine::needs_sync`(只有它 `sync_data`)、`Rollout::append_tool_started`、
> `ParsedSession::started` 与 `repair_pairing` 的第三个参数、`unfinished_result`。拆分时:`needs_sync` 跟 `RolloutLine`
> 走 `line.rs`;`started` 的收集在 `parse_session`(→ `validate.rs`);`repair_pairing`/`unfinished_result` 跟修复一起走。
> 第一节的行号与行数是 204 之前量的,开工时重量。

## 二、切法

| 新文件 | 内容 | 预估 |
|---|---|---|
| `rollout/sessions.rs` | 1043–1501 整段:打开/恢复/fork/digest/目录枚举/id 生成与路径校验 | ≈347 |
| `rollout/line.rs` | 1–424:`TurnError` 与它的 serde、`RolloutLine`、`LineMeta`、快照类型 | ≈357 |
| `rollout/validate.rs` | 725–1042:撕裂尾巴、信封链、provider route 与 message 校验、`parse_session` | ≈295 |
| `rollout.rs`(留) | `Rollout` 写入器 + `pub use` 门面 | ≈500 |

三刀而不是一刀,因为只切最后一层的话 rollout.rs 还有 903 code 行——比切之前好不了多少。

## 三、坑

- **`repair_pairing`(1502–1641)放哪要想一下**:它是 resume 时修两两配对的,
  既属于"读会话"也属于"校验"。建议跟 `sessions.rs`(它只被 `resume_session` 调),
  但开工时先 grep 调用点确认。
- **`pub` 面很大**。`inspect_session` / `load_session` / `fork_points` / `session_digest` 这些
  被 cli 的 picker、server 的 `thread_*` 方法、tui 的 resume 直接调。
  `pub use` 必须把**每一个**原来的 `pub` 符号原样导出——这条的回归风险主要在这里,
  开工第一步是把现在 `rollout::` 的对外符号列全(`grep -rn 'rollout::' crates/`)。
- **`validate_provider_routes` 是 plan 164 踩过的地方**(落盘校验和运行期共用一个函数,
  收紧一次就让已写出的会话打不开)。搬它的时候**一个条件都不能动**。
- 3413 总行里 1913 行是测试。按触达的私有项分文件,工作量与 181 同级。

## 四、验收

- `make check` 全绿;`rollout::` 的对外符号表一个不少不多(用开工时列的那张表对)。
- `rollout.rs` 降到 ≈500。
