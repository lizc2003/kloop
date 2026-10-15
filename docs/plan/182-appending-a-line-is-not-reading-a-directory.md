# Plan 182 ✅ — 往文件尾巴追加一行,和读一个会话目录,不是一件事

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

## 五、✅ 完成（2026-10-14）

实现提交：本节所在提交（`refactor(plan182): separate rollout format, validation, and sessions`）。
提交号可用 `git log -1 --format=%h --grep='^refactor(plan182):'` 查询。

开工时文件已增至 **1715 生产 code 行 / 4128 总行，63 条测试**。此前新增的格式
版本、provider 路由校验、写租约、原子 id 占用、request stub 与 `tool_started`
均保留；第一节的旧行号不再作为切片依据。

本节 code 行口径（2026-10-15 统一）：取 `#[cfg(test)] mod tests` 之前的非空、
非 `//` 注释物理行；其他单独的 `#[cfg(test)]` 项及其模块声明照计，不展开条件编译。
据此将父模块的 334 更正为 336，与 plan 181 对测试夹具模块声明的计数一致。

| 文件 | 职责 | 生产 code 行 / 总行 | 原有测试 |
|---|---|---|---|
| `rollout.rs` | 追加写入器、写租约、空会话清理与公开门面 | 336 / 727 | 10 |
| `rollout/line.rs` | JSONL 行、信封、快照、错误 serde 与同步判据 | 371 / 487 | 2 |
| `rollout/validate.rs` | 完整行读取、格式/信封/provider 校验与解析重放 | 338 / 820 | 12 |
| `rollout/sessions.rs` | 检查、恢复与配对修复、fork、摘要、目录查询、id/path | 697 / 2088 | 39 |
| `rollout/testutil.rs` | 七个共用测试夹具 | 仅测试 / 74 | 共用夹具 |

- 三个实现子模块均保持私有，原有 30 个公开符号全部从父模块保留或重导出；
  `civil_from_days`、`timestamp_id` 的 crate 内路径也保留，CLI/server/TUI 调用点未改。
- `SessionRead` 随会话 API 迁入 `sessions.rs`。确认 `repair_pairing` 只由
  `inspect_session` 调用后，与内部 `PairingRepair`、`unfinished_result` 一起搬入该模块；
  `PairingRepairStats` 是行格式的一部分，留在 `line.rs`。
- `LineMeta`、`RolloutLine` 与共用方法、`ParsedSession` 与解析结果字段、
  `ClaimedSession.path`、writer 需要的修复字段只提至 `pub(super)`。
  writer 原有私有字段和方法仍可由子模块访问，不增加新的公开接口。
- 63 条测试的名称、属性与正文全部保留，祖先模块仍只在测试时编译；路径检查中的
  Unix symlink 分支保持原条件。共用夹具供三个测试模块使用，fork 夹具留在会话测试内。
- 161 个函数逐一核对签名和正文；唯一正文差异是 id 占用函数通过
  `super::timestamp_id` 使用保留的 crate 内门面，其余 160 个函数一致。
  数据类型、serde 属性与 provider 校验条件均未改。

在外层执行沙箱之外运行完整 `make check`，fmt、clippy、workspace debug 全量测试
与本机 parity 全绿；core 1124 条测试通过，含全部 63 条 rollout 测试。
本机语料未修改。另核对完整差异，运行 `git diff --check` 与
`git diff --cached --check`；设计与能力清单已同步，交接见 HANDOFF 教训 236。

Windows 验证使用独立临时 workspace 与 target，钉住工具链 `1.98.1`，检查真实
core 与测试目标，不对 Rust 接口做 stub：

```sh
cargo +1.98.1 check --offline --locked -p kloop-core \
  --target x86_64-pc-windows-msvc --all-targets --all-features
```

先在五份迁移文件的测试或夹具中各注入一个未定义名字，检查报告预期的五个
E0425；移除探针、逐字节核对副本与工作区一致，清理 core 产物后复验通过。
沿用教训 201，仅为 Windows triple 的 C 编译与归档生成空产物；检查覆盖 Rust
类型、名字解析与测试目标，不代表 C 代码、链接或 Windows 上执行通过。
另有已有 `TestConfig::login_env` dead_code 告警，定义所在文件未改动。
