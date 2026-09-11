# Plan 140 — 那张字段表,抄了七遍

> 来源:plan 136 的全仓通读,第五节非目标里的两条(二.8 测试 Config fixture / 三.4 Config
> 的两个克隆构造)。2026-09-11 用户要求把 136 的非目标排成计划;本 plan 是其中一条,与
> 138、139、141、142 同批,可独立开工。**读完即可动手。**

两件事同属一个形状:`Config` 有 32 个字段,而它的字段表在仓库里被手抄了七遍。

## 一、五份测试 fixture

```
crates/core/src/compact.rs:626
crates/core/src/commands/mod.rs:316
crates/core/src/tools/mod.rs:1726
crates/core/src/tools/mod.rs:3664
crates/core/src/rollout.rs:2199
```
(行号是 `powershell_execution_gate: Default::default()` 那一行;每处上下各二三十行都是同一
张字段表。)

`crates/core/src/tools/mod.rs` 里已经有 `pub(crate) mod testutil`——注释写着"Shared fixtures
for the per-module tool tests: a permissive ToolCtx and a dispatch-path runner, so every tool
test exercises the real gate"。**它就是这份 fixture 该住的地方**,只是当初只收了 ToolCtx,
没收 Config 本身。

**做法**:`testutil` 里立一个 builder:

```rust
/// The one test Config. Fields that a test actually cares about are set through
/// the builder; everything else gets the same permissive default it has been
/// getting in five hand-copied literals.
pub(crate) struct TestConfig { … }
impl TestConfig {
    pub(crate) fn new(tag: &str) -> Self;          // offload/sessions 目录按 tag 分开
    pub(crate) fn provider(self, p: Provider) -> Self;
    pub(crate) fn context_window(self, w: Option<u64>) -> Self;
    pub(crate) fn max_rounds(self, r: Option<usize>) -> Self;
    pub(crate) fn tool_sources(self, s: Vec<Arc<dyn ToolSource>>) -> Self;
    pub(crate) fn surface(self, s: SurfaceCapabilities) -> Self;
    pub(crate) fn build(self) -> Arc<Config>;
}
```

**逐处搬,不要一次全换**:五处的默认值并不完全一样(例如 `compact.rs` 的
`max_rounds: Some(5)`、`context_window: Some(200_000)`,`tools/mod.rs` 的两处各有自己的
`tool_sources`)。搬一处、跑一次该模块的测试、再搬下一处;哪一处的差异 builder 表达不了,
就给 builder 加一个方法,**不要**在调用点改测试的语义。

`testutil` 是 `#[cfg(test)]` 的,而 `compact.rs` / `rollout.rs` / `commands/mod.rs` 的测试
都在 core 同一个 crate 内,`use crate::tools::testutil::…` 可达。

## 二、两份生产字段表

`crates/core/src/config.rs` 的 `subagent_from` 与 `clone_with_provider_route` 各罗列 32 个
字段。**漏写会编译报错**,所以这不是正确性问题,是两段三十行的噪音——而且每次给 Config
加字段都要在两处各写一遍,还要想清楚"子 agent 该继承还是重置"。

**做法**:给 `Config` 手写一个 `Clone`(不能 derive:`questioner`/`tool_sources` 是
`Arc<dyn …>`,`local_agent` 有自己的语义,但它们都 Clone,所以手写体就是全字段 clone),
然后:

```rust
pub fn clone_with_provider_route(&self, provider_route: FrozenProviderRoute) -> Self {
    Self { provider_route, ..self.clone() }
}

pub(crate) fn subagent_from(&self, workspace: &EffectiveWorkspace, max_rounds, agent_id) -> Self {
    Self {
        // 只列**与父不同**的那些 —— 这正是 sub-agent 语义的清单,
        // 而它现在淹没在 24 个 `Arc::clone(&self.x)` 里。
        provider_route: self.provider_route.child_route(None).expect(…),
        system: workspace.system.clone(),
        cwd: workspace.cwd.clone(),
        max_rounds,
        permissions: Arc::clone(&workspace.permissions),
        questioner: None,
        file_state: Arc::new(FileState::default()),
        session_id: …,
        local_agent: self.local_agent.child(agent_id),
        sandbox: workspace.sandbox.clone(),
        unlocked_tools: Arc::new(DeferredToolUnlocks::default()),
        inbox: Arc::new(Inbox::default()),
        active_worktree: Arc::new(ActiveWorktreeState::default()),
        surface: SurfaceCapabilities::default(),
        ..self.clone()
    }
}
```

改完 `subagent_from` 的函数体本身**就是**「一个子 agent 与父的差别」这份清单,13 行而不是
32 行。

**代价要认**:`..self.clone()` 之后,新加的字段默认**被子 agent 继承**;今天的写法则强制
作者当场选择。所以 `subagent_from` 上方要补一句注释说明这个默认,并在
`crates/core/src/agent/tests.rs` 里加/改一个测试,断言子 agent 的那 13 项确实与父不同
(尤其 `inbox`、`file_state`、`unlocked_tools`、`active_worktree` 这四个必须是**新实例**
——它们是"子不得污染父"的载体,`Arc::ptr_eq` 断言)。

## 三、非目标

- 不给 `Config` derive `Clone` 之外的任何新 trait,不改任何字段的类型或语义。
- 不碰 `EffectiveWorkspace`(它已经是 `#[derive(Clone)]`)。
- 不处理 plan 136 已经做掉的 `effective_*`(那五个便捷取值已经不再克隆整个 workspace)。

## 四、开工时问用户

`subagent_from` 改成 `..self.clone()` 之后,"新字段默认继承"这个默认值可以接受吗?
——另一条路是保留全字段罗列、只给它加一个测试锁住那 13 项差异。**建议前者**:差异清单
显式、共同项隐式,比 32 行里挑出 13 行更容易读对;但这是一个会影响以后每次加字段的取舍。

## 五、验收

- 五处 fixture 收成一处,`grep -c 'powershell_execution_gate: Default::default()'` 从 5 变 1;
- `config.rs` 两个构造合计从约 60 行降到约 20;
- 子 agent 的四个"必须是新实例"字段有 `Arc::ptr_eq` 断言;
- 全量测试逐条不变地通过(fixture 搬家不该改任何测试的断言);
- fmt / clippy(`-D warnings`) / `cargo test --workspace` 各自单独跑、当场取退出码。
