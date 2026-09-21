# Plan 180 — 前台跑完就结束,后台要活一整个会话

> 本批判据与统一纪律见 `HANDOFF.md` 第〇节。

## 一、现状

`rust/crates/core/src/tools/bash.rs`,**958 code 行 / 2731 总行**。
一刀就能切开,而且两半几乎一样大:

| 行 | code 行 | 是什么 |
|---|---|---|
| 1–572 | 473 | **前台**:`BashInput` 解析、`shell_spec`、`call_sandbox`、`run_foreground`、`BoundedStream` 与四个读取/收尾函数 |
| 573–1118 | 485 | **后台**:`BgStatus` / `BgSandbox` / `BgShell` / `ShellRegistry` / `BackgroundShells`(含 `Drop`)、`BackgroundMonitor`、`monitor`、`read_tail` |

两半的生命周期完全不同:前台是"一次调用之内跑完、拿到 stdout 就结束";后台是
**一个跨整个会话存活的注册表**(`BackgroundShells` 是 `Arc<Self>`,有自己的 `Drop`,
带一个监控任务)。它们共享的只有 `shell_spec` 和 `call_sandbox` 这两个"怎么起一个 shell"。

## 二、切法

| 新文件 | 内容 | 预估 |
|---|---|---|
| `tools/bash/background.rs` | 573–1118 整段 | ≈485 |
| `tools/bash.rs`(留) | 前台 + 两半共用的 `shell_spec` / `call_sandbox` / `BashInput` | ≈473 |

`BackgroundShells` 是 `pub`(被 `ToolCtx` 持有),从子模块 `pub use` 出去,对外符号不变。
`shell_spec` / `call_sandbox` 提到 `pub(super)`。

**先确认再动**:`BoundedStream` / `read_bounded_stream` 是不是两半都在用。
是的话它们属于共用层,留在 bash.rs;只有前台用就跟前台走。

## 三、坑

- **`Drop for BackgroundShells`(951)和 `monitor`(983)之间有生命周期约定**
  (drop 要把监控任务收掉)。搬的时候两者必须落在同一个文件,别把 Drop 留在父模块。
- 这个文件的 `#[cfg(test)] mod tests` 在 1119,**里面前台后台的测试是混着的**。
  搬代码时按测试实际触达的私有项分,不要按测试名猜。
- 后台 shell 牵着沙箱(`BgSandbox`)与权限;**重构不碰这两条线**。

## 四、验收

- `make check` 全绿;`tools/bash.rs` 原有测试一条不少。
- bash.rs 降到 ≈473。
