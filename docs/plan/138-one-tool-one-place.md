# Plan 138 — 一个工具,一个地方

> 来源:plan 136 的全仓通读把这条列为「本轮通读里最值得做的架构改动」并挂进非目标
> (plan 136 第五节第一条)。2026-09-11 用户要求把 136 的非目标全部排成计划,本 plan 是
> 其中第一条,与 139–142 同批、可独立开工。**本 plan 自足:读完即可动手,不必重读 136。**

## 一、症状:加一个工具,编译器一句话都不说

一个内置工具的事实散在七个并行的 `match` 里,没有任何一处会因为另一处漏改而报错:

| # | 位置 | 这里决定什么 | 漏改的后果 |
|---|---|---|---|
| 1 | `core/tools/mod.rs` `builtin_defs` | ToolDef(名字/描述/schema)、depth 门、shell 门 | 工具压根不存在 |
| 2 | `core/tools/mod.rs` `all_tool_defs` 的 `surface.*` 分支 | 哪个前端能看见它 | 前端看不见 / 不该看见的看见了 |
| 3 | `core/tools/mod.rs` `is_concurrency_safe` | 能否与同批次工具并发 | **静默退化成串行** |
| 4 | `core/permissions.rs` `CallFacts::is_readonly` | 权限意义上是否只读 | **静默变成每次都问** |
| 5 | `core/tools/mod.rs` `execute_tool` | 执行分派 | `unknown tool: x`(这条会被发现) |
| 6 | `core/permissions.rs` `tool_title` | 审批面板的人类名 | 面板显示原始工具名 |
| 7 | `tui/toolrow.rs` | TUI 的一行展示 | 显示原始名 + 原始 JSON |

只有 #5 会在运行时明确报错,其余六处漏改都是**静默降级**。

`reserved_names`(plan 136 收口过)已经从 `tool_defs` 派生,不在此列。

## 二、这不是理论问题:现在就有四处不一致

把 #3 / #4 / #5 / #7 四张表的名字取差集,当场得到:

1. **`run_program` 在 `is_concurrency_safe` 里没有条目**,落到 `other =>` 分支
   (`find_source(...)` 对内置名返回 None)→ **串行**。而 `CallFacts::is_readonly` 把它和
   `workflow` 一起判为只读,`is_concurrency_safe` 里 `workflow` 又是 `true`。两个代码执行
   器,一个能并发一个不能,而 `run_program` 的注释写的是「itself touches nothing; every
   `tools.<name>()` and `agent()` call re-enters this same gate」——按这个理由它该和
   `workflow` 一样。**是有意还是遗漏,本 plan 开工时问用户(见第六节)。**
2. `enter_plan_mode` / `exit_plan_mode` / `enter_worktree` / `exit_worktree` 同样不在
   `is_concurrency_safe` 里 → 串行。这四个是会话状态变更,串行**是对的**,但它对的原因是
   "恰好落进了保守的默认",不是"有人写下了这个判断"。
3. **`toolrow.rs` 缺 15 个内置工具的展示**:`notebook_edit`、`send_message`、`list_agents`、
   `task_*`(5 个)、`cron_*`(3 个)、`schedule_wakeup`、`ask_user_question`、
   `enter/exit_plan_mode`、`enter/exit_worktree`。它们在 TUI 里显示成原始名 + 原始 JSON。
4. `tool_title` 只覆盖 8 个名字。对 MCP 工具透传是**有意的**(注释写明"用户配置的名字就是
   他认得的名字"),但 `enter_worktree` 这类内置工具走 hazard 分支弹确认时,面板标题也是
   原始名。

这四条不是要在本 plan 里逐个修好,而是本 plan 的**验收素材**:收敛之后,它们要么被修,
要么变成一句写下来的判断。

## 三、做法:一个 enum,让编译器点名

**选型理由**:静态表(`&[BuiltinTool { name, def_fn, exec_fn, … }]`)对前六项都成立,但
`execute_tool` 是 async 且捕获 `&ToolCtx`/`&EffectiveWorkspace`,塞进函数指针表要走
`Pin<Box<dyn Future>>` + 高阶生命周期,收益不抵复杂度。**enum + 穷尽 match** 拿到同样的
编译期保证,而每个 `match` 体仍是今天那段代码。

```rust
// crates/core/src/tools/builtin.rs (新文件)

/// Every built-in tool, once. Adding a variant makes the compiler name every
/// place that has to decide something about it — which is the whole point:
/// six of the seven places used to fail silently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Builtin {
    Bash, BashOutput, StopBash, PowerShell,
    ReadFile, WriteFile, EditFile, NotebookEdit,
    Grep, Glob,
    TaskCreate, TaskGet, TaskUpdate, TaskList, TaskClear,
    SendMessage, ListAgents,
    RunAgent, WaitForActivity, StopAgent, StopProgram,
    Skill, ToolSearch, CallTool,
    AskUserQuestion, EnterPlanMode, ExitPlanMode,
    Workflow, StopWorkflow, RunProgram,
    EnterWorktree, ExitWorktree,
    CronCreate, CronDelete, CronList, ScheduleWakeup,
}

impl Builtin {
    pub(crate) fn from_name(name: &str) -> Option<Self>;   // match,穷尽
    pub(crate) fn name(self) -> &'static str;              // match,穷尽
    /// 权限意义的只读。`ByInput` 的两个(bash / exit_worktree)接 &CallFacts。
    pub(crate) fn readonly(self, call: &CallFacts) -> bool;
    /// 并发安全。bash 要看解析结果,所以接 &Value。
    pub(crate) fn concurrency_safe(self, input: &Value) -> bool;
    /// 审批面板的标题;`None` = 用工具自己的名字(和今天的 `other =>` 一致)。
    pub(crate) fn title(self) -> Option<&'static str>;
    /// 它归哪一道 surface 门;`None` = 不受门控。
    pub(crate) fn surface(self) -> Option<SurfaceGate>;
    /// 只在 depth 0 提供?
    pub(crate) fn depth0_only(self) -> bool;
}
```

改造顺序(每步单独可编译、可跑测试):

1. **立起 enum 与 `from_name`/`name`**,加一个测试断言 `from_name(name(v)) == Some(v)` 对
   每个变体成立(用一个显式的 `ALL: &[Builtin]` 常量,并断言它的长度——这是唯一防止
   `ALL` 漏项的办法)。
2. **`is_concurrency_safe` 改成**:先 `Builtin::from_name(name)`,命中就 `b.concurrency_safe(input)`,
   未命中才走 source 分支。原来的名字 `match` 整体搬进 `concurrency_safe`。
3. **`CallFacts::is_readonly` 同样搬**。注意它现在的签名是 `fn is_readonly(&self, name: &str)`,
   搬完之后 `permissions.rs` 只保留"非内置工具一律 false"这一句。
   **跨模块可见性**:`Builtin` 需要对 `permissions` 可见 → `pub(crate)`。
4. **`tool_title` 改成** `Builtin::from_name(name).and_then(Builtin::title).unwrap_or(name)`。
5. **`execute_tool` 改成对 `Builtin` 穷尽 match**(未命中 `from_name` 的先走 source 分支,
   和今天一样)。这一步会让 `run_one` 短一截,但**不在本 plan 拆 `run_one`**(那是 142)。
6. **`builtin_defs` 与 `all_tool_defs` 的 surface 分支**改成遍历 `ALL`,按
   `depth0_only` / `surface()` / shell 门过滤。**这一步最容易改出行为差异**:今天
   `defs` 的**顺序**是手写的,而工具数组顺序影响 provider 请求的字节(prompt cache!)。
   办法:`ALL` 的顺序照抄今天 `builtin_defs` 的顺序,并加一个测试锁住
   `all_tool_defs(0, …)` 的 name 序列与改造前逐字相同(先在改造前跑一次、把序列写进测试)。

## 四、TUI 那一处:收敛判断,不收敛渲染

`toolrow.rs` **不纳入 enum**。它把输入格式化成一行(`Read src/main.rs`、
`Grep TODO in src`),对 MCP 工具也要工作,是货真价实的展示逻辑。

但要给它加一个**守卫测试**:遍历 `Builtin::ALL`,断言每个变体要么有专门的一行形式,要么
出现在一张显式的 `RENDERS_GENERICALLY` 列表里。这样第二节第 3 条那 15 个会被点名,实现者
逐个裁决"补一行"还是"登记为通用渲染",而不是无人知晓。

## 五、非目标

- **不拆 `run_one` / `turn_rounds`**(plan 142)。本 plan 只让 `execute_tool` 的分派表变成
  穷尽 match,长度变化是副产品。
- **不动 `ToolSource` 那一侧**。外部工具的 readonly/并发仍由 source 自己回答,本 plan 只
  改内置工具那一半。
- **不改任何工具的现有行为**。第二节四条不一致里,只有 `run_program` 的并发语义可能要改
  (见下节),其余三条本轮只要求"被点名并写下判断"。

## 六、开工时问用户

1. **`run_program` 的并发语义**:改成与 `workflow` 一致(可并发),还是保持串行并在
   `concurrency_safe` 里写一句为什么?——这是唯一一处可能改变运行时行为的决定。
2. **`toolrow` 那 15 个**:本轮就补展示,还是先全部登记进 `RENDERS_GENERICALLY`、另开一条
   plan 补?(建议后者:本 plan 的价值是让它们**被看见**,补展示是 TUI 的活。)

## 七、验收

- `Builtin::ALL` 的往返测试 + 长度断言;
- `all_tool_defs(0, …)` 与 `all_tool_defs(1, …)` 的 name 序列**逐字**不变(改造前录下);
- 第二节四条不一致各自有归宿:被修,或被一个具名测试/注释写下判断;
- `toolrow` 守卫测试通过;
- `cargo fmt --all --check` / `cargo clippy --workspace --all-targets -- -D warnings` /
  `cargo test --workspace` 三条各自单独跑、当场取退出码(HANDOFF 111(b2))。
