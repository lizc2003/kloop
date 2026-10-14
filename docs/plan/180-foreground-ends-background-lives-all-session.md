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

## 五、✅ 完成（2026-10-14）

实现提交：`0648d75`（`refactor(plan180): separate session background shell lifecycle`）。

开工时 `bash.rs` 已从计划中的 2731 总行增至 3480 总行，生产代码约 1064 行。
按生命周期拆分，行为、权限和沙箱策略保持原样：

| 文件 | 职责 | 生产 code 行 / 总行 |
|---|---|---|
| `tools/bash.rs` | 工具入口、输入解析、前台执行与共用启动配置 | 446 / 1877 |
| `tools/bash/background.rs` | 会话级注册表、状态、监控、Drop、输出与停止工具 | 644 / 1305 |
| `tools/bash/testutil.rs` | 前台、后台共用的进程树、事件与沙箱测试夹具 | 仅测试 / 364 |

- `BoundedStream` / `read_bounded_stream` 只被前台读取链使用，留在父模块。
  后台通过继承的文件描述符写输出文件，沿用原有文件尾读取。
- `BackgroundShells` 与后台工具从父模块重导出，调用方路径和可见范围不变。
  父模块调用的 `spawn_background` 提至 `pub(super)`；共用的 `shell_spec`、
  `call_sandbox` 保持私有，Rust 子模块本来就能访问父模块私有项，无须提升。
  后台监控还复用原有进程树回收超时，仍从父模块导入。
- 52 条原有测试全部保留：前台与共用契约 36 条，后台 16 条；跨前台、后台的测试
  留在工具入口一侧。测试正文未改，只移除一条要求固定旧证据行号的过期注释。
- 本机 parity 语料固定了旧源码范围。经单独批准，仅更新其中 4 条 kloop 证据的
  位置，覆盖拆出的实现、测试与共用夹具；参考实现与捕获未改，语料仍不进仓库。

验证：在外层执行沙箱之外运行完整 `make check`，fmt、clippy、workspace debug
全量测试与本机 parity 全绿。本机实际执行的 46 条 Bash 测试全部通过；另 6 条
Windows 条件测试原样保留，未在 Windows 上执行。另逐一核对全部 52 条测试的
属性与祖先模块的平台条件、111 个函数正文，以及拆分前后的整块生产代码；
除必要的可见性和模块路径调整外，生产代码保持一致。

## 六、审查后补验与清单修正（2026-10-14）

实现提交漏了教训 166/183 要求的 Windows 类型检查。上节的本机门禁和文本核对
都没有编译 Windows 条件代码；本次补齐验证，并修正 HANDOFF 能力清单的落点。

从 `0648d75` 导出独立的临时 Rust workspace，使用单独的 target 目录；检查真实
core 和测试目标，未对 Rust 接口做 stub。工具链钉住 `1.98.1`，确认对应工具链已
安装 `x86_64-pc-windows-msvc` target。实际命令：

```sh
cargo +1.98.1 check --offline --locked -p kloop-core \
  --target x86_64-pc-windows-msvc --all-targets --all-features
```

构建环境沿用教训 201：`CARGO_TARGET_DIR` 指向临时副本自己的产物目录，仅为
Windows triple 设置 `CC_x86_64_pc_windows_msvc`、`CXX_x86_64_pc_windows_msvc`
与 `AR_x86_64_pc_windows_msvc`，由临时脚本生成空 C 编译、归档产物。

- 先在临时副本的 Windows 夹具构造函数、前台 MSIX 测试和后台 shutdown 测试各
  注入一个未定义名字。交叉 check 在 `lib test` 中报告预期的 3 个 E0425，确认
  夹具和两处测试调用面确实进入了 Windows 编译；被 ignore 的测试也参与编译。
- 移除三个故意错误，逐字节确认三份 Bash 源码与原工作区一致，再用同一工具链
  清理临时 target 中的 core 产物并重新检查。最终 check 通过；编译器另报告一条
  `TestConfig::login_env` 的 dead_code 告警，定义所在的 `tools/mod.rs` 未改动。
- 验证覆盖 Rust 类型、名字解析与测试目标，不证明 C 代码、链接或 Windows 上的
  执行结果。6 条 Windows 条件测试仍未执行；原有 macOS 门禁结果不变。
- HANDOFF 能力清单的“后台 bash”文件指针补上 `core/src/tools/bash/background.rs`，
  与实际注册表、monitor、Drop 的位置一致。

仓库本次仅改文档，按纯文档规则核对完整差异、引用与状态，并运行
`git diff --check` 与 `git diff --cached --check`；临时副本、脚本和产物不进入提交。
