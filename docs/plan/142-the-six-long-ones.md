# Plan 142 — 那六个长家伙

> 来源:plan 136 的全仓通读,第五节非目标里的一条(四.2)。2026-09-11 用户要求把 136 的
> 非目标排成计划;本 plan 是其中一条,与 138–141 同批。**依赖 plan 138**(见第二节),
> 其余各节可独立开工。

## 一、清单(行数为 plan 136/137 落地后的实测)

| 行数 | 位置 | 性质 |
|---|---|---|
| 548 | `core/agent.rs:315` `turn_rounds` | 8 种 `Sampled` 分支 + 4 种 outcome 分支 + 两条恢复路径揉在一个 `loop` |
| 425 | `provider/responses.rs:910` `stream` | SSE 事件状态机,最复杂的一条 rail |
| 367 | `tui/app.rs:653` `App::apply_core` | 一个 `match Event` 把所有前端投影摊平 |
| 366 | `cli/main.rs:96` `main` | 装配代码没分家,内嵌上百行的 `ConfigFactory` 闭包 |
| 356 | `core/tools/mod.rs:1090` `run_one` | 重命名校验 → 能力门 → 源一致性三次复查 → hook → 权限 → 准备 → 执行 → post hook |
| 339 | `tui/lib.rs:944` `ui_loop` | `tokio::select!` 里塞着全部键盘命令的处理 |

(`core/tools/mod.rs:689` `builtin_defs` 231 行**不在此列**:它几乎全是 `json!` 数据,
而且 plan 138 会重排它。)

## 二、顺序:`run_one` 等 plan 138

`run_one` 的长度里有一大截是 `execute_tool` 之前那串按名字做的判断(重命名迁移错误、
root-only task 工具、agent_type allowlist、shell 可用性),以及 `execute_tool` 自己那张
35 分支的分派表。**plan 138 把它们收进 `Builtin` 之后再拆,拆出来的边界才是稳定的。**

其余五个与 138 无关,可任意顺序。

## 三、逐个的拆法建议(不是规定)

**`turn_rounds`(548)** —— 最值钱也最危险。它的长度来自"每个退出点都要构造完整的
`Ending`"。建议只做两件事,**不要**试图重写循环:
- 把 `Sampled::Overflow` 那段(预测式压缩 + 反应式压缩 + 两种 NoOp 处理)抽成
  `async fn recover_from_overflow(...) -> Result<(), Ending>`;
- 把 `AssistantOutcome` 的四分支抽成 `fn classify_outcome(...) -> OutcomeAction`,
  action 是 `Continue` / `Break(Ending)` 的小 enum。
两步各自能单独编译、单独跑 `crates/core/src/agent/tests.rs`(3719 行,覆盖很密)。

**`responses::stream`(425)** —— **先做 plan 139**(SSE 骨架抽取)再看。139 会拿走开头
和结尾的骨架;剩下的状态机若仍超过 300 行,按事件族分组抽(`response.*` / `output_item.*` /
`function_call_arguments.*` / out-of-band),每族一个 `fn handle_x(&mut state, &Value)`。

**`App::apply_core`(367)** —— 按 `Event` 的族拆:item 生命周期(`ItemStarted`/`ItemDelta`/
`ItemCompleted`)一组、后台/调度/peer 三个 updated 一组、其余(`Usage`/`CwdChanged`/
`ModeChanged`/`Note`/`TaskGraphUpdated`)一组。`App` 是纯状态机,测试(`tui/app.rs` 的
`mod tests`)直接喂事件断言 cells,拆完逐条不变即可。

**`main`(366)** —— 最机械的一个。三件事:
- `ConfigFactory` 闭包搬成一个具名函数(它捕获 `args`/`provider`/`runtime`/`session_store`
  四个 Arc,搬成一个持有这四者的 struct 再 `impl Fn` 或直接返回闭包);
- `--serve` / headless / TUI / plain 四条路各自一个 `async fn run_*`;
- `main` 本身只剩参数解析 + 分派。
`crates/cli/tests/` 下的 PTY 与 headless 契约测试是它的网。

**`ui_loop`(339)** —— `tokio::select!` 的每个分支体搬成方法:
`fn on_key_command(&mut self, cmd: Command) -> ControlFlow<Result<()>>` 是最大的一块
(现在那个 14 分支的 `match app.on_key(...)`)。注意 `ui_loop` 借用了一堆局部
(`current_cancel`、`turn_started`、`app`),搬之前可能要先把它们收进一个 `UiState` struct。

## 四、共同的纪律

1. **一个函数一个提交**(或至少一次可独立回滚的改动)。六个混在一起没人能 review。
2. **抽出来的每个函数都要能被现有测试覆盖到**;如果某段代码抽出来之后没有任何测试经过它,
   那说明拆的位置不对(或者那段代码本来就没测试——那要先补,不是先拆)。
3. **不改行为,一行都不改。** 判据不是"看起来等价",是**测试文件一个字节都不动**还全绿。
   这一条是 HANDOFF 教训 37 的形状(把宽回调收敛成单事件流时,"行为不变"的判据是各前端
   投影出的旧输出逐字节不变)。
4. **`turn_rounds` 和 `ui_loop` 涉及 `Send` 边界**(教训 1:递归 async 的 Send 推断有共归纳
   盲区)。抽出 async fn 时如果撞上 Send 推断失败,**不要**加 `Box::pin` 到处救火——退回去,
   把那一段留在原地,换个边界。`execute_tool` 的类型擦除是既有的递归边界,别动它。

## 五、非目标

- 不为了行数而拆。`builtin_defs` 那 231 行是数据,拆了更难读。
- 不顺手改任何一处的**逻辑**——看到 bug 记下来另开 plan,别在重构提交里混修复。
- 不引入新的抽象层(trait / 泛型 / 宏)。这是把一个函数切成几个函数,不是重新设计。

## 六、验收

每个函数一条:
- 目标函数降到 **150 行以下**(或在 plan 里写明为什么它不该降);
- **相关测试文件的 diff 为空**;
- fmt / clippy(`-D warnings`) / `cargo test --workspace` 各自单独跑、当场取退出码。

全部做完后,plan 136 第四节第 2 条(超长函数)销账;把结果写回 136 的 ✅ 节。
