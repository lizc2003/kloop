# Plan 183 ✅ — main.rs 里藏着一个完整的 REPL

> 本批判据与统一纪律见 `HANDOFF.md` 第〇节。

## 一、现状

`rust/crates/cli/src/main.rs`,**833 code 行 / 1056 总行**。它是十三个 `mod` 的门厅,
本该只做"解析参数、选前端、起 runtime",但**后半个文件是一个完整的 `--plain` REPL**:

| 行 | code 行 | 是什么 |
|---|---|---|
| 1–539 | 435 | 门厅:`mod` 声明、`ProcessState`、`SessionState`、`main`、`run`、`run_front_end`、`serve_config_factory`、`run_serve`、`run_headless_turn`、`mcp_subcommand`、`server_skills_reader` |
| 540–994 | 398 | **plain 前端**:`run_plain`、`run_tui`(只是转调)、`read_stdin_if_piped`、`spawn_ctrl_c`、`PlainInput`、`PlainCtrlC`(含 `Drop`)、`PlainInputThread`、`next_plain_input*`、`run_plain_operation`、`plain_main` |

`ui.rs`(StdoutUi)和 `headless.rs` 都已经是自己的文件了,**plain 这一个前端没有**。

## 二、切法

| 新文件 | 内容 | 预估 |
|---|---|---|
| `plain.rs` | 540–994 里属于 plain 的全部:`PlainInput` / `PlainCtrlC` / `PlainInputThread` / `next_plain_input*` / `run_plain_operation` / `plain_main` / `run_plain` | ≈380 |
| `main.rs`(留) | 门厅 + `run_tui`(一行转调 `kloop_tui::run`)+ `read_stdin_if_piped`(headless 也用) | ≈450 |

## 三、坑

- **`read_stdin_if_piped`(615)和 `spawn_ctrl_c`(633)先查调用点**:如果 headless/serve
  也在用,它们属于门厅,不跟 plain 走。别按位置猜。
- `PlainCtrlC` 有 Windows 条件编译的 `Drop`,它中止会话级信号转发任务;
  **Drop 和它的构造必须同文件**（开工核对：这里不负责恢复终端状态）。
- `main.rs` 的 `#[cfg(test)] mod tests` 在 995,1056 总行里只有 61 行测试——
  这个文件测试很薄,所以**这次重构的安全网主要是 `cli/tests/plain_pty.rs`**。
  动手前先确认那条 PTY 测试是绿的,它是唯一真正跑 plain 前端的东西。
- `main.rs` 有 `build.rs` 戳进来的版本常量(plan 161),别把 `KLOOP_VERSION` 那条路搬歪。

## 四、验收

- `make check` 全绿;`cargo test -p kloop --test plain_pty` 通过。
- `main.rs` 降到 ≈450。

## 五、✅ 完成（2026-10-16）

实现提交：本节所在提交（`refactor(plan183): extract the plain CLI frontend`）。
提交号可用 `git log -1 --format=%h --grep='^refactor(plan183):'` 查询。

开工时 `main.rs` 已增至 **924 生产 code 行 / 1206 总行，3 条单测**。沿用
plan 181/182 的口径：取 `#[cfg(test)] mod tests` 之前的非空、非 `//` 注释
物理行；其他单独的 `#[cfg(test)]` 项照计，不展开条件编译。

| 文件 | 职责 | 生产 code 行 / 总行 | 原有单测 |
|---|---|---|---|
| `main.rs` | 进程/会话初始化、前端分派、server/headless/TUI 入口与 headless 输入/信号帮助函数 | 542 / 756 | 2 |
| `plain.rs` | plain/Mock 入口、行输入线程、会话级 Ctrl+C、Inbox 唤醒、命令/turn 与退出清理 | 400 / 478 | 1 |

- 按调用点确认 `read_stdin_if_piped`、文件类型判断与 `spawn_ctrl_c` 属于 headless，
  留在 `main.rs`；`run_tui` 的配置闭包也保留。542 行高于旧估算的 450，
  不为追行数再拆这些门厅职责。
- `run_plain` 与全部 plain 私有项迁入 `plain.rs`。仅入口提至 `pub(super)`，
  父模块的 `ProcessState`、`SessionState` 及字段保持私有，子模块直接访问。
- 调度唤醒单测随输入帮助函数迁移；stdin 文件类型与 mock server skills 两条
  单测留在父模块。三条单测的名称、属性、正文保持原样，plain PTY 夹具与测试未改。
- 逐块核对迁入代码（包括 Windows 构造、接收与 Drop）及单测原文；原有 30 个
  函数名及数量保留。父文件除删去迁移块、清理 import、声明模块、限定分派路径与
  修正文档指针之外逐字一致。版本串仍经 `args::version_string`，`build.rs` 未改。
- DESIGN 原有目录表误把 `StdoutUi`、参数/会话选择都归在 `main.rs`，已原位改写，
  补齐 plain、ui、headless、args 的真实落点；行为说明仍成立，无需追加新契约。

动手前的 `cargo test --locked -p kloop --test plain_pty` 在外层执行沙箱之外
通过：2 条 plain 行为测试与 5 条共用 PTY 夹具测试全绿。

拆分后在外层执行沙箱之外运行完整 `make check`：fmt、clippy、workspace debug
全量测试与本机 parity 全绿；CLI 154 条单测、plain PTY 7 项、TUI PTY 20 项
以及 core 1124 条测试全部通过。本机语料未修改。完整差异与
`git diff --check`、`git diff --cached --check` 已核对；交接见 HANDOFF 教训 237。

Windows 使用独立临时 workspace 与 target，钉住 `1.98.1`，检查真实 CLI 及其依赖，
不为 Rust 接口做 stub：

```sh
cargo +1.98.1 check --offline --locked -p kloop \
  --target x86_64-pc-windows-msvc --all-targets --all-features
```

先在 Windows 构造、Windows Drop 与迁移单测内各注入一个未定义名字，核对预期
的三个 E0425；移除探针，逐字节核对全部 crate 副本与工作区一致，清理 CLI 产物
后复验通过，无告警。按教训 201，仅为 Windows triple 的 C 编译与归档生成空
产物；本检查只覆盖 Rust 类型、名字解析与测试目标，不代表 C 代码、链接或
Windows 执行通过。
