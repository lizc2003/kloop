# Plan 130 — 手工压缩的那一分钟,屏幕什么都不说

> 来源:2026-09-09,用户 dogfood 两连:
>
> 1. 「手工运行 /compact, 界面上看不压缩的过程」——截图里只有
>    `⁙ Working (51s · esc to interrupt)`,转录里连敲下去的 `/compact` 都没留痕。
> 2. 「另外,压缩完成后,ctx 的百分比没有更新」——截图里 footer 仍是 `63% ctx`。

## 一、两件事,同一个根:命令通道不走 turn 的那套记账

自动压缩发生在 turn loop 里,手上有 `ui`:开工前 `ui.emit(Event::Note("predicted
context overflow; compacting history"))`(`agent.rs:484`,反应式那条在 `:549`),turn
结束再补一发 `Event::Usage`。所以自动压缩既看得见 `[compacting history]`,ctx 也会动。

`/compact` 走 slash 通道,这条通道两样都没有:

1. **没有 ui**。`commands::run_with_provider_state`(`commands/mod.rs:194`)签名里就没有
   事件出口,`commands/compact.rs` 只能等 `compact_once` 返回后交一行 `output`。而
   `compact_once` 干的是"把整段对话发给模型、等一份摘要"——截图里那 51 秒全在这。
2. **没有 Usage**。TUI worker 的 `WorkerMsg::Turn` 分支在 `run_turn` 之后发
   `CoreEvent::Usage(history.estimated_tokens())`(`tui/src/lib.rs:368`),`WorkerMsg::Command`
   分支没有;footer 的 `context_used` 只被 `Event::Usage` 写(`app.rs:970`)。于是
   `/compact` 把 history 砍掉之后,百分比还停在压缩前。**`/clear` 同病**:`ClearTranscript`
   只清 cells,不动 `context_used`,清空后 ctx 照样不归零。server 早就对了——它在
   `run_turn_or_command` 之后统一发 Usage(`server/src/lib.rs:1755`),漏的只有 TUI。

再叠一层:TUI 对 slash 行**不建 User cell**(`app.rs:1361` 原注释:"it is not a message,
so no User cell"),回车之后连"命令被受理了"都看不出来。三样凑一起,那一分钟里屏幕上
唯一变化的是秒数。

## 二、做了什么

1. `commands::run` / `run_with_provider_state` 收 `ui: &dyn Ui`,`/compact` 在
   `compact_once` **之前** emit `Note("compacting history")`——自动路径那两句的共同尾巴,
   同一套词。三个前端都现成有 ui:TUI worker 的 `Arc<dyn Ui>`、server 的 `ThreadUi`、
   plain REPL 的 `StdoutUi`(印成暗色 `[compacting history]`;server 投成 `note` 通知)。
2. 这一句**在结果已知之前**发:压缩失败、或者短到没得压,都照样先说。否则"什么都没发生"
   和"正在发生"长得一模一样,而区分这两者正是这条 note 的全部意义。
3. `send_command_result_events` 多收一个 `context_used`,在最后补发 `CoreEvent::Usage`:
   命令一结束 ctx 立刻更新(`/compact` 掉下去,`/clear` 归零)。
4. slash 行回显成 `Cell::User`:敲下去就看得见。它仍然不进 History——回显是给人看的,
   不是一条 user message。

## 三、非目标

- **不把活动行的 "Working" 换成 "Compacting"**:那个动词现在只由 `app.running` 驱动,
  要按当前工作换词得再拉一条状态通道;转录里那行 `[compacting history]` 已经说清了
  它在干什么,而且和自动压缩长得一样。
- **不流式显示摘要正文**:摘要是写给下一个 context 的,不是给人读的。压缩期间该看到的
  是"正在压缩",结束后该看到的是回执(`history compacted: N summarized, M kept verbatim`)。

## ✅ 已完成(2026-09-09 夜至 09-10;提交 SHA 以本条所在提交为准)

- `crates/core/src/commands/mod.rs`:`run` / `run_with_provider_state` 收 `ui: &dyn Ui`
  并转给 `compact`;其余命令不碰它。
- `crates/core/src/commands/compact.rs`:`compact_once` 之前 emit
  `Note("compacting history")`。
- `crates/tui/src/lib.rs`:worker 传 `ui.as_ref()`;`send_command_result_events` 收
  `context_used` 并在最后补发 `CoreEvent::Usage`。
- `crates/tui/src/app.rs`:`on_enter` 把 slash 行推成 `Cell::User`。
- `crates/server/src/lib.rs`、`crates/cli/src/main.rs`:各传自己的 ui(ThreadUi /
  StdoutUi)。无 wire/schema 变化——`note` 通知本来就有,只是多了一条。
- README:TUI 回显与 ctx 刷新时机各一句;server 那段补 `/compact` 会先发 `note`。

### 测试

- `commands::tests` 三条 compact 测试各加 `assert_eq!(ui.notes(), ["compacting history"])`,
  覆盖成功 / provider 失败 / no-op 三条路径。**失败与 no-op 两条就是"发生在工作之前"的
  证据**:no-op 那条 provider 一次都没被调用(`seen` 为空)却已经有 note。
- `clear_command_events_order_transcript_then_graph_fence_then_system_then_gauge`:命令
  事件序列末尾多断言一条 `Usage(40_000)`。
- `slash_command_routes_only_when_idle`:原来断言 `cells.is_empty()`(锁的正是"敲下去
  什么都不显示"),改为断言回显 `Cell::User("/help")`。

### 验证

`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、
`cargo test --workspace` 各自单独跑并当场取退出码(HANDOFF 111(b2)),均 0。全量那次跑在
只差一句注释和 README 的树上;之后本机内存吃紧,连着两次全量跑被系统杀掉(不是测试失败,
日志里 25 个 test binary 全 ok、停在 doc-test),最终树改用
`cargo test -p kloop-core -p kloop-tui -p kloop-server -p kloop --lib --tests -j 2`
复核:exit 0,core 800 / tui 209 全绿。**doc-test 只由前一次全量覆盖**,本片没有改动任何
文档示例。

**没做成的一项**:本想用 `--mock --plain` 手跑一次 `/compact` 看 stderr。`--mock` 是固定
的 7 轮演示脚本、根本不读 stdin,到不了 REPL,所以这条没跑成。plain REPL 那行 note 的
渲染路径是既有的(`StdoutUi` 的 `as_note` 兜底,就是 mock 输出里 `[bash {…}]` 那些暗色
行),本片没动它;TUI 的真机确认留给下一次 dogfood。
