# Plan 183 — main.rs 里藏着一个完整的 REPL

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
- `PlainCtrlC` 有 `Drop`(687),它恢复终端状态;**Drop 和它的构造必须同文件**。
- `main.rs` 的 `#[cfg(test)] mod tests` 在 995,1056 总行里只有 61 行测试——
  这个文件测试很薄,所以**这次重构的安全网主要是 `cli/tests/plain_pty.rs`**。
  动手前先确认那条 PTY 测试是绿的,它是唯一真正跑 plain 前端的东西。
- `main.rs` 有 `build.rs` 戳进来的版本常量(plan 161),别把 `KLOOP_VERSION` 那条路搬歪。

## 四、验收

- `make check` 全绿;`cargo test -p kloop --test plain_pty` 通过。
- 新文件 ≤800;`main.rs` 降到 ≈450 后跑 `make arch-baseline`,它会从基线里被摘掉。
