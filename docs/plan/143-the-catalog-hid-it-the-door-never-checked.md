# Plan 143 — 目录藏起来了,门上没人查

> 来源:plan 142 收尾时发现(把 `run_one` 的名字判断抽成 `reject_unavailable` 时,发现它
> 和 plan 138 的 `Builtin::gate()` 在说同一件事、成员却不一样)。142 第五节写着"看到 bug
> 记下来另开 plan,别在重构提交里混修复",所以单开这条。**读完即可动手**,规模小。
>
> 2026-09-14 用户拍板:开这条 plan。

## 一、症状:同一条规则写了两遍,一遍漏了三个

"哪些工具只给主 agent、不给子 agent",现在有两处独立写着。

**第一处,目录表** —— `crates/core/src/tools/builtin.rs` 的 `Builtin::gate()`,
`Gate::Depth0` 有 8 个成员:

```rust
// The session task graph is root-owned even though child Configs
// retain the same Arc.
Self::TaskCreate | Self::TaskGet | Self::TaskUpdate | Self::TaskList
    | Self::TaskClear => Gate::Depth0,
...
// Sub-agents cannot spawn further sub-agents, so the whole
// background-agent surface is root-only.
Self::RunAgent | Self::WaitForActivity | Self::StopAgent => Gate::Depth0,
```

`in_catalog`(`builtin.rs:318`)把 `Gate::Depth0 => depth == 0`,所以 depth ≥ 1 时这 8 个
**不进工具数组**。

**第二处,执行前的兜底** —— `crates/core/src/tools/mod.rs` 的 `reject_unavailable`,只查
**5 个** `task_*`:

```rust
// The catalog hides Task tools from child Agents, but stale context or a
// forged call must fail before allowlists, hooks, permissions, or the
// registry handler can observe it.
if ctx.depth > 0 && is_root_task_tool(name) {
    bail!("tool '{name}' is only available to the root agent");
}
```

那条注释自己说明了兜底为什么必须存在:**目录里没有 ≠ 调不到**。压缩前那一轮子 agent 还在
depth 0、伪造的调用、`call_tool` 转发,都不经过目录。而兜底漏了
`run_agent` / `wait_for_activity` / `stop_agent`。

## 二、漏掉的三个里,两个是真的

开工前已核实过(plan 142 会话),不必重查:

- **`run_agent` 没事。** 它在执行器里自己挡了 —— `tools/subagent.rs:103`
  `if ctx.depth >= 1 { bail!("run_agent: sub-agents cannot spawn further sub-agents"); }`,
  测试 `subagent.rs:1170` `run_agent_is_refused_at_depth_one` 锁着。
- **`stop_agent` 能造成真实影响。** `stop_agent_tool` → `stop_tool`
  (`background_executions.rs:666`)直接 `ctx.cfg.background_executions.request_stop(kind, id)`,
  全程没有 depth,也没有 owner/parent 检查 —— `request_stop`(`background_executions.rs:473`)
  只校验 id 的**形状**和 kind。而 `background_executions` 是父子**共享**的 Arc
  (`config.rs:784` 有 `Arc::ptr_eq` 断言锁着)。于是一个子 agent 只要拿到或猜到 `agent-N`,
  就能取消 root 持有的后台 agent / program / workflow。
  `Builtin::readonly()` 还把它判成只读,注释写的是"停止只给自己拥有的取消令牌发信号" ——
  这个前提在 depth > 0 上不成立,因为令牌表是共享的,所以权限门也不会弹确认。
- **`wait_for_activity` 会空转到超时。** `inbox` 恰恰是父子**不**共享的(`config.rs:517`
  每个子 agent 一个新的,`config.rs:712` 断言 `!Arc::ptr_eq`)。子 agent 数的是共享注册表里
  的活,等的是自己那个永远空的 inbox:root 那边完成了也不会碰它。默认睡 30 秒,而
  `timeout_ms` 由模型自己填,上限一小时(`background_executions.rs:39`)。

**现有测试的缺口:全仓没有任何一处在 depth > 0 上调过这两个工具。** 目录形状的测试
`mod.rs:2447` `tool_defs_expose_root_controls_only_at_depth_zero` 查的是 `run_agent` 和五个
`task_*`,没查这两个。

## 三、做法:兜底那道改成读同一张表

`reject_unavailable` 里那行换成读 `Builtin::gate()`,`is_root_task_tool`
(`mod.rs:768`)跟着删掉 —— 两张表合成一张,这正是 plan 138 立下的形状。

```rust
if ctx.depth > 0
    && Builtin::from_name(name).is_some_and(|b| matches!(b.gate(), builtin::Gate::Depth0))
{
    bail!("tool '{name}' is only available to the root agent");
}
```

`Gate` 已经 `derive(PartialEq, Eq)`,`builtin::Gate` 在 `mod.rs` 里已经能直接写
(plan 142 的 `reject_unavailable` 已经在用 `builtin::Gate::Shell`)。

**`subagent.rs:103` 那道 depth 守卫要保留,它不是死代码。** `run_agent_admitted` 有两个
**不经过 `run_one`** 的调用者:`codemode.rs:735`(Program 的 `agent()` API)和
`workflow.rs:514`。同理 `structured_agent_admitted`(`subagent.rs:239`)与 `fork_skill`
(`subagent.rs:670`)各自的守卫也保留。新加的这道管的是**模型发出的工具调用**那条路。

### 会动到的一条既有测试

`run_tool`(`mod.rs:1762`)走的是 `dispatch_tools` → `run_one` → `reject_unavailable`,所以
`run_agent_is_refused_at_depth_one` 拿到的错误信息会从 `"cannot spawn"` 变成统一的
`"tool 'run_agent' is only available to the root agent"`,**那条断言要改**。这是行为修复不是
重构,改测试断言是应该的 —— 但要在提交信息里点名,别让它看起来像 142 那批"测试一字不动"
的延续。

## 四、要补的测试

1. `stop_agent` 在 depth 1 被拒(现在能取消 root 的后台执行);
2. `wait_for_activity` 在 depth 1 被拒(现在会空转到超时);
3. 两条都要走 `run_tool`,即真实 dispatch 路径,而不是直接调执行器;
4. `tool_defs_expose_root_controls_only_at_depth_zero`(`mod.rs:2447`)补上这两个名字 ——
   目录那一侧本来就对,但清单漏列了它俩,正是这次漏检的同一个形状;
5. 加一条"目录与兜底同源"的守卫测试:遍历 `builtin::ALL`,凡 `gate() == Depth0` 的,
   depth 1 调用必须报错。这条是防复发的关键 —— 有它,下次再往 `Depth0` 加成员时,兜底
   忘了跟就会当场变红。

## 五、开工时问用户

**`Gate::Surface(_)` 要不要一起管?** 同一个形状:surface 工具(`ask_user_question` /
`cron_*` / `enter_plan_mode` / `exit_plan_mode` / `workflow` / `run_program` /
`enter_worktree` / `exit_worktree`)也只在 **depth 0** 追加(`all_tool_defs` 里那段注释写着
"The surface-gated block is depth-0 only (like run_agent)"),兜底同样不查。按这批"不留豁免
名单"的纪律,只修 Depth0、明知留着 Surface,就是在配一张豁免清单。

但一起管要多想两件事,所以先问:
- 判据要从 `gate()` 换成一个"这次请求到底提供了没有"的谓词(depth + `cfg.surface` +
  `shell_programs`),`Gate::Elsewhere`(`tool_search` / `call_tool` / `skill`)有各自的
  条件,不能一刀切;
- surface 是**前端能力**不是**权限**,一个 surface 关着的前端里子 agent 喊 `cron_create`,
  报错措辞该是"这个前端没开"还是"只给主 agent",要定。

**建议**:本条先只做 Depth0(小、确定、有真 bug),Surface 单开一条,由那条去设计
`Builtin::offered(depth, surface, shell_programs)` 这个单一谓词并让目录构建与执行兜底共用。
开工时把这个建议交给用户定。

## 六、非目标

- **不改 `readonly()` 的分类。** `stop_agent` 在 depth 0 判只读是对的(它只给自己的令牌
  发信号);depth > 0 的问题由这次的门解决,不该拿权限分类去补。
- **不给 `request_stop` 加 owner 检查。** 那是另一个设计问题(执行 id 该不该带归属),
  而且这次的门堵上之后它没有已知的触发路径。想做单开 plan。
- **不碰 `inbox` 父子不共享这个决定。** `wait_for_activity` 在 depth > 0 空转是"它根本不该
  被调到"的症状,不是 inbox 分家的错。

## 七、验收

- `reject_unavailable` 里不再有 `is_root_task_tool`,该函数已删;
- 第四节 5 条测试齐全,其中第 5 条(遍历 `ALL` 的守卫)必须在**故意把某个 `Depth0` 成员
  从兜底漏掉**时变红 —— 写完当场验一次再改回来;
- `run_agent_is_refused_at_depth_one` 的断言已更新,提交信息里点名说明为什么;
- fmt / clippy(`-D warnings`) / `cargo test --workspace` 各自单独跑、当场取退出码;
- 行为变更同步 README(若 README 有提到子 agent 可用工具面的话;没提就不用)。
