# Plan 112 — 三个编排原语的职责边界调研（只读，不改代码）

> **只读任务**：结论全部来自读代码与既有会话记录，未编译、未跑测试、未改任何实现文件。
> 本文不做决定，只把证据摆清楚。

## 触发

用户在 plan 111 收尾后问："run_program 在编排上，与 workflow 重复了吧"。查下来重叠是真的，
但方向和直觉相反——**有参考实现对应物的是 `workflow`，没有的是 `run_program`**。

## 一、三者的实际能力面（读代码，不读描述）

| | `run_agent` | `run_program` | `workflow` |
|---|---|---|---|
| 引擎 | 无（直接复用 `run_turn`） | QuickJS (`kloop-codemode`) | QuickJS（**同一个 crate**） |
| `tools.*` | — | ✅ 15 个可调内置 + MCP source | **✗ 完全没有** |
| `agent()` | — | ✅ | ✅ |
| `parallel()` / `pipeline()` | — | ✅ | ✅ |
| fs / net / process / imports | ✗ | ✗（只能经 `tools.*` 回灌 gate） | ✗ |
| `Date` / 随机 | — | 可用 | **✗ 禁用** |
| 前台 / 后台 | 都可 | 都可 | **恒后台** |
| 恢复 | — | `run-*` journal v3 | `wf_*`，**且允许改脚本重跑** |
| 其他 | — | — | 必须有 `meta`，`phase()` 进度 |

`run_program` 的 `tools` 对象排除表在 `codemode.rs:100 is_program_callable`：排掉自身、
`workflow`、`run_agent`、所有 `stop_*`、`wait_for_activity`、`bash_output`、`send_message`、
`list_agents` 和 `task_*`。**剩 15 个可调内置工具。**

## 二、重叠的精确切片

**"只派 agent、不调 tool 的扇出"**——这一块两边写法逐字相同（`agent()` + `parallel()` +
`pipeline()`），因为它们**就是同一份实现**：

```
crates/core/src/tools/workflow.rs:19  use super::codemode::journal::Claim;
crates/core/src/tools/workflow.rs:20  use super::codemode::journal::Journal;
crates/core/src/tools/workflow.rs:46  use kloop_codemode::BoxFuture;
crates/core/src/tools/workflow.rs:47  use kloop_codemode::HostBridge;
```

`workflow` 复用 `kloop-codemode` 引擎、`HostBridge` 缝和 `codemode::journal`。并发上限也共用
（plan 68：live agent 16 路、总量 1000、单 helper 4096）。

**所以 `workflow` 相对 `run_program` 的差异全是"减法 + 策略"**：去掉 `tools`、去掉不确定性源、
强制后台、强制 `meta`、外加一条"只在用户明确要求时用"的使用策略。
**减法通常该是参数或策略，而不是一个带独立名字、独立描述、独立 id 空间（`workflow-N` + `wf_*`）
和独立 `stop_workflow` 的第二个工具。** 这是用户直觉命中的地方。

## 三、参考实现对照（`refs/claude-code-2.1.220/tool-matrix.json`）

```
Agent      ↔ run_agent
Workflow   ↔ workflow          ← cc 有
None       ↔ run_program       ← cc 没有对应物
```

- **cc**：有 `Agent` + `Workflow` 管 agent 编排；**没有**"用代码批量调工具"这个原语。它的
  `Bash` 就承担这件事（`python3 -c` 就地处理，见 plan 111）。cc 的 Workflow 描述与 kloop 的
  同型：确定性多 agent、后台、返回 task ID、**"ONLY call this tool when the user has explicitly
  opted into multi-agent orchestration"**——kloop 那句策略是照抄的。
- **codex**：唯一工具就是 JS runtime 里的 `exec_command`。**它整个 agent 就是 run_program**，
  所以无从重复；也没有独立 workflow。

## 四、每次请求的固定成本

| 工具 | 描述字符 | ≈ token |
|---|---|---|
| `run_agent` | 1385 | 346 |
| `workflow` | 1827 | 456 |
| `stop_workflow` | 223 | 55 |
| `run_program` | 静态骨架 6972 + 生成的 TS 声明（15 个工具） | **≈ 2300** |

`run_program` 的描述是动态生成的（`codemode.rs:767 run_program_def`），内嵌 `declare const
tools: {...}` 的完整 TypeScript 声明。**它是三者里最贵的一个，且贵一个量级**——每一次采样都付。

## 五、实测用量（本机全部会话）

```
bash          1775      run_agent    31
read_file     1660      run_program  25
grep           674      workflow      0
```

按 plan 110 引导上线切开：

| | 会话 | bash | run_program |
|---|---|---|---|
| 引导之前 | 45 | 1666 | **2** |
| 引导之后 | 18 | 110 | 23 |

**混杂因素，必须记明：**

1. 引导之后那 23 次 `run_program` **几乎全部**来自 plan 110/111 的定向验收轮（"抓大 JSON 再抽
   字段"），是我为了测引导专门制造的场景，不是自然使用。
2. 同期的真实工作会话（`20260903-0317` × 4）是 **100 次 bash、0 次 run_program**。
3. **`workflow` 的 0 次不能解读为冗余**：它的描述写死"只在用户明确要求多 agent 编排时用"，
   且 `startup.rs:1004` 是 `workflow: !args.headless`——headless 根本不暴露。0 是符合设计预期的。

`run_program` 那 25 次的程序体在调什么：

```
tools.web_fetch      15/25
tools.bash            5/25
tools.read_offloaded  4/25
tools.read_file       1/25
```

**多数是单个工具的薄包装，一次都没用上"混合 tool 调用与 agent 扇出"这个独有能力。**

## 六、由此得到的三条待决问题（不做决定）

**Q1. `workflow` 该不该退化成 `run_program` 的一个模式？**
支持：实现已经共用；差异全是减法 + 策略；省下 456+55 token/请求与一套 id 空间。
反对：cc 也是两个独立工具，且"独立工具"本身就是那条使用策略最容易表达和审计的形式；
0 用量不构成证据。

**Q2. `run_program` 的三块价值是否都还站得住？**
- **抽取大工件** → plan 111 实测里成绩最好的一次（2 轮）用的是 `bash` + `python3 -c`，
  不是 `run_program`；那也正是 cc 的做法。**这块已被证伪为"更好由 bash 承担"。**
- **agent 扇出** → 与 `workflow` 重叠（第二节）。
- **混合 tool + agent 编排、且可前台** → 真正独有，但 25 次里 0 次用到。

**Q3. 如果要收敛工具面，先动谁？**
按证据，`run_program` 比 `workflow` 更像该被质疑的那个：它**没有参考实现对应物**（与被删的
`read_offloaded` 同形），它最常被用的那件事 `bash` 做得更好，它独有的那件事无人使用，而它是
三者里每次请求最贵的（≈2300 token）。

## 七、明确不做 / 未验证

- 不改代码。plan 24（run_program 作为内置工具）与 plan 66（三原语收口）都是拍过板的架构决定，
  本文的证据强度不足以翻案：用量样本单模型、单机器，且 `workflow` 的 0 用量按设计不可解读。
- **未验证**"把 run_program 收成 workflow 的一个模式"或反向合并的可行性——没有读
  `SurfaceCapabilities`、server 协议面与 journal v3 的兼容影响。
- **未测**去掉 `run_program` 后模型是否会自然回落到 `bash`（第五节暗示会，但那是观察不是实验）。
- 本文引用的"引导有效"结论来自 plan 110/111，n=2~3、单模型、单场景，不可外推。
