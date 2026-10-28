# Plan 270 — 模块切分与测试整理

> 2026-10-28 立。来源：全仓只读审查与随后的架构讨论(HANDOFF「〇⁵」节)。
> 状态：待实现。**排在最后**:267 会把参数 helper 移出 `tools/mod.rs`,264 会改 catalog 部分，
> 先做它们再切。纪律照 plan 177–186(行为零变更、测试跟代码走、可见性只提到够用)。
> 行号是 2026-10-28 的读数，开工先自己量。

## 问题

1. **`core/src/tools/mod.rs` 4719 行，职责堆叠**:分发与超时(`execute_tool`、`run_gated`)、门禁
   接线、工具目录与延迟加载分区(`all_tool_defs`、`partition_source_defs`、`reserved_names`)、参数
   helper(267 之后移走)、路径解析。plan 177–186 那一批把它列为"要读进去才能判边界、没读之前不立
   plan"——这次审查已读过，边界是上面这几块。
2. **以计划编号命名的测试文件放在 `src/tools/` 下**:`plan49_parity_tests.rs`、`plan50_…`、
   `plan52_…`、`plan53_…`、`plan56_…`、`plan57_…`、`plan58_…`、`plan59_acceptance_tests.rs`、
   `plan151_…`、`plan197_…`。读者从文件名看不出它测什么。
3. **测试临时目录各写一份**:`core/src/rewind.rs:171`、`diff.rs:248`、`project.rs:384`、
   `compact/branch.rs:237`、`history.rs:1255`、`commands/clear.rs:103`、`session_store.rs:292`、
   `cli/src/context.rs:369`。只有部分带序号计数器;`diff` 版建目录前不清理旧的;`history` 版不建目录。
   `tempfile` 已是 workspace 依赖。(git 仓库夹具的隔离在 plan 247。)

## 开工先问用户

- 第 2 条的改名有一个代价：本机 parity 语料的校验器把 kloop 位置钉在这些文件上(plan 242 的完成
  记录里，`plan52_parity_tests.rs` 的行号区间就因此红过一次)。(a) 改名并同步本机校验器(本机改动
  不进仓库);(b) 不改名，只在每个文件头加一行"测什么"的模块注释。推荐 (b)。
- `tui/src/app.rs`(5434 行)、`permissions.rs`、`fs.rs` 要不要也在这条里切?推荐不切：审查没有
  按"职责边界"读过它们，照 177–186 的判据，没读之前不立。

## 做法

1. `tools/mod.rs` 拆成 `tools/dispatch.rs`(分发、超时、取消)、`tools/catalog.rs`(工具目录、
   延迟加载分区、合并警告)、`tools/paths.rs`(路径解析),`mod.rs` 留门面与 `ToolCtx`。开工第一步
   `grep -rn 'tools::' crates/` 列全对外符号，收工按表核对。
2. 按问答处理测试文件名。
3. 临时目录统一换成 `tempfile::TempDir`(或一个 core 的 `#[cfg(test)]` helper),逐个文件替换。

## 验收

- diff 除 `use` 与可见性外接近零改动;全部测试不改断言即通过;对外符号表一个不少不多。
- 外层执行沙箱之外 `make check`;HANDOFF 记进展。
